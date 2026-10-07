//! OCI hooks (stage 5, step 6): programs a bundle names, run at fixed points
//! of a container's life, as the OCI runtime spec's `hooks` describes them.
//! GPU toolkits use them to hand a container its devices, network setups to
//! wire up its namespace, cleanups to undo both.
//!
//! | Phase | When | Run by |
//! |---|---|---|
//! | `prestart`, `createRuntime` | create: the mounts are made, the root not yet switched | the runtime |
//! | `createContainer` | right after, still before `pivot_root` | the container's init |
//! | `startContainer` | start, before the command | the container's init |
//! | `poststart` | start, once the command was let go | the runtime |
//! | `poststop` | delete, once the container is gone | the runtime |
//!
//! Each hook is an ordinary process (`fork` + `exec`, TLPI ch. 24–27): its
//! own arguments and environment, exactly as listed (nothing inherited), and
//! the container's state, as `state` prints it, on its standard input. A hook
//! with a `timeout` is killed (SIGKILL) once that many seconds have passed.
//! There's no default timeout, as in the spec and runc: a hook that hangs
//! hangs its phase.
//!
//! A hook is code the bundle runs with the runtime's privileges, root's for
//! root: only run bundles you trust.

use std::collections::BTreeMap;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use serde::{Deserialize, Serialize};

use crate::container::{OCI_VERSION, State, Status};
use nix::sys::signal::Signal;

use crate::process::{ExitStatus, is_pending};

/// How often a hook with a timeout is checked on.
const POLL: Duration = Duration::from_millis(10);

/// One hook, as the bundle's `config.json` gives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Hook {
    /// The program, an absolute path (where it's run: the host's, except
    /// for `startContainer`, which runs inside the container's root).
    pub path: PathBuf,
    /// Its arguments, as `execv` takes them: the first is its `argv[0]`.
    /// None: just the path.
    #[serde(default)]
    pub args: Vec<String>,
    /// Its whole environment, `KEY=VALUE` each.
    #[serde(default)]
    pub env: Vec<String>,
    /// Seconds it may take before it's killed and fails; none: no limit.
    #[serde(default)]
    pub timeout: Option<u32>,
}

impl Hook {
    /// A hook that runs `path`, with no arguments, environment or timeout.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            args: Vec::new(),
            env: Vec::new(),
            timeout: None,
        }
    }

    /// Why it can't run as given, if it can't.
    fn check(&self) -> Result<(), String> {
        if !self.path.is_absolute() {
            return Err(format!("{} isn't an absolute path", self.path.display()));
        }
        if self.timeout == Some(0) {
            return Err("a timeout must be above 0".into());
        }
        match self.env.iter().find(|entry| !entry.contains('=')) {
            Some(entry) => Err(format!("{entry:?} in env isn't KEY=VALUE")),
            None => Ok(()),
        }
    }
}

/// A container's hooks, phase by phase, in the order each phase runs them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Hooks {
    /// The spec's deprecated first phase: run just before `createRuntime`.
    #[serde(default)]
    pub prestart: Vec<Hook>,
    /// See [`Phase::CreateRuntime`].
    #[serde(default)]
    pub create_runtime: Vec<Hook>,
    /// See [`Phase::CreateContainer`].
    #[serde(default)]
    pub create_container: Vec<Hook>,
    /// See [`Phase::StartContainer`].
    #[serde(default)]
    pub start_container: Vec<Hook>,
    /// See [`Phase::Poststart`].
    #[serde(default)]
    pub poststart: Vec<Hook>,
    /// See [`Phase::Poststop`].
    #[serde(default)]
    pub poststop: Vec<Hook>,
}

impl Hooks {
    /// The hooks of `phase`.
    #[must_use]
    pub fn of(&self, phase: Phase) -> &[Hook] {
        match phase {
            Phase::Prestart => &self.prestart,
            Phase::CreateRuntime => &self.create_runtime,
            Phase::CreateContainer => &self.create_container,
            Phase::StartContainer => &self.start_container,
            Phase::Poststart => &self.poststart,
            Phase::Poststop => &self.poststop,
        }
    }

    /// Whether there are none at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        Phase::ALL.iter().all(|&phase| self.of(phase).is_empty())
    }

    /// Whether any runs while the container is being set up (up to
    /// `startContainer`): then its setup has to stop for them.
    #[must_use]
    pub fn during_setup(&self) -> bool {
        Phase::ALL
            .iter()
            .filter(|phase| phase.during_setup())
            .any(|&phase| !self.of(phase).is_empty())
    }

    /// Why one of them can't run as given (`hooks.PHASE[N]: why`), if one
    /// can't.
    ///
    /// # Errors
    ///
    /// A relative path, a timeout of 0, or an environment entry without `=`.
    pub fn check(&self) -> Result<(), String> {
        for phase in Phase::ALL {
            for (index, hook) in self.of(phase).iter().enumerate() {
                hook.check()
                    .map_err(|why| format!("hooks.{phase}[{index}]: {why}"))?;
            }
        }
        Ok(())
    }
}

/// When a hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Phase {
    /// Deprecated in the spec, kept for the tools that still use it: run by
    /// the runtime with `createRuntime`, before it.
    Prestart,
    /// The container's namespaces and mounts exist, its root isn't switched
    /// yet: run by the runtime, in the host's namespaces.
    CreateRuntime,
    /// Right after `createRuntime`: run by the container's init, in the
    /// container's namespaces, still before `pivot_root` (its path is the
    /// host's).
    CreateContainer,
    /// At start, before the command: run by the container's init, inside
    /// the container's root.
    StartContainer,
    /// Once the command was let go: run by the runtime. A failure is only a
    /// warning.
    Poststart,
    /// Once the container is deleted: run by the runtime. A failure is only
    /// a warning.
    Poststop,
}

impl Phase {
    /// Every phase, in the order they come.
    pub const ALL: [Self; 6] = [
        Self::Prestart,
        Self::CreateRuntime,
        Self::CreateContainer,
        Self::StartContainer,
        Self::Poststart,
        Self::Poststop,
    ];

    /// Its name in `config.json`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Prestart => "prestart",
            Self::CreateRuntime => "createRuntime",
            Self::CreateContainer => "createContainer",
            Self::StartContainer => "startContainer",
            Self::Poststart => "poststart",
            Self::Poststop => "poststop",
        }
    }

    /// The status the state its hooks get says.
    #[must_use]
    pub fn status(self) -> Status {
        match self {
            Self::Prestart | Self::CreateRuntime | Self::CreateContainer => Status::Creating,
            Self::StartContainer => Status::Created,
            Self::Poststart => Status::Running,
            Self::Poststop => Status::Stopped,
        }
    }

    /// Whether it runs while the container is set up, before its command:
    /// a failure then means there's no container.
    #[must_use]
    pub fn during_setup(self) -> bool {
        !matches!(self, Self::Poststart | Self::Poststop)
    }
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A container's hooks, with what their state says about the container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ContainerHooks {
    /// The hooks.
    pub hooks: Hooks,
    /// The container's ID.
    pub id: String,
    /// Its bundle (absolute).
    pub bundle: PathBuf,
    /// Its config's annotations.
    pub annotations: BTreeMap<String, String>,
}

impl ContainerHooks {
    /// The `hooks` of container `id`, from the bundle at `bundle`.
    #[must_use]
    pub fn new(
        hooks: Hooks,
        id: impl Into<String>,
        bundle: impl Into<PathBuf>,
        annotations: BTreeMap<String, String>,
    ) -> Self {
        Self {
            hooks,
            id: id.into(),
            bundle: bundle.into(),
            annotations,
        }
    }

    /// The container's state, as `phase`'s hooks get it, with its init's
    /// host PID `pid` (none once it's stopped).
    #[must_use]
    pub fn state(&self, phase: Phase, pid: Option<i32>) -> State {
        State {
            oci_version: OCI_VERSION.to_owned(),
            id: self.id.clone(),
            status: phase.status(),
            pid: pid.filter(|_| phase.status() != Status::Stopped),
            bundle: self.bundle.clone(),
            annotations: self.annotations.clone(),
        }
    }

    /// Runs `phase`'s hooks, in order, stopping at the first that fails.
    ///
    /// # Errors
    ///
    /// The hook that failed, and how.
    pub fn run(&self, phase: Phase, pid: Option<i32>) -> Result<(), HookError> {
        run(phase, self.hooks.of(phase), &self.state(phase, pid))
    }

    /// Runs every one of `phase`'s hooks, whatever the others do (the spec's
    /// rule for `poststart` and `poststop`), and returns each failure as a
    /// warning to show.
    #[must_use]
    pub fn run_all(&self, phase: Phase, pid: Option<i32>) -> Vec<String> {
        let state = self.state(phase, pid);
        let json = state_json(&state);
        let dir = working_dir(phase, &state);
        self.hooks
            .of(phase)
            .iter()
            .enumerate()
            .filter_map(|(index, hook)| run_one(phase, index, hook, &json, dir).err())
            .map(|err| err.to_string())
            .collect()
    }
}

/// Runs `hooks`, `phase`'s, in order, each with `state` on its standard
/// input, stopping at the first that fails.
///
/// # Errors
///
/// The hook that failed, and how.
pub fn run(phase: Phase, hooks: &[Hook], state: &State) -> Result<(), HookError> {
    let json = state_json(state);
    let dir = working_dir(phase, state);
    for (index, hook) in hooks.iter().enumerate() {
        run_one(phase, index, hook, &json, dir)?;
    }
    Ok(())
}

/// Where `phase`'s hooks run: in the bundle, as runc has them, so that a
/// relative path among their arguments means the bundle's files. Inside the
/// container (`startContainer`), and once the bundle is gone (a `poststop`
/// after it was removed), at `/`.
fn working_dir(phase: Phase, state: &State) -> &Path {
    if phase != Phase::StartContainer && state.bundle.is_dir() {
        &state.bundle
    } else {
        Path::new("/")
    }
}

/// The state as JSON. Nothing in it fails to encode (strings and numbers).
pub(crate) fn state_json(state: &State) -> Vec<u8> {
    serde_json::to_vec(state).unwrap_or_default()
}

/// A hook that failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the {phase} hook {} (hooks.{phase}[{index}]) {failure}", path.display())]
#[non_exhaustive]
pub struct HookError {
    /// Its phase.
    pub phase: Phase,
    /// Its place in the phase's list, from 0.
    pub index: usize,
    /// Its program.
    pub path: PathBuf,
    /// What went wrong.
    pub failure: HookFailure,
}

/// How a hook failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HookFailure {
    /// It couldn't be started (`exec` failed).
    #[error("couldn't be started: {}", .0.desc())]
    Start(Errno),
    /// It started, but couldn't be waited for (`waitpid` failed: it's lost).
    #[error("couldn't be waited for: {}", .0.desc())]
    Wait(Errno),
    /// It ended, but not with 0.
    #[error("{0}")]
    Ended(ExitStatus),
    /// It ran past its timeout, and was killed.
    #[error("didn't finish within its timeout ({0} s), and was killed")]
    TimedOut(u32),
}

// How a failure travels from the container's init (`createContainer`,
// `startContainer`) as a report record's errno and detail: the detail's
// bytes, little-endian, are the hook's index (2), the kind (2), and the raw
// wait status or the timeout (4).
const KIND_START: u16 = 1;
const KIND_ENDED: u16 = 2;
const KIND_TIMED_OUT: u16 = 3;
const KIND_WAIT: u16 = 4;

impl HookError {
    /// The failure as a report record's errno and detail.
    #[cfg_attr(
        not(target_os = "linux"),
        allow(dead_code, reason = "only a container's init sends one")
    )]
    pub(crate) fn to_record(&self) -> (Errno, u64) {
        let (errno, kind, value) = match self.failure {
            HookFailure::Start(errno) => (errno, KIND_START, 0),
            HookFailure::Wait(errno) => (errno, KIND_WAIT, 0),
            HookFailure::Ended(status) => (
                Errno::UnknownErrno,
                KIND_ENDED,
                status.wait_status().cast_unsigned(),
            ),
            HookFailure::TimedOut(seconds) => (Errno::ETIMEDOUT, KIND_TIMED_OUT, seconds),
        };
        // An index past 16 bits names no hook: `from_record` says so.
        let index = u16::try_from(self.index).unwrap_or(u16::MAX);
        let [i0, i1] = index.to_le_bytes();
        let [k0, k1] = kind.to_le_bytes();
        let [v0, v1, v2, v3] = value.to_le_bytes();
        (errno, u64::from_le_bytes([i0, i1, k0, k1, v0, v1, v2, v3]))
    }

    /// The failure a report record says, for `phase`'s `hooks`; `None` if
    /// the record doesn't make sense.
    pub(crate) fn from_record(
        phase: Phase,
        hooks: &[Hook],
        errno: Errno,
        detail: u64,
    ) -> Option<Self> {
        let [i0, i1, k0, k1, v0, v1, v2, v3] = detail.to_le_bytes();
        let index = usize::from(u16::from_le_bytes([i0, i1]));
        let value = u32::from_le_bytes([v0, v1, v2, v3]);
        let failure = match u16::from_le_bytes([k0, k1]) {
            KIND_START => HookFailure::Start(errno),
            KIND_WAIT => HookFailure::Wait(errno),
            KIND_ENDED => HookFailure::Ended(ExitStatus::from_wait_status(value.cast_signed())?),
            KIND_TIMED_OUT => HookFailure::TimedOut(value),
            _ => return None,
        };
        Some(Self {
            phase,
            index,
            path: hooks.get(index)?.path.clone(),
            failure,
        })
    }
}

/// Runs `hook`, number `index` of `phase`'s, in `dir`, with `state` (JSON)
/// on its standard input, and waits for it, up to its timeout.
fn run_one(
    phase: Phase,
    index: usize,
    hook: &Hook,
    state: &[u8],
    dir: &Path,
) -> Result<(), HookError> {
    let fail = |failure| HookError {
        phase,
        index,
        path: hook.path.clone(),
        failure,
    };
    let mut command = Command::new(&hook.path);
    if let Some((first, rest)) = hook.args.split_first() {
        command.arg0(first).args(rest);
    }
    command
        .current_dir(dir)
        .env_clear()
        .envs(hook.env.iter().filter_map(|entry| entry.split_once('=')));
    // Its stdout and stderr are the caller's: the runtime's, or the
    // container's for the init's.
    command.stdin(Stdio::piped());
    let started = Instant::now();
    let mut child = command
        .spawn()
        .map_err(|err| fail(HookFailure::Start(errno_of(&err))))?;
    let timeout = hook.timeout.map(|seconds| Timeout {
        seconds,
        deadline: started + Duration::from_secs(seconds.into()),
    });
    if let Some(stdin) = child.stdin.take() {
        // Closed (dropped) once written: a hook reading to end-of-file goes
        // on.
        write_state(&OwnedFd::from(stdin), state, timeout.map(|t| t.deadline));
    }
    wait(&mut child, timeout)
        .map_err(fail)
        .inspect_err(|err| tracing::debug!(%err, elapsed = ?started.elapsed(), "a hook failed"))
}

/// A hook's `timeout`, in seconds, and when it runs out.
#[derive(Clone, Copy)]
struct Timeout {
    seconds: u32,
    deadline: Instant,
}

/// Waits for the hook `child`, up to `timeout`, and says how it went: past
/// the timeout, it's killed and reaped first.
fn wait(child: &mut std::process::Child, timeout: Option<Timeout>) -> Result<(), HookFailure> {
    let lost = |err: std::io::Error| HookFailure::Wait(errno_of(&err));
    let status = match timeout {
        None => child.wait().map_err(lost)?,
        Some(Timeout { seconds, deadline }) => loop {
            if let Some(status) = child.try_wait().map_err(lost)? {
                break status;
            }
            let now = Instant::now();
            if now >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HookFailure::TimedOut(seconds));
            }
            std::thread::sleep(POLL.min(deadline - now));
        },
    };
    if status.success() {
        return Ok(());
    }
    // Neither exited nor killed can't happen for a waited-for child.
    ExitStatus::from_wait_status(status.into_raw())
        .map_or(Err(HookFailure::Wait(Errno::ECHILD)), |ended| {
            Err(HookFailure::Ended(ended))
        })
}

/// The errno behind an I/O error (EIO if there's none).
fn errno_of(err: &std::io::Error) -> Errno {
    err.raw_os_error().map_or(Errno::EIO, Errno::from_raw)
}

/// Writes `state` into the hook's standard input (`stdin`, the pipe's write
/// end), giving up at `deadline`. A hook needn't read it: one that closes
/// its end (EPIPE) or never reads a large state (the pipe stays full) is
/// left to finish without it.
fn write_state(stdin: &OwnedFd, state: &[u8], deadline: Option<Instant>) {
    // Non-blocking, so a full pipe can't hold this past the deadline.
    let nonblocking = fcntl(stdin, FcntlArg::F_GETFL)
        .map(|flags| OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK)
        .and_then(|flags| fcntl(stdin, FcntlArg::F_SETFL(flags)));
    if nonblocking.is_err() {
        return;
    }
    let sigpipe_was_pending = is_pending(Signal::SIGPIPE);
    let mut rest = state;
    while !rest.is_empty() {
        match nix::unistd::write(stdin, rest) {
            Ok(written) => rest = rest.get(written..).unwrap_or_default(),
            Err(Errno::EINTR) => {}
            Err(Errno::EAGAIN) => {
                let timeout = match deadline {
                    None => PollTimeout::NONE,
                    Some(deadline) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        // Rounded up: a 0 would spin until the deadline.
                        PollTimeout::try_from(left.as_millis().saturating_add(1))
                            .unwrap_or(PollTimeout::MAX)
                    }
                };
                let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLOUT)];
                if matches!(poll(&mut fds, timeout), Ok(0)) {
                    return;
                }
            }
            Err(Errno::EPIPE) => {
                discard_own_sigpipe(sigpipe_was_pending);
                return;
            }
            Err(_) => return,
        }
    }
}

/// Takes back the SIGPIPE the EPIPE just raised, if it's pending now and
/// wasn't before. Only where every signal is blocked (the container's init)
/// is it pending at all, and there it would later be passed on to the
/// command as if someone had sent it.
fn discard_own_sigpipe(was_pending: bool) {
    #[cfg(target_os = "linux")]
    if !was_pending && is_pending(Signal::SIGPIPE) {
        crate::process::discard_pending(Signal::SIGPIPE);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = was_pending;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(hooks: Hooks) -> ContainerHooks {
        ContainerHooks::new(
            hooks,
            "c1",
            "/bundles/c1",
            BTreeMap::from([("key".to_owned(), "value".to_owned())]),
        )
    }

    fn sh(script: &str) -> Hook {
        let mut hook = Hook::new("/bin/sh");
        hook.args = vec!["sh".into(), "-c".into(), script.into()];
        hook
    }

    #[test]
    fn hooks_read_as_the_spec_writes_them() {
        let hooks: Hooks = serde_json::from_value(serde_json::json!({
            "prestart": [{"path": "/bin/true"}],
            "createRuntime": [{"path": "/bin/a", "args": ["a", "-x"], "env": ["K=V"], "timeout": 5}],
            "poststop": []
        }))
        .unwrap();
        assert_eq!(hooks.prestart, vec![Hook::new("/bin/true")]);
        let hook = &hooks.create_runtime[0];
        assert_eq!(
            (hook.args.len(), hook.env[0].as_str(), hook.timeout),
            (2, "K=V", Some(5))
        );
        assert!(hooks.during_setup());
        assert!(!Hooks::default().during_setup());
        let mut after = Hooks::default();
        after.poststop.push(Hook::new("/bin/true"));
        assert!(!after.during_setup() && !after.is_empty());
    }

    #[test]
    fn hooks_that_cant_run_are_named() {
        let mut hooks = Hooks::default();
        hooks.poststart.push(Hook::new("/bin/true"));
        assert_eq!(hooks.check(), Ok(()));
        hooks.poststart.push(Hook::new("true"));
        assert_eq!(
            hooks.check(),
            Err("hooks.poststart[1]: true isn't an absolute path".into())
        );
        let mut hooks = Hooks::default();
        let mut zero = Hook::new("/bin/true");
        zero.timeout = Some(0);
        hooks.create_container.push(zero);
        assert!(hooks.check().unwrap_err().contains("above 0"));
        let mut hooks = Hooks::default();
        let mut env = Hook::new("/bin/true");
        env.env = vec!["NOEQUALS".into()];
        hooks.start_container.push(env);
        assert!(hooks.check().unwrap_err().contains("KEY=VALUE"));
    }

    #[test]
    fn each_phase_gets_its_status() {
        let hooks = container(Hooks::default());
        let state = hooks.state(Phase::CreateRuntime, Some(42));
        assert_eq!((state.status, state.pid), (Status::Creating, Some(42)));
        assert_eq!(
            hooks.state(Phase::StartContainer, Some(42)).status,
            Status::Created
        );
        assert_eq!(
            hooks.state(Phase::Poststart, Some(42)).status,
            Status::Running
        );
        let stopped = hooks.state(Phase::Poststop, Some(42));
        assert_eq!((stopped.status, stopped.pid), (Status::Stopped, None));
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["id"], "c1");
        assert_eq!(json["status"], "creating");
        assert_eq!(json["annotations"]["key"], "value");
        assert_eq!(json["ociVersion"], OCI_VERSION);
    }

    #[test]
    fn a_hook_gets_its_arguments_environment_and_the_state() {
        let dir = std::env::temp_dir().join(format!("shipyard-hooks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("out");
        // `ps` shows the shell's own argv; `$0` is the first argument after
        // the script (`sh -c script $0`).
        let mut hook = sh(&format!(
            "printf '%s|%s|%s|' \"$(ps -o args= -p $$ | cut -d' ' -f1)\" \"$0\" \
             \"$(env | grep -E '^(ONLY|PATH|HOME|USER)=' | sort | tr '\\n' ,)\" > {out}; \
             cat >> {out}",
            out = out.display()
        ));
        hook.args[0] = "my-hook".into();
        hook.args.push("first".into());
        hook.env = vec!["PATH=/usr/bin:/bin".into(), "ONLY=this".into()];
        let mut hooks = Hooks::default();
        hooks.create_runtime.push(hook);
        container(hooks).run(Phase::CreateRuntime, Some(7)).unwrap();
        let written = std::fs::read_to_string(&out).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let parts: Vec<&str> = written.splitn(4, '|').collect();
        // Rosetta, the x86-64 emulator, starts each program with its path
        // as `argv[0]`.
        if !shipyard_test_support::under_rosetta() {
            assert_eq!(parts[0], "my-hook");
        }
        assert_eq!(parts[1..3], ["first", "ONLY=this,PATH=/usr/bin:/bin,"]);
        let state: serde_json::Value = serde_json::from_str(parts[3]).unwrap();
        assert_eq!(state["pid"], 7);
        assert_eq!(state["status"], "creating");
        assert_eq!(state["bundle"], "/bundles/c1");
    }

    #[test]
    fn a_failing_hook_stops_its_phase() {
        let dir = std::env::temp_dir().join(format!("shipyard-hooks-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        let mut hooks = Hooks::default();
        hooks.prestart.push(sh("exit 3"));
        hooks
            .prestart
            .push(sh(&format!("touch {}", marker.display())));
        let err = container(hooks.clone())
            .run(Phase::Prestart, Some(1))
            .unwrap_err();
        assert_eq!(err.index, 0);
        assert_eq!(err.failure, HookFailure::Ended(ExitStatus::Exited(3)));
        assert_eq!(
            err.to_string(),
            "the prestart hook /bin/sh (hooks.prestart[0]) exited with code 3"
        );
        assert!(!marker.exists(), "the next one didn't run");
        // poststop's rule: every one runs, each failure a warning.
        hooks.poststop = hooks.prestart.clone();
        let warnings = container(hooks).run_all(Phase::Poststop, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(marker.exists(), "the next one ran");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hook_past_its_timeout_is_killed() {
        let mut hook = sh("sleep 30");
        hook.timeout = Some(1);
        let mut hooks = Hooks::default();
        hooks.poststart.push(hook);
        let started = Instant::now();
        let err = container(hooks).run(Phase::Poststart, Some(1)).unwrap_err();
        assert_eq!(err.failure, HookFailure::TimedOut(1));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_hook_that_doesnt_read_the_state_is_fine() {
        // Exits at once, closing its end: the write fails with EPIPE (and,
        // SIGPIPE being ignored here, nothing else happens).
        let mut hooks = Hooks::default();
        hooks.poststart.push(Hook::new("/usr/bin/true"));
        if !Path::new("/usr/bin/true").exists() {
            hooks.poststart[0] = Hook::new("/bin/true");
        }
        let mut annotations = BTreeMap::new();
        // Far past a pipe's buffer.
        annotations.insert("big".to_owned(), "x".repeat(1 << 20));
        let container = ContainerHooks::new(hooks, "c1", "/b", annotations);
        assert_eq!(container.run(Phase::Poststart, Some(1)), Ok(()));
        // And one that never reads, with a timeout: not held by the write.
        let mut hooks = Hooks::default();
        let mut sleeper = sh("sleep 30");
        sleeper.timeout = Some(1);
        hooks.poststart.push(sleeper);
        let container = ContainerHooks { hooks, ..container };
        let err = container.run(Phase::Poststart, Some(1)).unwrap_err();
        assert_eq!(err.failure, HookFailure::TimedOut(1));
    }

    #[test]
    fn a_hook_that_cant_start_says_why() {
        let mut hooks = Hooks::default();
        hooks.create_runtime.push(Hook::new("/nonexistent/hook"));
        let err = container(hooks)
            .run(Phase::CreateRuntime, Some(1))
            .unwrap_err();
        // Under Rosetta, the x86-64 emulator, the failed `exec` doesn't come
        // back: the child just exits 127.
        if shipyard_test_support::under_rosetta() {
            assert_eq!(err.failure, HookFailure::Ended(ExitStatus::Exited(127)));
            return;
        }
        assert_eq!(err.failure, HookFailure::Start(Errno::ENOENT));
        assert!(err.to_string().contains("couldn't be started"), "{err}");
    }

    #[test]
    fn failures_round_trip_through_a_record() {
        let hooks = vec![Hook::new("/a"), Hook::new("/b")];
        for failure in [
            HookFailure::Start(Errno::EACCES),
            HookFailure::Ended(ExitStatus::Exited(7)),
            HookFailure::Ended(ExitStatus::from_wait_status(libc::SIGSEGV).unwrap()),
            HookFailure::TimedOut(30),
            HookFailure::Wait(Errno::ECHILD),
        ] {
            let err = HookError {
                phase: Phase::StartContainer,
                index: 1,
                path: PathBuf::from("/b"),
                failure,
            };
            let (errno, detail) = err.to_record();
            assert_eq!(
                HookError::from_record(Phase::StartContainer, &hooks, errno, detail),
                Some(err)
            );
        }
        // A record that names no hook, or no kind, makes no sense.
        assert_eq!(
            HookError::from_record(Phase::StartContainer, &hooks, Errno::EPERM, 5 | (1 << 16)),
            None
        );
        assert_eq!(
            HookError::from_record(Phase::StartContainer, &hooks, Errno::EPERM, 9 << 16),
            None
        );
    }
}

//! Exec (step 11.2b2b, D11.2b-2, D11.2b-3): `POST /containers/{id}/exec`
//! makes an instance, `POST /exec/{id}/start` runs it (the connection taken
//! over, or detached), `GET /exec/{id}/json` says how it went, and `POST
//! /exec/{id}/resize` resizes its terminal: as `docker exec` asks.
//!
//! An exec runs as a child `shipyard exec` (this very binary), not in the
//! service: the engine's exec installs a process-wide supervisor (signal
//! handlers, a subreaper) that a server of many requests can't host. Its
//! environment's values go to the child in `SHIPYARD_CALLER_ENV`, its argv
//! naming only the keys (`--env=KEY`, copied from the caller), so a value
//! is never in `ps`. With a terminal, the child gets a pseudo-terminal of
//! its own, raw (a transport, not a user's terminal), as its controlling
//! terminal, whose master is the client's stream; without one, pipes,
//! framed as Docker frames them. Each attached session runs on threads of
//! its own and counts among the streams served at once; a client that
//! hangs up gets its exec hung up too (SIGHUP to its process group, then
//! SIGKILL).
//!
//! Instances live in the service's memory, lost when it exits, as Docker's
//! are at a restart (D11.2b-3); one never started, or ended, is dropped
//! after [`KEPT`]. `Pid` is the command's, as the host sees it: the child
//! writes it to a file of the exec's once it runs (F117).
//!
//! A detached exec (`docker exec -d`) holds the service up a while at most
//! ([`DETACHED_HOLD`]): a `sleep infinity` never keeps it from its idle
//! exit. The child, and so the command, outlive the service (its unit's
//! `KillMode=process`), as a Docker exec does its daemon's restart; its
//! instance goes with the service (404 then). One its client detached from
//! with its keys holds the service up until it ends: its terminal is the
//! service's, whose exit would hang it up.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper::upgrade::OnUpgrade;
use serde::{Deserialize, Serialize};
use shipyard_runtime::attach::DetachKeys;
use shipyard_runtime::logs::cri::Stream;
use shipyard_runtime::process::EXIT_ENGINE_ERROR;
use shipyard_runtime::terminal::{DEFAULT_SIZE, Size, host_pty, set_window_size};
use tokio::sync::OwnedSemaphorePermit;

use crate::Shared;
use crate::create::Words;
use crate::errors::ApiError;
use crate::idle::Busy;
use crate::query::Query;
use crate::reply::{self, Body};
use crate::streams;

type Reply = Result<hyper::Response<Body>, ApiError>;

/// How long an instance never started, or ended, is kept.
const KEPT: Duration = Duration::from_mins(5);

/// How long a detached exec holds the service up at most (F117): a short
/// one's code is kept while it's still asked for, a long one never keeps
/// the service from its idle exit.
const DETACHED_HOLD: Duration = Duration::from_secs(10);

/// The most a PID file holds.
const PID_FILE_MAX: u64 = 32;

/// After a SIGHUP to an exec whose client hung up, how long before SIGKILL.
const HANG_UP_GRACE: Duration = Duration::from_secs(10);

/// How often a session asks whether its exec has ended or its client hung
/// up; and how long an output read waits before it asks again.
const LOOK: Duration = Duration::from_millis(200);

/// After the exec's end, how long its outputs are drained at most: a
/// process it left running in the background may hold them open for good.
const DRAIN_WAIT: Duration = Duration::from_secs(2);

/// The most an exec's variables may hold: they travel to the child in one
/// environment string, and the kernel takes 128 KiB at most in one.
const ENV_BYTES: usize = 120 * 1024;

/// `POST /containers/{id}/exec`'s body (Docker's `ExecConfig`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub(crate) struct CreateBody {
    attach_stdin: Option<bool>,
    attach_stdout: Option<bool>,
    attach_stderr: Option<bool>,
    tty: Option<bool>,
    cmd: Option<Words>,
    env: Option<Vec<String>>,
    user: Option<String>,
    working_dir: Option<String>,
    privileged: Option<bool>,
    detach_keys: Option<String>,
}

/// `POST /exec/{id}/start`'s body.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct StartBody {
    detach: Option<bool>,
    console_size: Option<[u16; 2]>,
}

/// What an exec runs, as made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecConfig {
    /// The command and its arguments.
    cmd: Vec<String>,
    /// `KEY=VALUE`s.
    env: Vec<String>,
    /// `USER[:GROUP]`.
    user: Option<String>,
    /// Its working directory.
    workdir: Option<String>,
    /// A terminal of its own.
    tty: bool,
    /// Its input attached.
    stdin: bool,
    /// Its stdout and stderr attached, by [`Stream::index`].
    streams: [bool; 2],
    /// The keys that end its session and leave it running (Docker's
    /// ctrl-p,ctrl-q unless it gives others), typed into its terminal: a
    /// session without one passes every byte on, as Docker's.
    detach_keys: DetachKeys,
    /// Its `DetachKeys` as given, for its inspect.
    detach_keys_text: String,
}

/// One exec: its container, what it runs, and how it's going.
#[derive(Debug)]
struct Instance {
    container: String,
    config: ExecConfig,
    state: Mutex<State>,
    /// Where its child writes the command's PID once it runs (in the
    /// runtime's state root, the service user's alone).
    pid_file: Option<std::path::PathBuf>,
}

/// How an exec is going.
#[derive(Debug)]
struct State {
    /// When it was made, or ended (what [`KEPT`] counts from).
    since: Instant,
    started: bool,
    running: bool,
    exit_code: Option<i32>,
    /// Its command's PID on the host, once its PID file is read (never the
    /// child `shipyard exec`'s).
    pid: Option<u32>,
    /// Its terminal's master, while it runs with one.
    master: Option<Arc<OwnedFd>>,
}

impl State {
    /// An exec made at `since`, not started.
    fn new(since: Instant) -> Self {
        Self {
            since,
            started: false,
            running: false,
            exit_code: None,
            pid: None,
            master: None,
        }
    }
}

/// The exec instances, by ID.
#[derive(Debug, Default)]
pub(crate) struct Execs(Mutex<HashMap<String, Arc<Instance>>>);

impl Execs {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Instance>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The instance `id`, or Docker's 404.
    fn get(&self, id: &str) -> Result<Arc<Instance>, ApiError> {
        self.lock().get(id).cloned().ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                format!("No such exec instance: {id}"),
            )
        })
    }

    /// Keeps `instance` as `id`, dropping those kept long enough.
    fn insert(&self, id: String, instance: Arc<Instance>) {
        let mut execs = self.lock();
        let now = Instant::now();
        execs.retain(|_, kept| !kept.expired(now));
        execs.insert(id, instance);
    }
}

impl Instance {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether it's kept no more at `now`: not running, and made (never
    /// started) or ended [`KEPT`] ago.
    fn expired(&self, now: Instant) -> bool {
        let state = self.state();
        !state.running && now.saturating_duration_since(state.since) >= KEPT
    }
}

/// `POST /containers/{id}/exec`: an instance for a running container
/// (409 otherwise), its ID (201).
pub(crate) async fn create(shared: &Arc<Shared>, id: String, body: Incoming) -> Reply {
    #[derive(Serialize)]
    struct Created {
        #[serde(rename = "Id")]
        id: String,
    }
    let body: CreateBody = crate::read_json(body).await?;
    let config = config(body)?;
    let standing = running(shared, id).await?;
    let exec_id = shipyard_engine::docker::new_id()?;
    // Without one, its `Pid` stays 0: said in the log, never the exec's.
    let pid_file = shared
        .paths
        .exec_pid_file(&exec_id)
        .inspect_err(|error| tracing::warn!("exec {exec_id}'s PID won't be known: {error:#}"))
        .ok();
    let instance = Arc::new(Instance {
        container: standing.id.as_str().to_owned(),
        config,
        state: Mutex::new(State::new(Instant::now())),
        pid_file,
    });
    shared.execs.insert(exec_id.clone(), instance);
    Ok(reply::json(StatusCode::CREATED, &Created { id: exec_id }))
}

/// Docker's exec config as Shipyard's: a command needed; `Privileged`
/// refused (Shipyard has no privileged containers yet); variables no
/// bigger than the child's environment takes; detach keys that are keys.
pub(crate) fn config(body: CreateBody) -> Result<ExecConfig, ApiError> {
    if body.privileged == Some(true) {
        return Err(ApiError::bad(
            "Privileged isn't supported: give the container the capabilities it needs",
        ));
    }
    let cmd = body.cmd.map(|words| words.0).unwrap_or_default();
    if cmd.is_empty() {
        return Err(ApiError::bad("No exec command specified"));
    }
    let env = body.env.unwrap_or_default();
    // Measured as the child gets them: one encoded string (escapes grow
    // it), never past what the kernel takes in one (11.6b's API review).
    let values: Vec<(String, String)> = env
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    let encoded = shipyard_runtime::attach::forward::caller_env_value(&values);
    if env.iter().map(String::len).sum::<usize>() > ENV_BYTES || encoded.len() > ENV_BYTES {
        return Err(ApiError::bad(format!(
            "an exec's variables may hold {} KiB at most",
            ENV_BYTES / 1024
        )));
    }
    let nonempty = |text: Option<String>| text.filter(|text| !text.is_empty());
    let detach_keys_text = body.detach_keys.unwrap_or_default();
    let detach_keys = if detach_keys_text.is_empty() {
        DetachKeys::default_keys()
    } else {
        DetachKeys::parse(&detach_keys_text).map_err(|error| ApiError::bad(error.to_string()))?
    };
    Ok(ExecConfig {
        cmd,
        env,
        user: nonempty(body.user),
        workdir: nonempty(body.working_dir),
        tty: body.tty.unwrap_or(false),
        stdin: body.attach_stdin.unwrap_or(false),
        streams: [
            body.attach_stdout.unwrap_or(false),
            body.attach_stderr.unwrap_or(false),
        ],
        detach_keys,
        detach_keys_text,
    })
}

/// The child `shipyard exec`'s argv (after the program) and its
/// `SHIPYARD_CALLER_ENV`: each variable's key on the argv (`--env=KEY`),
/// its value only in the environment. An entry without `=` names no value
/// (Docker's client resolves those itself), so it's left out. The command's
/// PID goes to `pid_file` once it runs. The container and the command come
/// after `--`, so neither is read as an option.
pub(crate) fn child_command(
    container: &str,
    config: &ExecConfig,
    pid_file: Option<&std::path::Path>,
) -> (Vec<OsString>, String) {
    let mut args: Vec<OsString> = vec!["exec".into()];
    if config.stdin {
        args.push("--interactive".into());
    }
    if config.tty {
        args.push("--tty".into());
    }
    if let Some(user) = &config.user {
        args.push(format!("--user={user}").into());
    }
    if let Some(workdir) = &config.workdir {
        args.push(format!("--workdir={workdir}").into());
    }
    let mut values = Vec::new();
    for (key, value) in config.env.iter().filter_map(|entry| entry.split_once('=')) {
        if key.is_empty() {
            continue;
        }
        args.push(format!("--env={key}").into());
        values.push((key.to_owned(), value.to_owned()));
    }
    if let Some(file) = pid_file {
        let mut arg = OsString::from("--pid-file=");
        arg.push(file);
        args.push(arg);
    }
    args.push("--".into());
    args.push(container.into());
    args.extend(config.cmd.iter().map(OsString::from));
    let caller_env = shipyard_runtime::attach::forward::caller_env_value(&values);
    (args, caller_env)
}

/// Container `reference`'s standing, if it's running.
///
/// # Errors
///
/// Not found (404); not running (409, Docker's words).
async fn running(
    shared: &Arc<Shared>,
    reference: String,
) -> Result<shipyard_engine::docker::Standing, ApiError> {
    let standing = shared.standing(reference).await?;
    if !standing.started() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("Container {} is not running", standing.id),
        ));
    }
    Ok(standing)
}

/// `POST /exec/{id}/start` (`Detach`, `ConsoleSize`; an empty body asks
/// neither): detached, 200 at once; else the connection taken over (101)
/// and the exec's streams on it. Started once only (409).
pub(crate) async fn start(
    shared: &Arc<Shared>,
    exec_id: String,
    body: Incoming,
    upgrade: Option<OnUpgrade>,
) -> Reply {
    let body: StartBody = crate::read_json_or_default(body).await?;
    let instance = shared.execs.get(&exec_id)?;
    // Its container still up, as Docker's daemon asks (409): a container
    // gone since the create is said here, not as the exec's exit code.
    running(shared, instance.container.clone()).await?;
    let attached = if body.detach.unwrap_or(false) {
        None
    } else {
        let upgrade = upgrade.ok_or_else(|| {
            ApiError::bad("an exec's start takes the connection over: ask with `Upgrade: tcp`")
        })?;
        Some((upgrade, streams::stream_permit(shared)?))
    };
    {
        let mut state = instance.state();
        if state.started {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                format!("Exec command {exec_id} has already run"),
            ));
        }
        state.started = true;
    }
    let mode = match (&attached, instance.config.tty) {
        (None, _) => Mode::Detached,
        (Some(_), true) => Mode::Terminal(
            body.console_size
                .map_or(DEFAULT_SIZE, |[rows, cols]| Size::new(rows, cols)),
        ),
        (Some(_), false) => Mode::Pipes,
    };
    let busy = shared.idle.connection();
    let (child, io) = spawn(&instance, mode)
        .map_err(|error| ApiError::internal(format!("the exec couldn't start: {error}")))?;
    let pid = child.id();
    let watched = Arc::clone(&instance);
    let Some((upgrade, permit)) = attached else {
        if thread(move || finish(&instance, child, None, busy)).is_none() {
            unwatched(&watched, pid);
            return Err(ApiError::internal(
                "the system gave no thread to watch the exec: it was ended",
            ));
        }
        return Ok(reply::empty(StatusCode::OK));
    };
    let terminal = instance.config.tty;
    tokio::spawn(async move {
        let client = crate::hijack::take_over(upgrade).await;
        if thread(move || session(&instance, child, io, client, busy, permit)).is_none() {
            unwatched(&watched, pid);
        }
    });
    Ok(crate::hijack::switching(streams::content_type(terminal)))
}

/// A thread of the exec's own, or `None` if the system refuses one (a task
/// limit): never std's panic, which would end the whole service, every
/// session with it (11.6b's API review).
fn thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<JoinHandle<T>> {
    std::thread::Builder::new().spawn(work).ok()
}

/// An exec no thread could watch: ended (its process group killed: it's
/// its own), said ended with Shipyard's failure, never "running" for good.
fn unwatched(instance: &Instance, pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let mut state = instance.state();
    state.running = false;
    state.exit_code = Some(i32::from(EXIT_ENGINE_ERROR));
    state.master = None;
    state.since = Instant::now();
}

/// How an exec's streams are made, decided before it's spawned.
#[derive(Debug, Clone, Copy)]
enum Mode {
    /// A terminal of its own, this big.
    Terminal(Size),
    /// Pipes.
    Pipes,
    /// None: detached.
    Detached,
}

/// A running exec's streams.
#[derive(Debug)]
enum Io {
    /// A terminal: its master, both ways.
    Terminal(Arc<OwnedFd>),
    /// Pipes: its input (if attached), stdout and stderr.
    Pipes {
        stdin: Option<std::process::ChildStdin>,
        stdout: std::process::ChildStdout,
        stderr: std::process::ChildStderr,
    },
    /// None: detached.
    Detached,
}

/// Spawns the exec's child, its streams made as `mode` says: it and its
/// streams. Linux only: the child is this binary, as `/proc/self/exe`.
fn spawn(instance: &Instance, mode: Mode) -> std::io::Result<(Child, Io)> {
    use std::os::unix::process::CommandExt as _;
    if cfg!(not(target_os = "linux")) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "containers need Linux",
        ));
    }
    let (args, caller_env) = child_command(
        &instance.container,
        &instance.config,
        instance.pid_file.as_deref(),
    );
    let mut command = Command::new("/proc/self/exe");
    command
        .args(&args)
        .env(shipyard_runtime::attach::forward::CALLER_ENV, caller_env);
    let mut master = None;
    match mode {
        Mode::Terminal(size) => {
            let (primary, slave) = host_pty(size).map_err(std::io::Error::from)?;
            // A transport, not a user's terminal: raw, so the bytes pass as
            // they are (no `\r\n` for `\n`, no echo, no ^C as a signal); the
            // child sets the container's own terminal as the command asks.
            let mut settings = nix::sys::termios::tcgetattr(&slave)?;
            nix::sys::termios::cfmakeraw(&mut settings);
            nix::sys::termios::tcsetattr(&slave, nix::sys::termios::SetArg::TCSANOW, &settings)?;
            command
                .stdin(Stdio::from(slave.try_clone()?))
                .stdout(Stdio::from(slave.try_clone()?))
                .stderr(Stdio::from(slave));
            // SAFETY: between the fork and the exec, two system calls only
            // (`setsid`, `ioctl`): async-signal-safe, no allocation, no lock.
            unsafe {
                command.pre_exec(shipyard_runtime::terminal::take_controlling_terminal);
            }
            master = Some(Arc::new(primary));
        }
        Mode::Pipes => {
            let stdin = if instance.config.stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            };
            command
                .stdin(stdin)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
        }
        Mode::Detached => {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0);
        }
    }
    let mut child = command.spawn()?;
    let io = match (mode, &master) {
        (Mode::Terminal(_), Some(master)) => Io::Terminal(Arc::clone(master)),
        (Mode::Pipes, _) => Io::Pipes {
            stdin: child.stdin.take(),
            stdout: child.stdout.take().ok_or(std::io::ErrorKind::BrokenPipe)?,
            stderr: child.stderr.take().ok_or(std::io::ErrorKind::BrokenPipe)?,
        },
        _ => Io::Detached,
    };
    let mut state = instance.state();
    state.running = true;
    state.master = master;
    Ok((child, io))
}

/// A session: the exec's output to the client, the client's input to the
/// exec, until it ends or the client types its detach keys (a terminal's:
/// the session ends, the exec goes on); a client gone before it began, or
/// that hangs up, hangs the exec up. `busy` holds the service up, `permit`
/// is its stream's place.
fn session(
    instance: &Instance,
    child: Child,
    io: Io,
    client: Option<(tokio::net::UnixStream, Bytes)>,
    busy: Busy,
    permit: OwnedSemaphorePermit,
) {
    let taken = client.and_then(|(client, typed)| {
        let client = client.into_std().ok()?;
        client.set_nonblocking(false).ok()?;
        Some((client, typed))
    });
    let Some((client, typed)) = taken else {
        hang_up(&child);
        return finish(instance, child, Some(Watched::gone()), busy);
    };
    let ended = Arc::new(AtomicBool::new(false));
    let keys_typed = Arc::new(AtomicBool::new(false));
    let outputs = match io {
        Io::Terminal(master) => terminal(
            &client,
            typed,
            &master,
            &instance.config.detach_keys,
            &keys_typed,
            &ended,
        ),
        Io::Pipes {
            stdin,
            stdout,
            stderr,
        } => pipes(
            &client,
            typed,
            stdin,
            [OwnedFd::from(stdout), OwnedFd::from(stderr)],
            instance.config.streams,
            &ended,
        ),
        Io::Detached => Vec::new(),
    };
    let watched = Watched {
        client: Some(client),
        outputs,
        ended,
        hung_up: false,
        keys_typed,
        permit: Some(permit),
    };
    finish(instance, child, Some(watched), busy);
}

/// An attached exec's client and output pumps, for [`finish`].
struct Watched {
    /// Its client: `None` if it was gone before the session began.
    client: Option<UnixStream>,
    outputs: Vec<JoinHandle<()>>,
    /// Set once the exec has ended: its outputs drain for [`DRAIN_WAIT`].
    ended: Arc<AtomicBool>,
    /// Hung up already (its client was gone before the session began).
    hung_up: bool,
    /// Set once its client typed its detach keys: it runs on, detached.
    keys_typed: Arc<AtomicBool>,
    /// Its stream's place, let go of once it's detached.
    permit: Option<OwnedSemaphorePermit>,
}

impl Watched {
    /// A session whose client was gone before it began: hung up at once.
    fn gone() -> Self {
        Self {
            client: None,
            outputs: Vec::new(),
            ended: Arc::new(AtomicBool::new(false)),
            hung_up: true,
            keys_typed: Arc::new(AtomicBool::new(false)),
            permit: None,
        }
    }
}

/// A terminal's pumps: the master's bytes to the client, as they are; the
/// client's (its first ones first) to the master, watched for its detach
/// `keys`: typed, `keys_typed` is set, the client's stream ends both ways
/// and the exec runs on (its output still read, and dropped: a full
/// terminal would stop it). The output pump's handle.
fn terminal(
    client: &UnixStream,
    typed: Bytes,
    master: &Arc<OwnedFd>,
    keys: &DetachKeys,
    keys_typed: &Arc<AtomicBool>,
    ended: &Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    use std::io::Read as _;
    let (Ok(from_master), Ok(mut to_master), Ok(mut from_client), Ok(mut to_client)) = (
        master.try_clone(),
        master.try_clone().map(std::fs::File::from),
        client.try_clone(),
        client.try_clone(),
    ) else {
        return Vec::new();
    };
    let Ok(closing) = client.try_clone() else {
        return Vec::new();
    };
    let mut matcher = keys.matcher();
    let detaching = Arc::clone(keys_typed);
    // (Refused a thread, it takes no input: its output's still passed on.)
    let _ = thread(move || {
        let mut passed = Vec::new();
        let mut buffer = vec![0_u8; 8 * 1024];
        let mut bytes = typed.to_vec();
        loop {
            passed.clear();
            let matched = matcher.feed(&bytes, &mut passed);
            if to_master.write_all(&passed).is_err() {
                return;
            }
            if matched {
                detaching.store(true, Ordering::Release);
                let _ = closing.shutdown(std::net::Shutdown::Both);
                return;
            }
            match from_client.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => bytes = buffer.get(..read).unwrap_or_default().to_vec(),
            }
        }
    });
    let (ended, detached) = (Arc::clone(ended), Arc::clone(keys_typed));
    // The master's end (EIO once the child's side is closed) ends it.
    thread(move || {
        pump(&from_master, &ended, |bytes| {
            if detached.load(Ordering::Acquire) {
                return Ok(());
            }
            to_client.write_all(bytes).or_else(|error| {
                if detached.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err(error)
                }
            })
        });
    })
    .into_iter()
    .collect()
}

/// Pipes' pumps: stdout and stderr framed to the client (each only if
/// attached, else read and dropped), the client's bytes to stdin (its
/// first ones first), stdin closed at the client's half-close. The output
/// pumps' handles.
fn pipes(
    client: &UnixStream,
    typed: Bytes,
    stdin: Option<std::process::ChildStdin>,
    [stdout, stderr]: [OwnedFd; 2],
    streams: [bool; 2],
    ended: &Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    let Ok(writer) = client.try_clone() else {
        return Vec::new();
    };
    let writer = Arc::new(Mutex::new(writer));
    if let (Some(mut stdin), Ok(mut from_client)) = (stdin, client.try_clone()) {
        let _ = thread(move || {
            if stdin.write_all(&typed).is_ok() {
                let _ = std::io::copy(&mut from_client, &mut stdin);
            }
            // Dropped: its end of file.
        });
    }
    let out = |from: OwnedFd, stream: Stream| {
        let (writer, ended) = (Arc::clone(&writer), Arc::clone(ended));
        let wanted = streams[stream.index()];
        thread(move || {
            pump(&from, &ended, |bytes| {
                if !wanted {
                    return Ok(());
                }
                let framed = streams::frame(stream, bytes);
                let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
                writer.write_all(&framed)
            });
        })
    };
    [out(stdout, Stream::Stdout), out(stderr, Stream::Stderr)]
        .into_iter()
        .flatten()
        .collect()
}

/// Reads `from` and hands each piece to `write`, until its end, a write
/// that fails, or, once `ended` is set (the exec's end), a quiet look or
/// [`DRAIN_WAIT`]: what a background process the exec left holds open is
/// never waited for.
fn pump(from: &OwnedFd, ended: &AtomicBool, mut write: impl FnMut(&[u8]) -> std::io::Result<()>) {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let mut buffer = vec![0_u8; 32 * 1024];
    let mut draining_since = None;
    let look = PollTimeout::try_from(LOOK).unwrap_or(PollTimeout::MAX);
    loop {
        let ready = {
            let mut fds = [PollFd::new(from.as_fd(), PollFlags::POLLIN)];
            poll(&mut fds, look).is_ok_and(|ready| ready > 0)
        };
        if ended.load(Ordering::Acquire) {
            let since = *draining_since.get_or_insert_with(Instant::now);
            if !ready || since.elapsed() >= DRAIN_WAIT {
                return;
            }
        }
        if !ready {
            continue;
        }
        let read = match nix::unistd::read(from, &mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        if write(buffer.get(..read).unwrap_or_default()).is_err() {
            return;
        }
    }
}

/// Waits for the exec's end, its command's PID read as soon as it's
/// written, watching its client (a hang-up hangs it up: SIGHUP, then one
/// SIGKILL after [`HANG_UP_GRACE`]); then keeps its code, drains its
/// outputs, closes the client's stream for writing, and lets the service go
/// (`busy`). Detached at its start, it lets the service go before its end,
/// after [`DETACHED_HOLD`]; detached from by its keys, its stream's place
/// goes at once, the service when it ends.
fn finish(instance: &Instance, mut child: Child, mut watched: Option<Watched>, busy: Busy) {
    let mut hung_up_at = watched
        .as_ref()
        .is_some_and(|watched| watched.hung_up)
        .then(Instant::now);
    let mut killed = false;
    let began = Instant::now();
    let mut busy = Some(busy);
    let status = loop {
        read_pid(instance);
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if released(watched.as_ref(), began) {
            busy = None;
        }
        let keys_typed = watched
            .as_ref()
            .is_some_and(|watched| watched.keys_typed.load(Ordering::Acquire));
        if keys_typed && let Some(watched) = &mut watched {
            watched.permit = None;
        }
        // Its keys read after the hang-up's look: they're set before the
        // stream is shut, so a shut seen here is never taken for a hang-up.
        if hung_up_at.is_none()
            && let Some(watched) = watched.as_ref()
            && let Some(client) = watched.client.as_ref()
            && crate::hijack::hung_up(client.as_fd())
            && !watched.keys_typed.load(Ordering::Acquire)
        {
            hang_up(&child);
            hung_up_at = Some(Instant::now());
        }
        if !killed && hung_up_at.is_some_and(|at| at.elapsed() >= HANG_UP_GRACE) {
            let _ = child.kill();
            killed = true;
        }
        std::thread::sleep(LOOK);
    };
    let code = exit_code(status);
    // A command that ended at once may have said its PID only now; a file
    // never read whole (garbled) goes all the same.
    read_pid(instance);
    if let Some(file) = &instance.pid_file {
        let _ = std::fs::remove_file(file);
    }
    // Kept before the client sees its stream end: Docker's CLI asks for the
    // code at once.
    {
        let mut state = instance.state();
        state.running = false;
        state.exit_code = Some(code);
        state.master = None;
        state.since = Instant::now();
    }
    if let Some(watched) = watched {
        watched.ended.store(true, Ordering::Release);
        for output in watched.outputs {
            let _ = output.join();
        }
        if let Some(client) = watched.client {
            let _ = client.shutdown(std::net::Shutdown::Write);
        }
    }
    drop(busy);
}

/// Whether the exec lets the service go before its end: detached at its
/// start (none watched), once [`DETACHED_HOLD`] has passed. One detached
/// from by its client's keys never does: its child's terminal is the
/// service's, and the service's exit would hang it up, and so the command
/// (11.6a3's Murphy review); its stream's place goes at the detach.
fn released(watched: Option<&Watched>, began: Instant) -> bool {
    watched.is_none() && began.elapsed() >= DETACHED_HOLD
}

/// The code an exec's `status` says, Docker's way: its own, or 128 + its
/// signal; Shipyard's failure if it can't be told.
fn exit_code(status: Option<std::process::ExitStatus>) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    status
        .and_then(|status| {
            status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal))
        })
        .unwrap_or(i32::from(EXIT_ENGINE_ERROR))
}

/// Removes the PID files execs left in the runtime's state root (`dir`),
/// and their writes' temporaries, older than [`KEPT`]: one written after its
/// service had gone (its exec's instance with it), or cut short. A live
/// one is read within a look: another service's (root's tests share the
/// root) is never taken.
pub(crate) fn sweep_pid_files(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let ours = name.starts_with(".exec-") || name.starts_with("..exec-");
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|at| at.elapsed().is_ok_and(|age| age >= KEPT));
        if ours && old && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The command's PID, from its PID file once the child has written it (a
/// file whole or not at all): kept, and the file removed.
fn read_pid(instance: &Instance) {
    let Some(file) = &instance.pid_file else {
        return;
    };
    if instance.state().pid.is_some() {
        return;
    }
    let pid = shipyard_sys::fs::read_bounded(file, PID_FILE_MAX)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| text.trim().parse::<u32>().ok());
    if let Some(pid) = pid {
        instance.state().pid = Some(pid);
        let _ = std::fs::remove_file(file);
    }
}

/// SIGHUP to the exec's process group (its own: a session or a group of
/// its own), as a terminal's hang-up would.
fn hang_up(child: &Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGHUP,
        );
    }
}

/// `GET /exec/{id}/json`'s answer (Docker's `ExecInspectResponse`).
#[derive(Debug, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "a mirror of Docker's answer, which has these booleans"
)]
#[serde(rename_all = "PascalCase")]
struct ExecInspect {
    #[serde(rename = "ID")]
    id: String,
    running: bool,
    exit_code: Option<i32>,
    process_config: ProcessConfig,
    open_stdin: bool,
    open_stdout: bool,
    open_stderr: bool,
    can_remove: bool,
    #[serde(rename = "ContainerID")]
    container_id: String,
    detach_keys: String,
    /// The command's, as the host sees it (0 until it runs).
    pid: u32,
}

/// An exec's process, as Docker says it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProcessConfig {
    privileged: bool,
    user: String,
    tty: bool,
    entrypoint: String,
    arguments: Vec<String>,
}

/// `GET /exec/{id}/json`.
pub(crate) fn inspect(shared: &Shared, exec_id: String) -> Reply {
    let instance = shared.execs.get(&exec_id)?;
    Ok(reply::json(StatusCode::OK, &inspected(exec_id, &instance)))
}

/// What `GET /exec/{id}/json` says of `instance`.
fn inspected(id: String, instance: &Instance) -> ExecInspect {
    let state = instance.state();
    let config = &instance.config;
    let (entrypoint, arguments) = config
        .cmd
        .split_first()
        .map_or((String::new(), Vec::new()), |(first, rest)| {
            (first.clone(), rest.to_vec())
        });
    ExecInspect {
        id,
        running: state.running,
        exit_code: state.exit_code,
        process_config: ProcessConfig {
            privileged: false,
            user: config.user.clone().unwrap_or_default(),
            tty: config.tty,
            entrypoint,
            arguments,
        },
        open_stdin: config.stdin,
        open_stdout: config.streams[Stream::Stdout.index()],
        open_stderr: config.streams[Stream::Stderr.index()],
        can_remove: false,
        container_id: instance.container.clone(),
        detach_keys: config.detach_keys_text.clone(),
        pid: state.pid.unwrap_or(0),
    }
}

/// `POST /exec/{id}/resize` (`h`, `w`): its terminal resized (the child's
/// own resizes the container's at the SIGWINCH the kernel sends it). 409
/// if it has no terminal, or isn't running.
pub(crate) fn resize(shared: &Shared, exec_id: &str, query: &Query) -> Reply {
    let rows: u16 = query
        .parsed("h")?
        .ok_or_else(|| ApiError::bad("resize needs h and w"))?;
    let cols: u16 = query
        .parsed("w")?
        .ok_or_else(|| ApiError::bad("resize needs h and w"))?;
    let instance = shared.execs.get(exec_id)?;
    let master = instance.state().master.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            format!("exec {exec_id} has no terminal running"),
        )
    })?;
    set_window_size(master.as_raw_fd(), Size::new(rows, cols))
        .map_err(|error| ApiError::internal(format!("the exec's terminal: {error}")))?;
    Ok(reply::empty(StatusCode::OK))
}

#[cfg(test)]
mod tests;

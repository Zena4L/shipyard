//! `shipyard run`: flags in, a validated `ProcessSpec` out.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, value_parser};
use shipyard_runtime::isolation::{Isolation, Root};
use shipyard_runtime::process::{self, ExitStatus};
use shipyard_runtime::process::{
    DEFAULT_STOP_TIMEOUT_SECS, Env, ProcessSpec, Rlimit, Signal, StopPolicy, User,
};
use shipyard_runtime::security::seccomp::Profile;
use shipyard_runtime::security::{Capabilities, Capability, CapabilitySet, Seccomp, Security};

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Set an environment variable, or copy KEY from your shell if no value is given.
    #[arg(short, long, value_name = "KEY[=VALUE]", value_parser = value_parser!(OsString))]
    env: Vec<OsString>,

    /// Working directory inside the container (absolute path).
    #[arg(short, long, value_name = "DIR", default_value = "/")]
    workdir: PathBuf,

    /// Run as this user and group: names (from the container's /etc/passwd
    /// and /etc/group) or numbers.
    #[arg(short, long, value_name = "USER[:GROUP]", value_parser = parse_user)]
    user: Option<(String, Option<String>)>,

    /// Add a supplementary group: a name or a number.
    #[arg(long, value_name = "GROUP", value_parser = parse_name_or_id)]
    group_add: Vec<String>,

    /// Set a resource limit, e.g. nofile=1024:2048 or core=0.
    #[arg(long, value_name = "NAME=SOFT[:HARD]")]
    ulimit: Vec<Rlimit>,

    /// Signal sent to stop the command.
    #[arg(long, value_name = "SIGNAL", default_value = "SIGTERM")]
    stop_signal: Signal,

    /// Seconds to wait after the stop signal before killing the command.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_STOP_TIMEOUT_SECS)]
    stop_timeout: u64,

    /// Run without isolation: the command sees the host's processes, mounts,
    /// hostname and IPC. On Linux, commands are isolated by default (which
    /// needs root); on macOS they always run like this for now.
    #[arg(long)]
    no_isolation: bool,

    /// Hostname inside the container (Linux).
    #[arg(long, value_name = "NAME", conflicts_with = "no_isolation")]
    hostname: Option<String>,

    /// NIS domain name inside the container (Linux).
    #[arg(long, value_name = "NAME", conflicts_with = "no_isolation")]
    domainname: Option<String>,

    /// Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824
    /// (Linux; IPC settings only).
    #[arg(long, value_name = "KEY=VALUE", value_parser = parse_sysctl, conflicts_with = "no_isolation")]
    sysctl: Vec<(String, String)>,

    /// Use DIR as the container's root filesystem: it becomes `/` inside,
    /// with its own /proc, /sys and /dev (Linux).
    #[arg(long, value_name = "DIR", conflicts_with = "no_isolation")]
    rootfs: Option<PathBuf>,

    /// Make the container's root filesystem read-only (with --rootfs).
    #[arg(long, requires = "rootfs")]
    read_only: bool,

    /// Give the command a capability on top of the default set, like
    /// `NET_ADMIN` (or `CAP_NET_ADMIN`); `ALL` for every one. Repeatable
    /// (Linux).
    #[arg(long, value_name = "CAP", value_parser = parse_capability, conflicts_with = "no_isolation")]
    cap_add: Vec<CapabilityArg>,

    /// Take a capability out of the default set; `ALL` for every one.
    /// Repeatable (Linux).
    #[arg(long, value_name = "CAP", value_parser = parse_capability, conflicts_with = "no_isolation")]
    cap_drop: Vec<CapabilityArg>,

    /// A security option: `no-new-privileges=false` lets setuid programs
    /// inside gain privileges (they can't by default); `seccomp=FILE` filters
    /// system calls with a profile of your own, `seccomp=unconfined` not at
    /// all (Docker's profile by default). Repeatable (Linux).
    #[arg(long, value_name = "OPTION", value_parser = parse_security_opt, conflicts_with = "no_isolation")]
    security_opt: Vec<SecurityOpt>,

    /// Run the OCI bundle in DIR: its config.json says what to run and how,
    /// so no COMMAND or container options are given (Linux).
    #[arg(
        long,
        value_name = "DIR",
        conflicts_with_all = [
            "command", "env", "workdir", "user", "group_add", "ulimit",
            "no_isolation", "hostname", "domainname", "sysctl", "rootfs", "read_only",
            "cap_add", "cap_drop", "security_opt",
        ],
    )]
    bundle: Option<PathBuf>,

    /// The command to run, followed by its arguments.
    #[arg(
        required_unless_present = "bundle",
        trailing_var_arg = true,
        value_name = "COMMAND",
        value_parser = value_parser!(OsString),
    )]
    command: Vec<OsString>,
}

/// Everything a `shipyard run` needs, checked.
#[derive(Debug)]
pub(crate) struct Invocation {
    spec: ProcessSpec,
    stop: StopPolicy,
    isolation: Option<Isolation>,
    /// What a bundle asks for that's ignored, to show the user.
    warnings: Vec<String>,
}

impl RunArgs {
    /// Builds and validates the process spec, stop policy and isolation.
    pub(crate) fn into_invocation(self) -> Result<Invocation> {
        let stop = StopPolicy {
            signal: self.stop_signal,
            timeout: Duration::from_secs(self.stop_timeout),
        };
        stop.validate()?;
        if let Some(dir) = &self.bundle {
            return from_bundle(dir, stop);
        }
        let isolation = self.isolation()?;
        let mut user = match self.user {
            Some((name, group)) => {
                let mut user = User::named(name, group);
                // A container's own /etc/group, as Docker; not the host's,
                // whose groups an ordinary user couldn't take anyway.
                user.names.memberships = isolation.is_some();
                user
            }
            None => User::current(),
        };
        user.names.additional_groups = dedup(self.group_add);

        let mut env = Env::new();
        for entry in &self.env {
            add_env(&mut env, entry)?;
        }
        env.fill_default_path();

        let mut spec = ProcessSpec::new(self.command);
        spec.env = env;
        spec.cwd = self.workdir;
        spec.user = user;
        spec.rlimits = last_wins(self.ulimit);
        spec.validate()?;
        Ok(Invocation {
            spec,
            stop,
            isolation,
            warnings: Vec::new(),
        })
    }
}

/// A run of the OCI bundle in `dir`: everything but the stop policy comes
/// from its `config.json`.
fn from_bundle(dir: &Path, stop: StopPolicy) -> Result<Invocation> {
    if !cfg!(target_os = "linux") {
        bail!(
            "--bundle needs Linux namespaces, which aren't available on {} yet",
            std::env::consts::OS
        );
    }
    let bundle = shipyard_runtime::oci::load_bundle(dir)?;
    Ok(Invocation {
        spec: bundle.spec,
        stop,
        isolation: Some(bundle.isolation),
        warnings: bundle.warnings,
    })
}

impl RunArgs {
    /// The isolation for this run: the standard namespaces on Linux unless
    /// `--no-isolation`; none on macOS, where namespaces don't exist (yet:
    /// the VM comes in a later version).
    fn isolation(&self) -> Result<Option<Isolation>> {
        if self.no_isolation {
            return Ok(None);
        }
        if !cfg!(target_os = "linux") {
            for (given, flag) in [
                (self.hostname.is_some(), "--hostname"),
                (self.domainname.is_some(), "--domainname"),
                (!self.sysctl.is_empty(), "--sysctl"),
                (self.rootfs.is_some(), "--rootfs"),
                (!self.cap_add.is_empty(), "--cap-add"),
                (!self.cap_drop.is_empty(), "--cap-drop"),
                (!self.security_opt.is_empty(), "--security-opt"),
            ] {
                if given {
                    bail!(
                        "{flag} needs Linux namespaces, which aren't available on {} yet",
                        std::env::consts::OS
                    );
                }
            }
            tracing::debug!("no namespaces on this system; running without isolation");
            return Ok(None);
        }
        let mut isolation = Isolation::standard();
        isolation.hostname.clone_from(&self.hostname);
        isolation.domainname.clone_from(&self.domainname);
        isolation.sysctls.clone_from(&self.sysctl);
        isolation.security = self.security()?;
        if let Some(dir) = &self.rootfs {
            // Absolute and without symlinks, as the runtime wants it; and a
            // mistyped path is caught here, with its name.
            let path = std::fs::canonicalize(dir)
                .with_context(|| format!("--rootfs {}", dir.display()))?;
            let mut root = Root::new(path);
            root.read_only = self.read_only;
            isolation.root = Some(root);
        }
        isolation.validate()?;
        Ok(Some(isolation))
    }
}

impl RunArgs {
    /// The container's capabilities and `no_new_privs`, from Docker's default
    /// set and `--cap-drop`, `--cap-add` and `--security-opt`.
    fn security(&self) -> Result<Security> {
        let named = |args: &[CapabilityArg]| -> CapabilitySet {
            args.iter()
                .filter_map(|arg| match arg {
                    CapabilityArg::One(capability) => Some(*capability),
                    CapabilityArg::All => None,
                })
                .collect()
        };
        let (dropped, added) = (named(&self.cap_drop), named(&self.cap_add));
        if let Some(both) = dropped
            .iter()
            .find(|&capability| added.contains(capability))
        {
            bail!("--cap-add and --cap-drop both name {both}");
        }
        let mut kept = if self.cap_drop.contains(&CapabilityArg::All) {
            CapabilitySet::EMPTY
        } else {
            CapabilitySet::DOCKER_DEFAULT
        };
        for capability in dropped.iter() {
            kept.remove(capability);
        }
        let added = if self.cap_add.contains(&CapabilityArg::All) {
            CapabilitySet::ALL
        } else {
            added
        };
        let mut security = Security::standard();
        security.capabilities = Capabilities::Default { kept, added };
        for option in &self.security_opt {
            match option {
                SecurityOpt::NoNewPrivileges(on) => security.no_new_privileges = *on,
                SecurityOpt::SeccompUnconfined => security.seccomp = Seccomp::Unconfined,
                SecurityOpt::SeccompProfile(path) => {
                    security.seccomp = Seccomp::Profile(Box::new(read_profile(path)?));
                }
            }
        }
        Ok(security)
    }
}

/// `--cap-add` or `--cap-drop`: one capability, or all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilityArg {
    All,
    One(Capability),
}

fn parse_capability(s: &str) -> Result<CapabilityArg, String> {
    if s.eq_ignore_ascii_case("ALL") {
        return Ok(CapabilityArg::All);
    }
    s.parse()
        .map(CapabilityArg::One)
        .map_err(|err| format!("{err}"))
}

/// A seccomp profile file, checked (`--security-opt seccomp=FILE`): a
/// mistake shows here, naming the file, not when the container starts.
fn read_profile(path: &Path) -> Result<Profile> {
    Profile::from_file(path).with_context(|| format!("seccomp profile {}", path.display()))
}

/// `--security-opt`: Docker's spelling, `KEY=VALUE` or `KEY:VALUE`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SecurityOpt {
    NoNewPrivileges(bool),
    /// `seccomp=unconfined`: no filter.
    SeccompUnconfined,
    /// `seccomp=FILE`.
    SeccompProfile(PathBuf),
}

fn parse_security_opt(s: &str) -> Result<SecurityOpt, String> {
    let (key, value) = match s.split_once(['=', ':']) {
        Some((key, value)) => (key, Some(value)),
        None => (s, None),
    };
    match (key, value) {
        ("no-new-privileges", None | Some("true")) => Ok(SecurityOpt::NoNewPrivileges(true)),
        ("no-new-privileges", Some("false")) => Ok(SecurityOpt::NoNewPrivileges(false)),
        ("no-new-privileges", Some(other)) => Err(format!(
            "no-new-privileges takes true or false, not \"{other}\""
        )),
        ("seccomp", Some("unconfined")) => Ok(SecurityOpt::SeccompUnconfined),
        ("seccomp", Some(path)) if !path.is_empty() => {
            Ok(SecurityOpt::SeccompProfile(PathBuf::from(path)))
        }
        ("seccomp", _) => Err("seccomp takes a profile file, or unconfined".to_owned()),
        _ => Err(format!(
            "unknown security option \"{s}\" (known: no-new-privileges[=true|false], \
             seccomp=FILE|unconfined)"
        )),
    }
}

/// Runs the command under the supervisor and exits the way it did: with its
/// exit code, or `128 + N` if signal N killed it. Failures to start it exit
/// 125, 126 or 127.
pub(crate) fn run(args: RunArgs) -> Result<ExitCode> {
    let Invocation {
        spec,
        stop,
        isolation,
        warnings,
    } = args.into_invocation()?;
    for warning in &warnings {
        crate::warning(warning);
    }
    tracing::debug!(?spec, ?stop, ?isolation, "running");
    let mut supervisor = process::Supervisor::install()?;
    let status = match supervisor.supervise(&spec, stop, isolation.as_ref()) {
        Ok(status) => status,
        Err(err) => {
            let code = err.exit_code();
            let isolation_failed = is_isolation_failure(&err);
            let exit = crate::report(&anyhow::Error::from(err), code);
            // The runtime can't name the CLI's flag; this is where it's known.
            // Only when it would work: `--no-isolation` can't be combined with
            // a root filesystem, a hostname, a domain name or sysctls.
            if isolation_failed && isolation.as_ref().is_some_and(only_namespaces) {
                crate::hint("--no-isolation runs the command without namespaces");
            }
            return Ok(exit);
        }
    };
    if let ExitStatus::Signaled { signal, .. } = status
        && signal == Signal::INT
    {
        // So that a shell loop around `shipyard run` stops on Ctrl-C too.
        process::die_from(signal);
    }
    Ok(ExitCode::from(status.code()))
}

/// Whether `isolation` asks for nothing but the namespaces themselves, so
/// that `--no-isolation` could replace it.
fn only_namespaces(isolation: &Isolation) -> bool {
    isolation.root.is_none()
        && isolation.hostname.is_none()
        && isolation.domainname.is_none()
        && isolation.sysctls.is_empty()
        && isolation.security == Security::standard()
}

/// Whether `err` is isolation going wrong (missing privileges, a refusal,
/// or a failed namespace step), which `--no-isolation` avoids.
fn is_isolation_failure(err: &shipyard_runtime::Error) -> bool {
    use shipyard_runtime::Error;
    match err {
        Error::IsolationNeedsRoot | Error::IsolationRefused | Error::InitDidNotStart { .. } => true,
        Error::Spawn(spawn) => spawn.kind() == process::SpawnErrorKind::Isolation,
        _ => false,
    }
}

/// `KEY=VALUE` sets a variable; a bare `KEY` copies it from shipyard's own
/// environment, and is skipped if unset (Docker's behavior).
///
/// Error messages name the variable but never echo the value, which may be a
/// secret.
fn add_env(env: &mut Env, entry: &OsStr) -> Result<()> {
    let bytes = entry.as_bytes();
    let key = OsStr::from_bytes(bytes.split(|&b| b == b'=').next().unwrap_or_default());
    let context = || format!("invalid --env \"{}\"", key.to_string_lossy());

    if bytes.contains(&b'=') {
        return env.push_entry(entry).with_context(context);
    }
    // A bare key is looked up in the shell, so check it first: an empty or
    // spaced name would otherwise just be "not found" and silently dropped.
    if key.is_empty() {
        bail!("invalid --env: the variable name is empty");
    }
    // Not named in the message: without `=`, a typo like `TOKEN secret`
    // would put the secret in the "name".
    if bytes.iter().any(u8::is_ascii_whitespace) {
        bail!("invalid --env: names can't contain spaces (did you mean KEY=VALUE?)");
    }
    let Some(value) = std::env::var_os(key) else {
        // Not named here either: a value passed without `KEY=` (`-e "$TOKEN"`)
        // looks exactly like a name that isn't set.
        tracing::debug!("a bare --env name isn't set in the shell; skipped");
        return Ok(());
    };
    env.set(key, &value).with_context(context)
}

/// Removes repeated groups, keeping the first occurrence of each.
fn dedup(groups: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    groups
        .into_iter()
        .filter(|group| seen.insert(group.clone()))
        .collect()
}

/// A later `--ulimit` for the same resource replaces an earlier one.
fn last_wins(limits: Vec<Rlimit>) -> Vec<Rlimit> {
    let mut merged: Vec<Rlimit> = Vec::new();
    for limit in limits {
        match merged.iter_mut().find(|m| m.kind == limit.kind) {
            Some(existing) => *existing = limit,
            None => merged.push(limit),
        }
    }
    merged
}

/// Parses `USER` or `USER:GROUP`, each a name or a number.
fn parse_user(s: &str) -> Result<(String, Option<String>), String> {
    match s.split_once(':') {
        Some((user, group)) => Ok((parse_name_or_id(user)?, Some(parse_name_or_id(group)?))),
        None => Ok((parse_name_or_id(s)?, None)),
    }
}

/// A user or group: a number (checked as [`parse_id`] does), or a name to
/// look up in the container's `/etc/passwd` or `/etc/group`.
fn parse_name_or_id(s: &str) -> Result<String, String> {
    if s.bytes().all(|b| b.is_ascii_digit()) {
        return parse_id(s).map(|_| s.to_owned());
    }
    // A sign or a space: a malformed number, more likely than a name.
    let malformed = s.is_empty()
        || s.starts_with(['-', '+'])
        || s.contains(|c: char| c.is_whitespace() || matches!(c, ':' | ',' | '/' | '\0'));
    if malformed {
        return Err(format!("\"{s}\" is neither a name nor a number"));
    }
    Ok(s.to_owned())
}

/// `KEY=VALUE`, split at the first `=`. Which keys are allowed is checked
/// with the rest of the isolation.
fn parse_sysctl(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .ok_or_else(|| format!("\"{s}\" isn't KEY=VALUE"))
}

/// A strict numeric user or group ID: digits only, and never `u32::MAX`, which
/// the kernel reads as "leave unchanged".
fn parse_id(s: &str) -> Result<u32, String> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("\"{s}\" is not a valid numeric ID"));
    }
    match s.parse::<u32>() {
        Ok(u32::MAX) => Err(format!(
            "{} is reserved: the kernel reads it as \"leave unchanged\"",
            u32::MAX
        )),
        Ok(id) => Ok(id),
        Err(_) => Err(format!("\"{s}\" is too large for an ID")),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use clap::Parser;
    use proptest::prelude::*;
    use shipyard_runtime::process::RlimitKind;

    use super::*;

    #[derive(Debug, Parser)]
    struct Harness {
        #[command(flatten)]
        run: RunArgs,
    }

    fn parse(args: &[&str]) -> Result<RunArgs, clap::Error> {
        let argv = std::iter::once("run").chain(args.iter().copied());
        Harness::try_parse_from(argv).map(|h| h.run)
    }

    fn spec(args: &[&str]) -> ProcessSpec {
        parse(args).unwrap().into_invocation().unwrap().spec
    }

    #[test]
    fn command_flags_are_not_ours() {
        let run = parse(&["-e", "A=1", "ls", "-l", "--env", "X", "-u", "5"]).unwrap();
        assert_eq!(run.command, ["ls", "-l", "--env", "X", "-u", "5"]);
        assert_eq!(run.env, ["A=1"]);
        assert!(run.user.is_none());
    }

    #[test]
    fn double_dash_works() {
        let run = parse(&["--", "--weird-program", "x"]).unwrap();
        assert_eq!(run.command, ["--weird-program", "x"]);
    }

    #[test]
    fn unknown_flags_before_the_command_are_errors() {
        assert!(parse(&["--bogus", "ls"]).is_err());
    }

    #[test]
    fn command_is_required() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["-e", "A=1"]).is_err());
    }

    #[test]
    fn user_forms() {
        assert_eq!(parse_user("1000"), Ok(("1000".into(), None)));
        assert_eq!(
            parse_user("1000:20"),
            Ok(("1000".into(), Some("20".into())))
        );
        assert_eq!(parse_user("0:0"), Ok(("0".into(), Some("0".into()))));
        for bad in [
            "",
            "+5",
            "-1",
            " 5",
            "5 ",
            "5:",
            ":5",
            "1:2:3",
            "4294967295",
            "5:4294967295",
            "99999999999",
        ] {
            assert!(parse_user(bad).is_err(), "{bad:?} should be rejected");
        }
        let named =
            |user: &str, group: Option<&str>| Ok((user.to_owned(), group.map(str::to_owned)));
        assert_eq!(parse_user("nobody"), named("nobody", None));
        assert_eq!(parse_user("alice:staff"), named("alice", Some("staff")));
        assert_eq!(parse_user("alice:20"), named("alice", Some("20")));
        for not_a_name in ["a b", "a,b", "a/b"] {
            assert!(parse_user(not_a_name).is_err(), "{not_a_name}");
        }
    }

    #[test]
    fn user_and_groups_are_resolved_where_the_command_runs() {
        let user = spec(&["-u", "alice:staff", "--group-add", "wheel", "true"]).user;
        assert_eq!(user.names.user.as_deref(), Some("alice"));
        assert_eq!(user.names.group.as_deref(), Some("staff"));
        assert_eq!(user.names.additional_groups, ["wheel"]);
        // A container's own /etc/group, never the host's.
        assert_eq!(user.names.memberships, cfg!(target_os = "linux"));
        let host = spec(&["--no-isolation", "-u", "alice", "true"]).user;
        assert!(!host.names.memberships);
    }

    #[test]
    fn no_user_flag_keeps_the_current_user() {
        let user = spec(&["true"]).user;
        assert_eq!(
            (user.uid, user.gid),
            (User::current().uid, User::current().gid)
        );
    }

    #[test]
    fn groups_are_deduplicated_in_order() {
        let user = spec(&[
            "--group-add",
            "20",
            "--group-add",
            "10",
            "--group-add",
            "20",
            "true",
        ])
        .user;
        assert_eq!(user.names.additional_groups, ["20", "10"]);
    }

    #[test]
    fn later_ulimit_wins() {
        let spec = spec(&[
            "--ulimit",
            "nofile=10",
            "--ulimit",
            "core=0",
            "--ulimit",
            "nofile=20:30",
            "true",
        ]);
        assert_eq!(spec.rlimits.len(), 2);
        assert_eq!(spec.rlimits[0].kind, RlimitKind::Nofile);
        assert_eq!(spec.rlimits[0].to_string(), "nofile=20:30");
    }

    #[test]
    fn bad_values_are_rejected_at_parse_time() {
        assert!(parse(&["--ulimit", "nofile=10:5", "true"]).is_err());
        assert!(parse(&["--stop-signal", "FOO", "true"]).is_err());
        assert!(parse(&["--stop-timeout", "-1", "true"]).is_err());
        assert!(parse(&["--group-add", "a:b", "true"]).is_err());
        assert!(parse(&["--sysctl", "kernel.shmmax", "true"]).is_err());
    }

    #[test]
    fn relative_workdir_fails_validation() {
        let err = parse(&["-w", "tmp", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn env_is_not_inherited_and_has_defaults() {
        let env = spec(&["-e", "A=1", "true"]).env;
        assert_eq!(env.get(OsStr::new("A")), Some(OsStr::new("1")));
        assert!(env.get(OsStr::new("PATH")).is_some());
        // HOME is set where the command runs, from the user's /etc/passwd.
        assert_eq!(env.len(), 2, "nothing from the host leaked in: {env:?}");
    }

    #[test]
    fn bare_env_key_copies_from_the_shell_or_is_skipped() {
        // HOME is set on any real host and never equals our default ("/" or
        // "/root" for root), so a match proves the value was really copied.
        let Some(host_home) = std::env::var_os("HOME") else {
            return;
        };
        let env = spec(&["-e", "HOME", "-e", "SHIPYARD_SURELY_UNSET_9F3A", "true"]).env;
        assert_eq!(env.get(OsStr::new("HOME")), Some(host_home.as_os_str()));
        assert!(env.get(OsStr::new("SHIPYARD_SURELY_UNSET_9F3A")).is_none());
    }

    #[test]
    fn env_errors_never_echo_values() {
        for entry in ["=hunter2", "A=b\0hunter2"] {
            let err = parse(&["-e", entry, "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(!format!("{err:#}").contains("hunter2"), "{entry}: {err:#}");
        }
    }

    #[test]
    fn malformed_bare_env_keys_are_rejected_not_skipped() {
        for entry in ["", "TOKEN hunter2"] {
            let err = parse(&["-e", entry, "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(!format!("{err:#}").contains("hunter2"), "{err:#}");
        }
    }

    #[test]
    fn stop_policy() {
        let stop = parse(&["--stop-signal", "INT", "--stop-timeout", "3", "true"])
            .unwrap()
            .into_invocation()
            .unwrap()
            .stop;
        assert_eq!(stop.signal, "SIGINT".parse().unwrap());
        assert_eq!(stop.timeout, Duration::from_secs(3));
        let default = parse(&["true"]).unwrap().into_invocation().unwrap().stop;
        assert_eq!(default, StopPolicy::default());
    }

    #[test]
    fn capability_flags_build_dockers_rule() {
        let security = |args: &[&str]| parse(args).unwrap().security().unwrap();
        let rule = |args: &[&str]| match security(args).capabilities {
            Capabilities::Default { kept, added } => (kept, added),
            other => panic!("{other:?}"),
        };
        let cap = |name: &str| name.parse::<Capability>().unwrap();
        assert_eq!(
            rule(&["true"]),
            (CapabilitySet::DOCKER_DEFAULT, CapabilitySet::EMPTY)
        );
        let (kept, added) = rule(&["--cap-drop", "chown", "--cap-add", "CAP_SYS_ADMIN", "true"]);
        assert!(!kept.contains(cap("CHOWN")) && kept.contains(cap("KILL")));
        assert_eq!(added.iter().collect::<Vec<_>>(), [cap("SYS_ADMIN")]);
        assert_eq!(rule(&["--cap-drop", "ALL", "true"]).0, CapabilitySet::EMPTY);
        assert_eq!(rule(&["--cap-add", "all", "true"]).1, CapabilitySet::ALL);
        // Dropping everything, then adding one: just that one.
        let (kept, added) = rule(&["--cap-drop", "ALL", "--cap-add", "KILL", "true"]);
        assert!(kept.is_empty() && added.contains(cap("KILL")));
        assert!(security(&["true"]).no_new_privileges);
        for (option, on) in [
            ("no-new-privileges", true),
            ("no-new-privileges=true", true),
            ("no-new-privileges:false", false),
            ("no-new-privileges=false", false),
        ] {
            assert_eq!(
                security(&["--security-opt", option, "true"]).no_new_privileges,
                on,
                "{option}"
            );
        }
    }

    #[test]
    fn seccomp_options_choose_the_filter() {
        let seccomp = |args: &[&str]| parse(args).unwrap().security().map(|s| s.seccomp);
        assert_eq!(seccomp(&["true"]).unwrap(), Seccomp::Default);
        assert_eq!(
            seccomp(&["--security-opt", "seccomp=unconfined", "true"]).unwrap(),
            Seccomp::Unconfined
        );
        let dir = std::env::temp_dir().join(format!("shipyard-seccomp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, json: &str| {
            let path = dir.join(name);
            std::fs::write(&path, json).unwrap();
            format!("seccomp={}", path.display())
        };
        let good = file(
            "good.json",
            r#"{"defaultAction": "SCMP_ACT_ERRNO",
                "syscalls": [{"names": ["read"], "action": "SCMP_ACT_ALLOW"}]}"#,
        );
        let Seccomp::Profile(profile) = seccomp(&["--security-opt", &good, "true"]).unwrap() else {
            panic!("not a profile");
        };
        assert_eq!(profile.syscalls[0].names, ["read"]);
        // The last one wins, as with Docker.
        assert_eq!(
            seccomp(&[
                "--security-opt",
                &good,
                "--security-opt",
                "seccomp:unconfined",
                "true"
            ])
            .unwrap(),
            Seccomp::Unconfined
        );
        // Mistakes name the file, and what's wrong.
        for (name, json, wanted) in [
            (
                "typo.json",
                r#"{"defaultAction": "SCMP_ACT_ERRNO", "syscalls": [{"names": ["raed"], "action": "SCMP_ACT_ALLOW"}]}"#,
                "\"raed\" isn't a system call",
            ),
            (
                "field.json",
                r#"{"defaultAction": "SCMP_ACT_ERRNO", "allowAll": true}"#,
                "unknown field `allowAll`",
            ),
            ("broken.json", "{", "EOF while parsing"),
        ] {
            let option = file(name, json);
            let err = format!(
                "{:#}",
                seccomp(&["--security-opt", &option, "true"]).unwrap_err()
            );
            assert!(err.contains(name) && err.contains(wanted), "{err}");
        }
        let missing = format!(
            "{:#}",
            seccomp(&["--security-opt", "seccomp=/no/such.json", "true"]).unwrap_err()
        );
        assert!(
            missing.contains("seccomp profile /no/such.json"),
            "{missing}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(parse(&["--security-opt", "seccomp=", "true"]).is_err());
        assert!(parse(&["--security-opt", "seccomp", "true"]).is_err());
        assert!(
            parse(&[
                "--no-isolation",
                "--security-opt",
                "seccomp=unconfined",
                "true"
            ])
            .is_err()
        );
        assert!(parse(&["--bundle", ".", "--security-opt", "seccomp=unconfined"]).is_err());
    }

    #[test]
    fn capability_flags_are_checked() {
        assert!(parse(&["--cap-add", "FLY", "true"]).is_err());
        assert!(parse(&["--security-opt", "apparmor=x", "true"]).is_err());
        assert!(parse(&["--security-opt", "no-new-privileges=maybe", "true"]).is_err());
        for flag in ["--cap-add", "--cap-drop"] {
            assert!(
                parse(&["--no-isolation", flag, "KILL", "true"]).is_err(),
                "{flag}"
            );
            assert!(parse(&["--bundle", ".", flag, "KILL"]).is_err(), "{flag}");
        }
        let both = parse(&["--cap-add", "KILL", "--cap-drop", "kill", "true"]).unwrap();
        assert!(
            both.security()
                .unwrap_err()
                .to_string()
                .contains("both name CAP_KILL")
        );
        if !cfg!(target_os = "linux") {
            let err = parse(&["--cap-add", "KILL", "true"])
                .unwrap()
                .into_invocation()
                .unwrap_err();
            assert!(err.to_string().contains("--cap-add needs Linux"), "{err}");
        }
    }

    #[test]
    fn isolation_is_the_default_on_linux_only() {
        let isolation = |args: &[&str]| parse(args).unwrap().into_invocation().unwrap().isolation;
        assert_eq!(
            isolation(&["true"]).is_some(),
            cfg!(target_os = "linux"),
            "isolated by default exactly on Linux"
        );
        assert_eq!(isolation(&["--no-isolation", "true"]), None);
        if cfg!(target_os = "linux") {
            let named = isolation(&["--hostname", "box", "true"]).unwrap();
            assert_eq!(named.hostname.as_deref(), Some("box"));
        }
    }

    #[test]
    fn a_root_filesystem_is_canonical_and_checked() {
        let isolation = |args: &[&str]| parse(args).unwrap().into_invocation();
        // Contradictions, whatever the system.
        assert!(parse(&["--no-isolation", "--rootfs", "/", "true"]).is_err());
        assert!(parse(&["--read-only", "true"]).is_err(), "needs --rootfs");
        let missing = isolation(&["--rootfs", "/no/such/dir", "true"]).unwrap_err();
        assert!(format!("{missing:#}").contains("--rootfs"), "{missing:#}");
        if cfg!(target_os = "linux") {
            assert!(
                format!("{missing:#}").contains("/no/such/dir"),
                "{missing:#}"
            );
            let dir = std::env::temp_dir();
            let given = dir.join(".").join("");
            let invocation =
                isolation(&["--rootfs", given.to_str().unwrap(), "--read-only", "true"]).unwrap();
            let root = invocation.isolation.unwrap().root.unwrap();
            assert_eq!(root.path, std::fs::canonicalize(dir).unwrap());
            assert!(root.read_only);
        }
    }

    #[test]
    fn a_bundle_is_the_whole_description() {
        assert!(parse(&["--bundle", "/b"]).is_ok(), "no COMMAND needed");
        for conflicting in [
            &["--bundle", "/b", "true"][..],
            &["--bundle", "/b", "-e", "A=1"],
            &["--bundle", "/b", "-w", "/tmp"],
            &["--bundle", "/b", "--rootfs", "/r"],
            &["--bundle", "/b", "--no-isolation"],
        ] {
            assert!(parse(conflicting).is_err(), "{conflicting:?}");
        }
        assert!(parse(&[]).is_err(), "a COMMAND or a bundle");
        let missing = parse(&["--bundle", "/no/such/bundle"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        let expected = if cfg!(target_os = "linux") {
            "/no/such/bundle/config.json"
        } else {
            "--bundle"
        };
        assert!(format!("{missing:#}").contains(expected), "{missing:#}");
    }

    #[test]
    fn hostname_problems_are_usage_errors() {
        // Both: together they contradict each other.
        assert!(parse(&["--no-isolation", "--hostname", "box", "true"]).is_err());
        // An invalid name, or (macOS) no namespaces to give it to.
        let err = parse(&["--hostname", "bad_name", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(format!("{err:#}").contains("hostname"), "{err:#}");
    }

    proptest! {
        #[test]
        fn user_parser_never_panics(s in any::<String>()) {
            let _ = parse_user(&s);
        }

        #[test]
        fn valid_ids_round_trip(uid in 0..u32::MAX, gid in 0..u32::MAX) {
            prop_assert_eq!(
                parse_user(&format!("{uid}:{gid}")),
                Ok((uid.to_string(), Some(gid.to_string())))
            );
        }
    }
}

//! `shipyard run`: flags in, a validated `ProcessSpec` out.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, value_parser};
use shipyard_runtime::process::{
    DEFAULT_STOP_TIMEOUT_SECS, Env, ProcessSpec, Rlimit, Signal, StopPolicy, User,
};

#[derive(Debug, Args)]
pub(crate) struct RunArgs {
    /// Set an environment variable, or copy KEY from your shell if no value is given.
    #[arg(short, long, value_name = "KEY[=VALUE]", value_parser = value_parser!(OsString))]
    env: Vec<OsString>,

    /// Working directory inside the container (absolute path).
    #[arg(short, long, value_name = "DIR", default_value = "/")]
    workdir: PathBuf,

    /// Run as this numeric user and group (group defaults to 0).
    #[arg(short, long, value_name = "UID[:GID]", value_parser = parse_user)]
    user: Option<(u32, Option<u32>)>,

    /// Add a supplementary group (numeric).
    #[arg(long, value_name = "GID", value_parser = parse_id)]
    group_add: Vec<u32>,

    /// Set a resource limit, e.g. nofile=1024:2048 or core=0.
    #[arg(long, value_name = "NAME=SOFT[:HARD]")]
    ulimit: Vec<Rlimit>,

    /// Signal sent to stop the command.
    #[arg(long, value_name = "SIGNAL", default_value = "SIGTERM")]
    stop_signal: Signal,

    /// Seconds to wait after the stop signal before killing the command.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_STOP_TIMEOUT_SECS)]
    stop_timeout: u64,

    /// The command to run, followed by its arguments.
    #[arg(
        required = true,
        trailing_var_arg = true,
        value_name = "COMMAND",
        value_parser = value_parser!(OsString),
    )]
    command: Vec<OsString>,
}

impl RunArgs {
    /// Builds the process spec and stop policy, and validates the spec.
    pub(crate) fn into_spec(self) -> Result<(ProcessSpec, StopPolicy)> {
        let mut user = match self.user {
            Some((uid, gid)) => User {
                uid,
                gid: gid.unwrap_or(0),
                umask: None,
                additional_gids: Vec::new(),
            },
            None => User::current(),
        };
        user.additional_gids = dedup(self.group_add);

        let mut env = Env::new();
        for entry in &self.env {
            add_env(&mut env, entry)?;
        }
        env.fill_defaults(user.uid);

        let spec = ProcessSpec {
            args: self.command,
            env,
            cwd: self.workdir,
            user,
            rlimits: last_wins(self.ulimit),
        };
        spec.validate()?;

        let stop = StopPolicy {
            signal: self.stop_signal,
            timeout: Duration::from_secs(self.stop_timeout),
        };
        Ok((spec, stop))
    }
}

pub(crate) fn run(args: RunArgs) -> Result<ExitCode> {
    let (spec, stop) = args.into_spec()?;
    tracing::debug!(?spec, ?stop, "validated process spec");
    bail!("the command and its settings are valid, but this version can't run commands yet")
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
        tracing::debug!(key = %key.to_string_lossy(), "--env KEY not set in shell, skipped");
        return Ok(());
    };
    env.set(key, &value).with_context(context)
}

/// Removes repeated groups, keeping the first occurrence of each.
fn dedup(gids: Vec<u32>) -> Vec<u32> {
    let mut seen = HashSet::new();
    gids.into_iter().filter(|gid| seen.insert(*gid)).collect()
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

/// Parses `UID` or `UID:GID`, numbers only.
fn parse_user(s: &str) -> Result<(u32, Option<u32>), String> {
    match s.split_once(':') {
        Some((uid, gid)) => Ok((parse_user_id(uid)?, Some(parse_user_id(gid)?))),
        None => Ok((parse_user_id(s)?, None)),
    }
}

/// Like [`parse_id`], but explains that names aren't accepted yet. (Names need
/// the container's `/etc/passwd`, which arrives with root filesystems.)
fn parse_user_id(s: &str) -> Result<u32, String> {
    let looks_like_name = s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_');
    if looks_like_name {
        return Err(format!(
            "\"{s}\" is a name; user and group names are not supported yet, \
             use a numeric UID[:GID]"
        ));
    }
    parse_id(s)
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
        parse(args).unwrap().into_spec().unwrap().0
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
        assert_eq!(parse_user("1000"), Ok((1000, None)));
        assert_eq!(parse_user("1000:20"), Ok((1000, Some(20))));
        assert_eq!(parse_user("0:0"), Ok((0, Some(0))));
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
        let err = parse_user("nobody").unwrap_err();
        assert!(err.contains("not supported yet"), "{err}");
        // Malformed numbers are not names, so they don't get the names hint.
        for not_a_name in ["-1", "+5", "1.5"] {
            let err = parse_user(not_a_name).unwrap_err();
            assert!(!err.contains("not supported yet"), "{not_a_name}: {err}");
        }
    }

    #[test]
    fn user_without_group_gets_gid_0() {
        let user = spec(&["-u", "1000", "true"]).user;
        assert_eq!((user.uid, user.gid), (1000, 0));
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
        assert_eq!(user.additional_gids, [20, 10]);
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
        assert!(parse(&["--group-add", "wheel", "true"]).is_err());
    }

    #[test]
    fn relative_workdir_fails_validation() {
        let err = parse(&["-w", "tmp", "true"])
            .unwrap()
            .into_spec()
            .unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn env_is_not_inherited_and_has_defaults() {
        let env = spec(&["-e", "A=1", "true"]).env;
        assert_eq!(env.get(OsStr::new("A")), Some(OsStr::new("1")));
        assert!(env.get(OsStr::new("PATH")).is_some());
        assert!(env.get(OsStr::new("HOME")).is_some());
        assert_eq!(env.len(), 3, "nothing from the host leaked in: {env:?}");
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
                .into_spec()
                .unwrap_err();
            assert!(!format!("{err:#}").contains("hunter2"), "{entry}: {err:#}");
        }
    }

    #[test]
    fn malformed_bare_env_keys_are_rejected_not_skipped() {
        for entry in ["", "TOKEN hunter2"] {
            let err = parse(&["-e", entry, "true"])
                .unwrap()
                .into_spec()
                .unwrap_err();
            assert!(!format!("{err:#}").contains("hunter2"), "{err:#}");
        }
    }

    #[test]
    fn stop_policy() {
        let (_, stop) = parse(&["--stop-signal", "INT", "--stop-timeout", "3", "true"])
            .unwrap()
            .into_spec()
            .unwrap();
        assert_eq!(stop.signal, "SIGINT".parse().unwrap());
        assert_eq!(stop.timeout, Duration::from_secs(3));
        let (_, default) = parse(&["true"]).unwrap().into_spec().unwrap();
        assert_eq!(default, StopPolicy::default());
    }

    proptest! {
        #[test]
        fn user_parser_never_panics(s in any::<String>()) {
            let _ = parse_user(&s);
        }

        #[test]
        fn valid_ids_round_trip(uid in 0..u32::MAX, gid in 0..u32::MAX) {
            prop_assert_eq!(parse_user(&format!("{uid}:{gid}")), Ok((uid, Some(gid))));
        }
    }
}

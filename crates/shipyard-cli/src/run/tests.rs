use std::ffi::OsStr;

use clap::Parser;
use std::time::Duration;

use shipyard_runtime::isolation::Namespace;
use shipyard_runtime::process::{ProcessSpec, RlimitKind, StopPolicy, User};

use super::commands::is_host_command;
use super::*;

impl RunArgs {
    /// Checked and finished without an image, as a run without one is.
    fn into_invocation(self) -> anyhow::Result<Invocation> {
        let engine = crate::reporter::engine();
        match shipyard_engine::run::check(&engine, self.into_request()?)? {
            Checked::Bundle(invocation) => Ok(*invocation),
            Checked::Command(planned) => Ok(planned.finish(&engine, None, None)?.0),
        }
    }
}
use shipyard_engine::run::{Checked, Invocation, RunRequest};
use std::os::unix::ffi::OsStringExt as _;

#[derive(Debug, Parser)]
#[command(disable_help_flag = true)]
struct Harness {
    #[command(flatten)]
    run: RunArgs,
}

fn parse(args: &[&str]) -> Result<RunArgs, clap::Error> {
    let argv = std::iter::once("run").chain(args.iter().copied());
    Harness::try_parse_from(argv).map(|h| h.run)
}

fn spec(args: &[&str]) -> ProcessSpec {
    parse(args)
        .unwrap()
        .into_invocation()
        .unwrap()
        .spec()
        .clone()
}

/// The request these arguments make, or the usage error.
fn request(args: &[&str]) -> Result<RunRequest, clap::Error> {
    parse(args)?.into_request()
}

/// The health flags as the engine takes them: `--health-cmd`'s check
/// (tuned); the others alone, tuning the image's (no test);
/// `--no-healthcheck`, off (refused with any other); nothing asked.
#[test]
fn health_flags_ask_as_dockers() {
    use shipyard_engine::health::HealthCheck;
    let health = |args: &[&str]| {
        let mut all = args.to_vec();
        all.push("alpine");
        request(&all).map(|run| run.health)
    };
    assert_eq!(health(&[]).unwrap(), None);
    let full = health(&["--health-cmd", "true", "--health-retries", "2"])
        .unwrap()
        .unwrap();
    assert_eq!((full.test.len(), full.retries), (2, 2));
    let tuned = health(&["--health-interval", "2s"]).unwrap().unwrap();
    // A millisecond is the least a flag can say (`500us` isn't a
    // duration here): kept as given. (Under one is refused by the
    // engine's `flag_millis`, tested there.)
    assert_eq!(
        health(&["--health-timeout", "1ms"])
            .unwrap()
            .unwrap()
            .timeout_ms,
        1
    );
    assert!(tuned.test.is_empty());
    assert_eq!(tuned.interval_ms, 2000);
    assert_eq!(
        health(&["--no-healthcheck"]).unwrap(),
        Some(HealthCheck::disabled())
    );
    assert!(health(&["--no-healthcheck", "--health-retries", "1"]).is_err());
}

/// The first argument is the image, unless a flag says there's none;
/// everything after it is the command's.
#[test]
fn the_image_comes_first() {
    let run = request(&["-e", "A=1", "alpine", "ls", "--rm", "-e", "B"]).unwrap();
    assert_eq!(run.image.as_deref(), Some("alpine"));
    assert_eq!(
        run.command,
        ["ls", "--rm", "-e", "B"],
        "options after it are the command's"
    );
    assert_eq!(run.env, ["A=1"]);
    let alone = request(&["alpine"]).unwrap();
    assert!(alone.command.is_empty(), "the image's own command");
    for (flags, first) in [
        (&["--no-image"][..], "ls"),
        (&["--rootfs", "/r"][..], "ls"),
        (&["--no-isolation"][..], "ls"),
    ] {
        let run = request(&[flags, &["ls", "-l"]].concat()).unwrap();
        assert!(run.image.is_none(), "{flags:?}");
        assert_eq!(run.command, [first, "-l"], "{flags:?}");
    }
    let alias = request(&["--image", "alpine", "ls"]).unwrap();
    assert_eq!(alias.image.as_deref(), Some("alpine"), "--image, hidden");
    assert_eq!(alias.command, ["ls"]);
    let dashed = request(&["alpine", "--", "-x"]).unwrap();
    assert_eq!(
        dashed.command,
        ["--", "-x"],
        "after the image, `--` is the command's too"
    );
    let before = request(&["--", "alpine", "ls"]).unwrap();
    assert_eq!(
        before.image.as_deref(),
        Some("alpine"),
        "`--` before the image"
    );
    assert_eq!(before.command, ["ls"]);
    let both = request(&["--image", "alpine", "busybox", "ls"]).unwrap();
    assert_eq!(
        both.command,
        ["busybox", "ls"],
        "with --image, all are the command"
    );
    let mut args = parse(&[]).unwrap();
    args.args = vec![OsString::from_vec(vec![b'a', 0xff]), "true".into()];
    assert!(
        args.into_request()
            .unwrap_err()
            .to_string()
            .contains("isn't UTF-8")
    );
}

#[test]
fn a_bare_word_on_the_path_is_a_host_command() {
    assert!(is_host_command("sh"), "/bin/sh, everywhere");
    assert!(!is_host_command("alpine:3.20"));
    assert!(!is_host_command("ghcr.io/owner/app"));
    assert!(!is_host_command("no-such-program-anywhere"));
}

/// The flags that need an image: with a positional one they're fine,
/// without one each is a usage error (exit 125, as clap's).
#[test]
fn flags_that_need_an_image() {
    for flag in [
        &["-P"][..],
        &["--entrypoint", "x"],
        &["--pull", "always"],
        &["--read-only"],
    ] {
        assert!(
            request(&[flag, &["alpine", "true"]].concat()).is_ok(),
            "{flag:?}"
        );
        let refused = request(&[&["--no-image"], flag, &["true"]].concat()).unwrap_err();
        assert!(
            refused.to_string().contains("needs an image"),
            "{flag:?}: {refused}"
        );
    }
    assert!(request(&["--rootfs", "/r", "--read-only", "true"]).is_ok());
    let none = request(&[]).unwrap_err();
    assert!(none.to_string().contains("an IMAGE is needed"), "{none}");
    let no_command = request(&["--no-image"]).unwrap_err();
    assert!(
        no_command.to_string().contains("a COMMAND is needed"),
        "{no_command}"
    );
    assert!(
        parse(&["--bundle", "/b", "--entrypoint", "x"]).is_err(),
        "a clap conflict"
    );
    assert!(
        request(&["--bundle", "/b", "true"]).is_err(),
        "a bundle takes no command"
    );
    assert!(parse(&["--no-image", "--image", "alpine", "true"]).is_err());
    assert!(parse(&["--no-image", "--rootfs", "/r", "true"]).is_err());
}

#[test]
fn a_volume_needs_a_root_filesystem() {
    let err = parse(&["--no-image", "--volume", "data:/data", "true"])
        .unwrap()
        .into_invocation()
        .unwrap_err();
    let expected = if cfg!(target_os = "linux") {
        "needs a root filesystem"
    } else {
        "--volume needs Linux"
    };
    assert!(err.to_string().contains(expected), "{err}");
}

#[test]
fn command_flags_are_not_ours() {
    let run = parse(&[
        "--no-image",
        "-e",
        "A=1",
        "ls",
        "-l",
        "--env",
        "X",
        "-u",
        "5",
    ])
    .unwrap();
    assert_eq!(run.args, ["ls", "-l", "--env", "X", "-u", "5"]);
    assert_eq!(run.env, ["A=1"]);
    assert!(run.user.is_none());
}

#[test]
fn double_dash_works() {
    let run = parse(&["--no-image", "--", "--weird-program", "x"]).unwrap();
    assert_eq!(run.args, ["--weird-program", "x"]);
}

#[test]
fn unknown_flags_before_the_command_are_errors() {
    assert!(parse(&["--no-image", "--bogus", "ls"]).is_err());
}

#[test]
fn command_is_required() {
    assert!(request(&["--no-image", "-e", "A=1"]).is_err());
}

#[test]
fn user_and_groups_are_resolved_where_the_command_runs() {
    let user = spec(&[
        "--no-image",
        "-u",
        "alice:staff",
        "--group-add",
        "wheel",
        "true",
    ])
    .user;
    assert_eq!(user.names.user.as_deref(), Some("alice"));
    assert_eq!(user.names.group.as_deref(), Some("staff"));
    assert_eq!(user.names.additional_groups, ["wheel"]);
    // A container's own /etc/group, never the host's.
    assert_eq!(user.names.memberships, cfg!(target_os = "linux"));
    let host = spec(&["--no-isolation", "-u", "alice", "true"]).user;
    assert!(!host.names.memberships);
}

#[test]
fn without_a_user_flag_isolated_runs_are_root_inside() {
    let ids = |user: User| (user.uid, user.gid);
    let current = ids(User::current());
    assert_eq!(ids(spec(&["--no-isolation", "true"]).user), current);
    // Isolated (on Linux): root inside, whoever runs shipyard.
    let isolated = if cfg!(target_os = "linux") {
        (0, 0)
    } else {
        current
    };
    assert_eq!(ids(spec(&["--no-image", "true"]).user), isolated);
}

#[test]
fn groups_are_deduplicated_in_order() {
    let user = spec(&[
        "--no-image",
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
        "--no-image",
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
    assert!(parse(&["--no-image", "--ulimit", "nofile=10:5", "true"]).is_err());
    assert!(parse(&["--no-image", "--stop-signal", "FOO", "true"]).is_err());
    assert!(parse(&["--no-image", "--stop-timeout", "-1", "true"]).is_err());
    assert!(parse(&["--no-image", "--group-add", "a:b", "true"]).is_err());
    assert!(parse(&["--no-image", "--sysctl", "kernel.shmmax", "true"]).is_err());
}

#[test]
fn relative_workdir_fails_validation() {
    let err = parse(&["--no-image", "-w", "tmp", "true"])
        .unwrap()
        .into_invocation()
        .unwrap_err();
    assert!(err.to_string().contains("absolute"), "{err}");
}

#[test]
fn env_is_not_inherited_and_has_defaults() {
    let env = spec(&["--no-image", "-e", "A=1", "true"]).env;
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
    let env = spec(&[
        "--no-image",
        "-e",
        "HOME",
        "-e",
        "SHIPYARD_SURELY_UNSET_9F3A",
        "true",
    ])
    .env;
    assert_eq!(env.get(OsStr::new("HOME")), Some(host_home.as_os_str()));
    assert!(env.get(OsStr::new("SHIPYARD_SURELY_UNSET_9F3A")).is_none());
}

#[test]
fn env_errors_never_echo_values() {
    for entry in ["=hunter2", "A=b\0hunter2"] {
        let err = parse(&["--no-image", "-e", entry, "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(!format!("{err:#}").contains("hunter2"), "{entry}: {err:#}");
    }
}

#[test]
fn malformed_bare_env_keys_are_rejected_not_skipped() {
    for entry in ["", "TOKEN hunter2"] {
        let err = parse(&["--no-image", "-e", entry, "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(!format!("{err:#}").contains("hunter2"), "{err:#}");
    }
}

#[test]
fn stop_policy() {
    let stop = parse(&[
        "--no-image",
        "--stop-signal",
        "INT",
        "--stop-timeout",
        "3",
        "true",
    ])
    .unwrap()
    .into_invocation()
    .unwrap()
    .stop();
    assert_eq!(stop.signal, "SIGINT".parse().unwrap());
    assert_eq!(stop.timeout, Duration::from_secs(3));
    let default = parse(&["--no-image", "true"])
        .unwrap()
        .into_invocation()
        .unwrap()
        .stop();
    assert_eq!(default, StopPolicy::default());
}

/// A request with every flag given.
fn every_flag() -> RunRequest {
    parse(&[
        "-i",
        "-t",
        "-e",
        "A=1",
        "-w",
        "/work",
        "-u",
        "5:6",
        "--group-add",
        "7",
        "--ulimit",
        "nofile=1:2",
        "--oom-score-adj",
        "3",
        "--stop-signal",
        "SIGINT",
        "--stop-timeout",
        "4",
        "--hostname",
        "box",
        "--domainname",
        "dom",
        "--sysctl",
        "kernel.shmmax=1",
        "--network",
        "app",
        "--name",
        "web",
        "--network-alias",
        "www",
        "--ip",
        "10.0.0.2",
        "--ip6",
        "fd00::2",
        "--mac-address",
        "02:42:0a:00:00:02",
        "--add-host",
        "db:10.0.0.3",
        "--dns",
        "1.1.1.1",
        "--dns-search",
        "example",
        "--dns-option",
        "ndots:2",
        "-p",
        "8080:80",
        "-P",
        "--entrypoint",
        "/e",
        "--pull",
        "never",
        "--read-only",
        "--cap-add",
        "KILL",
        "--cap-drop",
        "CHOWN",
        "--security-opt",
        "no-new-privileges=false",
        "--uidmap",
        "0:1:1",
        "--gidmap",
        "0:2:1",
        "-m",
        "64m",
        "--volume",
        "v:/v",
        "--device",
        "/dev/null",
        "--device-cgroup-rule",
        "c 1:3 r",
        "alpine",
        "cmd",
        "arg",
    ])
    .unwrap()
    .into_request()
    .unwrap()
}

/// Every flag reaches the engine's request, as given (`into_request`'s
/// mapping, field by field): the process and the container's names.
#[test]
fn every_flag_reaches_the_request() {
    let request = every_flag();
    assert!(request.streams.interactive && request.streams.tty);
    assert_eq!(request.env, ["A=1"]);
    assert_eq!(
        request.workdir.as_deref(),
        Some(std::path::Path::new("/work"))
    );
    assert_eq!(request.user, Some(("5".into(), Some("6".into()))));
    assert_eq!(request.group_add, ["7"]);
    assert_eq!(request.ulimit.len(), 1);
    assert_eq!(request.oom_score_adj, Some(3));
    assert_eq!(request.stop_signal, Signal::INT);
    assert_eq!(request.stop_timeout, 4);
    assert_eq!(request.hostname.as_deref(), Some("box"));
    assert_eq!(request.domainname.as_deref(), Some("dom"));
    assert_eq!(request.sysctl.len(), 1);
    assert_eq!(request.network, [NetworkMode::bridge("app")]);
    assert_eq!(request.name.as_deref(), Some("web"));
    assert_eq!(request.network_alias, ["www"]);
    assert!(request.ip.is_some() && request.ip6.is_some() && request.mac_address.is_some());
    assert_eq!(request.add_host.len(), 1);
    assert_eq!(request.dns.len(), 1);
}

/// The same: the network, the root, security, the rest.
#[test]
fn every_other_flag_reaches_the_request() {
    let request = every_flag();
    assert_eq!(request.dns_search, ["example"]);
    assert_eq!(request.dns_option, ["ndots:2"]);
    assert_eq!(request.publish.len(), 1);
    assert!(request.publish_all);
    assert_eq!(request.image.as_deref(), Some("alpine"));
    assert_eq!(
        request.entrypoint.as_deref(),
        Some(std::ffi::OsStr::new("/e"))
    );
    assert_eq!(
        request.pull,
        Some(shipyard_engine::image::PullPolicy::Never)
    );
    assert!(request.read_only);
    assert_eq!(request.cap_add.len(), 1);
    assert_eq!(request.cap_drop.len(), 1);
    assert_eq!(request.security_opt, [SecurityOpt::NoNewPrivileges(false)]);
    assert_eq!(request.uidmap.len(), 1);
    assert_eq!(request.gidmap.len(), 1);
    assert_eq!(request.limits.first_given, Some("--memory"));
    assert!(request.limits.given());
    assert_eq!(request.volume, ["v:/v"]);
    assert_eq!(request.device.len(), 1);
    assert_eq!(request.device_cgroup_rule.len(), 1);
    assert_eq!(request.command, ["cmd", "arg"]);
}

/// A root filesystem, a bundle and no isolation reach it too.
#[test]
fn the_root_bundle_and_isolation_reach_the_request() {
    let rootfs = parse(&["--rootfs", "/r", "--no-isolation", "true"]);
    assert!(rootfs.is_err(), "clap's: they can't go together");
    let rootfs = request(&["--rootfs", "/r", "true"]).unwrap();
    assert_eq!(rootfs.rootfs.as_deref(), Some(std::path::Path::new("/r")));
    let plain = parse(&["--no-isolation", "--bundle", "/b"]);
    assert!(plain.is_err());
    let bundle = request(&["--bundle", "/b"]).unwrap();
    assert_eq!(bundle.bundle.as_deref(), Some(std::path::Path::new("/b")));
    assert!(request(&["--no-isolation", "true"]).unwrap().no_isolation);
}

/// Docker's spellings of capabilities and security options, as the
/// engine takes them.
#[test]
fn capability_and_security_spellings() {
    use shipyard_runtime::security::Capability;
    let cap = |name: &str| name.parse::<Capability>().unwrap();
    assert_eq!("ALL".parse::<CapabilityArg>(), Ok(CapabilityArg::All));
    assert_eq!("all".parse::<CapabilityArg>(), Ok(CapabilityArg::All));
    assert_eq!(
        "chown".parse::<CapabilityArg>(),
        Ok(CapabilityArg::One(cap("CHOWN")))
    );
    assert_eq!(
        "CAP_SYS_ADMIN".parse::<CapabilityArg>(),
        Ok(CapabilityArg::One(cap("SYS_ADMIN")))
    );
    for (option, on) in [
        ("no-new-privileges", true),
        ("no-new-privileges=true", true),
        ("no-new-privileges:false", false),
        ("no-new-privileges=false", false),
    ] {
        assert_eq!(
            option.parse::<SecurityOpt>(),
            Ok(SecurityOpt::NoNewPrivileges(on)),
            "{option}"
        );
    }
    assert_eq!(
        "seccomp=/p.json".parse::<SecurityOpt>(),
        Ok(SecurityOpt::SeccompProfile("/p.json".into()))
    );
}

#[test]
fn seccomp_options_are_checked() {
    assert!(parse(&["--no-image", "--security-opt", "seccomp=", "true"]).is_err());
    assert!(parse(&["--no-image", "--security-opt", "seccomp", "true"]).is_err());
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
    assert_eq!(
        parse(&["--no-image", "--security-opt", "seccomp:unconfined", "true"])
            .unwrap()
            .security_opt,
        [SecurityOpt::SeccompUnconfined]
    );
}

#[test]
fn id_map_flags_parse_and_conflict() {
    assert!(
        parse(&["--no-image", "--gidmap", "0:1:1", "true"]).is_err(),
        "--gidmap needs --uidmap"
    );
    assert!(parse(&["--no-image", "--uidmap", "0:1", "true"]).is_err());
    assert!(parse(&["--no-isolation", "--uidmap", "0:1:1", "true"]).is_err());
    assert!(parse(&["--bundle", ".", "--uidmap", "0:1:1"]).is_err());
    if !cfg!(target_os = "linux") {
        let err = parse(&["--no-image", "--uidmap", "0:100000:65536", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(err.to_string().contains("--uidmap needs Linux"), "{err}");
    }
}

#[test]
fn the_oom_score_adjustment_is_checked() {
    if !cfg!(target_os = "linux") {
        let err = parse(&["--no-image", "--oom-score-adj", "5", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(err.to_string().contains("needs Linux"), "{err}");
        return;
    }
    assert_eq!(
        spec(&["--no-image", "--oom-score-adj", "-500", "true"]).oom_score_adj,
        Some(-500)
    );
    let err = parse(&["--no-image", "--oom-score-adj", "1001", "true"])
        .unwrap()
        .into_invocation()
        .unwrap_err();
    assert!(err.to_string().contains("between -1000 and 1000"), "{err}");
    assert!(parse(&["--bundle", ".", "--oom-score-adj", "5"]).is_err());
}

#[test]
fn limit_flags_need_a_container() {
    for limit in [
        &["--memory", "64m"][..],
        &["--pids-limit", "5"],
        &["--cpus", "1"],
    ] {
        let args = [&["--no-isolation"], limit, &["true"]].concat();
        assert!(parse(&args).is_err(), "{args:?}");
        let args = [&["--bundle", "."], limit].concat();
        assert!(parse(&args).is_err(), "{args:?}");
    }
    let limited = parse(&["--no-image", "-m", "64m", "-c", "512", "true"]).unwrap();
    assert_eq!(limited.args, ["true"], "-m and -c are ours");
    if !cfg!(target_os = "linux") {
        let err = limited.into_invocation().unwrap_err();
        assert!(
            err.to_string().contains("--memory needs Linux cgroups"),
            "{err}"
        );
    }
}

#[test]
fn capability_flags_are_checked() {
    assert!(parse(&["--no-image", "--cap-add", "FLY", "true"]).is_err());
    assert!(parse(&["--no-image", "--security-opt", "apparmor=x", "true"]).is_err());
    assert!(
        parse(&[
            "--no-image",
            "--security-opt",
            "no-new-privileges=maybe",
            "true"
        ])
        .is_err()
    );
    for flag in ["--cap-add", "--cap-drop"] {
        assert!(
            parse(&["--no-isolation", flag, "KILL", "true"]).is_err(),
            "{flag}"
        );
        assert!(parse(&["--bundle", ".", flag, "KILL"]).is_err(), "{flag}");
    }
    if !cfg!(target_os = "linux") {
        let err = parse(&["--no-image", "--cap-add", "KILL", "true"])
            .unwrap()
            .into_invocation()
            .unwrap_err();
        assert!(err.to_string().contains("--cap-add needs Linux"), "{err}");
    }
}

/// `--network container:NAME`: another container's network, alone, with
/// nothing of this one's in it.
#[test]
fn another_container_s_network() {
    // Another container's, by name or ID.
    assert_eq!(
        parse_network("container:db"),
        Ok(NetworkMode::Container("db".to_owned()))
    );
    assert!(parse_network("container:").is_err());
    // `--ipc`: its own, or another container's.
    for (given, ipc) in [
        ("private", shipyard_engine::run::IpcMode::Private),
        ("shareable", shipyard_engine::run::IpcMode::Private),
        (
            "container:db",
            shipyard_engine::run::IpcMode::Container("db".to_owned()),
        ),
    ] {
        assert_eq!(
            shipyard_engine::run::IpcMode::parse(given),
            Ok(ipc),
            "{given}"
        );
    }
    for refused in ["host", "none", "container:"] {
        assert!(
            shipyard_engine::run::IpcMode::parse(refused).is_err(),
            "{refused}"
        );
    }
    if !cfg!(target_os = "linux") {
        return;
    }
    let isolation = |args: &[&str]| {
        parse(args)
            .unwrap()
            .into_invocation()
            .map(|run| run.isolation().cloned())
    };
    // The control: alone, it's taken (the other is found at its start).
    assert!(
        isolation(&["--no-image", "--network", "container:db", "true"]).is_ok(),
        "alone"
    );
    // Another container's network is its own: nothing of this one's
    // goes in it, nor another network beside it.
    for args in [
        &[
            "--no-image",
            "--network",
            "container:db",
            "--hostname",
            "x",
            "true",
        ][..],
        &[
            "--no-image",
            "--network",
            "container:db",
            "--network",
            "app",
            "true",
        ],
        &[
            "--no-image",
            "--network",
            "container:db",
            "--add-host",
            "db:10.0.0.9",
            "true",
        ],
        &[
            "--no-image",
            "--network",
            "container:db",
            "--dns",
            "10.0.0.53",
            "true",
        ],
        &[
            "--no-image",
            "--network",
            "container:db",
            "-p",
            "8080:80",
            "true",
        ],
        &[
            "--no-image",
            "--network",
            "container:db",
            "--sysctl",
            "net.ipv4.ip_forward=1",
            "true",
        ],
    ] {
        let error = isolation(args).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<shipyard_engine::Error>()
                .map(shipyard_engine::Error::kind),
            Some(shipyard_engine::ErrorKind::InvalidRequest),
            "{args:?}: {error:#}"
        );
    }
}

#[test]
fn network_modes() {
    assert_eq!(parse_network("host"), Ok(NetworkMode::Host));
    assert_eq!(parse_network("none"), Ok(NetworkMode::None));
    assert_eq!(parse_network("bridge"), Ok(NetworkMode::bridge("bridge")));
    assert_eq!(parse_network("app"), Ok(NetworkMode::bridge("app")));
    assert!(parse_network("a/b").is_err());
    assert!(parse(&["--network", "none", "--no-isolation", "true"]).is_err());
    assert!(parse(&["--no-image", "--network", "-x", "true"]).is_err());
    // Addresses need a network to be on; /etc files a root to be in.
    assert!(parse(&["--no-image", "--ip", "10.0.0.2", "true"]).is_err());
    assert!(parse(&["--no-image", "--mac-address", "02:42:0a:00:00:02", "true"]).is_err());
    assert!(
        parse(&[
            "--no-image",
            "--mac-address",
            "00:42:0a:00:00:02",
            "--network",
            "bridge",
            "true"
        ])
        .is_err()
    );
    assert!(parse(&["--no-image", "--add-host", "db", "true"]).is_err());
    let isolation = |args: &[&str]| {
        parse(args)
            .unwrap()
            .into_invocation()
            .map(|run| run.isolation().cloned())
    };
    if cfg!(target_os = "linux") {
        let own = isolation(&["--no-image", "--network", "none", "true"])
            .unwrap()
            .unwrap();
        assert!(own.makes_network_namespace());
        let host = isolation(&["--no-image", "--network", "host", "true"])
            .unwrap()
            .unwrap();
        assert!(!host.has(Namespace::Network));
        // The default: a network of its own (the bridge network for the
        // host's root, pasta without), except for a root that can't
        // make one. (Without pasta installed, a rootless run is refused.)
        if let Ok(Some(default)) = isolation(&["--no-image", "true"]) {
            assert_eq!(
                default.has(Namespace::Network),
                shipyard_engine::network::can_network() || !shipyard_engine::network::host_root()
            );
        }
        // A network setting needs a network of its own, and the error
        // says how to give it one.
        let error = isolation(&[
            "--no-image",
            "--network",
            "host",
            "--sysctl",
            "net.ipv4.ip_forward=1",
            "true",
        ])
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("any --network but host gives it one"),
            "{error}"
        );
        // Others reach the container whatever its network.
        let ipc = isolation(&[
            "--no-image",
            "--network",
            "host",
            "--sysctl",
            "kernel.shmmax=1",
            "true",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(ipc.sysctls, [("kernel.shmmax".to_owned(), "1".to_owned())]);
        let set = isolation(&[
            "--no-image",
            "--network",
            "none",
            "--sysctl",
            "net.ipv4.ip_forward=1",
            "true",
        ])
        .unwrap()
        .unwrap();
        assert!(
            set.sysctls
                .contains(&("net.ipv4.ip_forward".into(), "1".into()))
        );
    } else {
        let error = isolation(&["--no-image", "--network", "none", "true"]).unwrap_err();
        assert!(
            error.to_string().contains("--network needs Linux"),
            "{error}"
        );
    }
}

#[test]
fn the_long_form_gives_a_network_its_own_settings() {
    let parsed =
        parse_network("name=app,ip=10.89.0.5,mac-address=02:42:0a:59:00:05,alias=web,alias=x")
            .unwrap();
    assert_eq!(
        parsed,
        NetworkMode::Bridges(vec![Joined {
            name: "app".into(),
            ip: Some("10.89.0.5".parse().unwrap()),
            ip6: None,
            mac: Some([2, 0x42, 10, 0x59, 0, 5]),
            aliases: vec!["web".into(), "x".into()],
        }])
    );
    for bad in [
        "ip=10.89.0.5",
        "name=host",
        "name=app,ip=nope",
        "name=app,ip=10.0.0.1,ip=10.0.0.2",
        "name=app,name=db",
        "name=app,gw-priority=1",
        "name=app,alias=a.b",
        "name=app,mac-address=01:00:00:00:00:00",
        "name=app,ip",
    ] {
        assert!(parse_network(bad).is_err(), "{bad}");
    }
}

#[test]
fn names_are_dns_labels() {
    assert!(parse(&["--no-image", "--name", "db.app", "true"]).is_err());
    assert!(parse(&["--name", "db", "--no-isolation", "true"]).is_err());
    // An alias needs the network named.
    assert!(parse(&["--no-image", "--network-alias", "db", "true"]).is_err());
}

#[test]
fn isolation_is_the_default_on_linux_only() {
    let isolation = |args: &[&str]| {
        parse(args)
            .unwrap()
            .into_invocation()
            .unwrap()
            .isolation()
            .cloned()
    };
    assert_eq!(
        isolation(&["--no-image", "true"]).is_some(),
        cfg!(target_os = "linux"),
        "isolated by default exactly on Linux"
    );
    assert_eq!(isolation(&["--no-isolation", "true"]), None);
    if cfg!(target_os = "linux") {
        let named = isolation(&["--no-image", "--hostname", "box", "true"]).unwrap();
        assert_eq!(named.hostname.as_deref(), Some("box"));
    }
}

#[test]
fn a_root_filesystem_is_canonical_and_checked() {
    let isolation = |args: &[&str]| parse(args).unwrap().into_invocation();
    // Contradictions, whatever the system.
    assert!(parse(&["--no-isolation", "--rootfs", "/", "true"]).is_err());
    assert!(
        request(&["--no-image", "--read-only", "true"]).is_err(),
        "needs an image or --rootfs"
    );
    assert!(request(&["--read-only", "alpine", "true"]).is_ok());
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
        let root = invocation.isolation().cloned().unwrap().root.unwrap();
        assert_eq!(root.path, std::fs::canonicalize(dir).unwrap());
        assert!(root.read_only);
    }
}

#[test]
fn a_bundle_is_the_whole_description() {
    assert!(parse(&["--bundle", "/b"]).is_ok(), "no COMMAND needed");
    assert!(request(&["--bundle", "/b", "true"]).is_err(), "no COMMAND");
    for conflicting in [
        &["--bundle", "/b", "-e", "A=1"][..],
        &["--bundle", "/b", "-w", "/tmp"],
        &["--bundle", "/b", "--rootfs", "/r"],
        &["--bundle", "/b", "--no-isolation"],
    ] {
        assert!(parse(conflicting).is_err(), "{conflicting:?}");
    }
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
    let err = parse(&["--no-image", "--hostname", "bad_name", "true"])
        .unwrap()
        .into_invocation()
        .unwrap_err();
    assert!(format!("{err:#}").contains("hostname"), "{err:#}");
}

/// A bare `-e KEY` in the macOS guest copies the Mac's value, as the agent
/// passed it on, and never the guest `shipyard`'s own (its `PATH`);
/// elsewhere, this process's.
#[test]
fn a_bare_env_key_copies_the_callers_value() {
    use std::ffi::OsString;
    let key = |name: &str| OsString::from(name);
    let forwarded = Some(r#"{"TOKEN":"from the mac"}"#);
    assert_eq!(
        super::request::caller_var(forwarded, &key("TOKEN")),
        Some(OsString::from("from the mac"))
    );
    assert_eq!(super::request::caller_var(forwarded, &key("PATH")), None);
    assert_eq!(
        super::request::caller_var(None, &key("PATH")),
        std::env::var_os("PATH")
    );
    assert!(std::env::var_os("PATH").is_some(), "a control: PATH is set");
}

/// `--tmpfs` and `--mount` (Docker's): a bind and a volume join `-v`'s
/// strings, after them (a volume without a source as its lone place, an
/// anonymous one); a tmpfs joins `--tmpfs`'s; a bad one is clap's usage
/// error.
#[test]
fn tmpfs_and_mount_reach_the_request() {
    let asked = request(&[
        "-v",
        "a:/a",
        "--mount",
        "type=bind,source=/srv,target=/s,readonly",
        "--mount",
        "type=tmpfs,target=/t,tmpfs-size=1m",
        "--tmpfs",
        "/run:size=64m",
        "alpine",
    ])
    .unwrap();
    assert_eq!(asked.volume, ["a:/a", "/srv:/s:ro"]);
    let tmpfs: Vec<String> = asked
        .tmpfs
        .iter()
        .map(|mount| format!("{}:{}", mount.destination().display(), mount.options_text()))
        .collect();
    assert_eq!(tmpfs, ["/run:size=64m", "/t:size=1m"]);
    // No source: an anonymous volume, named at the create.
    let anonymous = request(&["--mount", "type=volume,target=/data,readonly", "alpine"]).unwrap();
    assert_eq!(anonymous.volume, ["/data:ro"]);
    for refused in [
        &["--tmpfs", "/run:x=1", "alpine"][..],
        &["--mount", "type=volume,target=data", "alpine"],
    ] {
        assert!(parse(refused).is_err(), "{refused:?}");
    }
}

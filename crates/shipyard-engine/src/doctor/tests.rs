use super::*;

fn check(level: Level) -> Check {
    Check::new(level, "x", "")
}

#[test]
fn the_verdict_counts_what_failed_and_what_is_limited() {
    let said = |checks: &[Check]| {
        let Verdict { text, failed } = verdict(checks);
        (text, failed)
    };
    let (text, failed) = said(&[check(Level::Ok), check(Level::Ok)]);
    assert_eq!(text, "containers run here");
    assert!(!failed);
    let (text, failed) = said(&[check(Level::Warn), check(Level::Ok), check(Level::Warn)]);
    assert_eq!(text, "containers run here; 2 things limited");
    assert!(!failed);
    let (text, failed) = said(&[check(Level::Warn), check(Level::Fail)]);
    assert_eq!(
        text,
        "containers can't run here as they are: 1 problem to fix"
    );
    assert!(failed);
    let (text, _) = said(&[check(Level::Fail), check(Level::Fail)]);
    assert_eq!(
        text,
        "containers can't run here as they are: 2 problems to fix"
    );
    let (text, _) = said(&[check(Level::Warn)]);
    assert_eq!(text, "containers run here; 1 thing limited");
    assert!(!verdict(&[]).failed);
}

/// The boot units (contrib/systemd), root's and a user's, say what F52's
/// design rests on: the stop in a unit of its own, started at once (no
/// `ExecStart`) and ordered before the recover unit, so stopped after it;
/// the recover unit with no stop of its own, wanting the stop unit, and
/// ending only its own process when stopped (`KillMode=process`: the
/// monitors are left for the stop), and stopped with it (`PartOf`). Root's
/// and the user's agree but for what they're wanted by, their ordering
/// and wants (the network), and their descriptions.
#[test]
fn the_boot_units_say_what_the_design_needs() {
    let contrib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/systemd");
    let settings = |path: std::path::PathBuf| -> Vec<(String, String)> {
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
            .lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    };
    let has = |unit: &[(String, String)], key: &str, wanted: &str| {
        unit.iter()
            .any(|(k, value)| k == key && value.split_whitespace().any(|word| word == wanted))
    };
    let lacks = |unit: &[(String, String)], key: &str| !unit.iter().any(|(k, _)| k == key);
    for (dir, target) in [
        (contrib.clone(), "multi-user.target"),
        (contrib.join("user"), "default.target"),
    ] {
        let stop = settings(dir.join("shipyard-stop.service"));
        let recover = settings(dir.join("shipyard-recover.service"));
        assert!(
            lacks(&stop, "ExecStart"),
            "{dir:?}: the stop unit starts nothing"
        );
        assert!(has(&stop, "Type", "oneshot") && has(&stop, "RemainAfterExit", "yes"));
        assert!(has(&stop, "ExecStop", "stop") && has(&stop, "ExecStop", "system"));
        assert!(has(&stop, "Before", "shipyard-recover.service"), "{dir:?}");
        assert!(
            lacks(&recover, "ExecStop"),
            "{dir:?}: the recover unit stops nothing"
        );
        assert!(has(&recover, "KillMode", "process"), "{dir:?}");
        assert!(has(&recover, "Wants", "shipyard-stop.service"), "{dir:?}");
        assert!(has(&recover, "After", "shipyard-stop.service"), "{dir:?}");
        assert!(has(&recover, "ExecStart", "recover"), "{dir:?}");
        assert!(has(&recover, "PartOf", "shipyard-stop.service"), "{dir:?}");
        assert!(has(&stop, "KillMode", "mixed") && has(&stop, "TimeoutStopSec", "5min"));
        for unit in [&stop, &recover] {
            assert!(has(unit, "WantedBy", target), "{dir:?}");
        }
    }
    // Root's and a user's agree, but for what they're wanted by, and the
    // network (a user's manager has none of its own to wait for).
    let apart = |unit: Vec<(String, String)>| -> Vec<(String, String)> {
        unit.into_iter()
            .filter(|(key, _)| {
                !["WantedBy", "After", "Wants", "Description"].contains(&key.as_str())
            })
            .collect()
    };
    for name in ["shipyard-stop.service", "shipyard-recover.service"] {
        assert_eq!(
            apart(settings(contrib.join(name))),
            apart(settings(contrib.join("user").join(name))),
            "{name}"
        );
    }
}

/// The API's units (contrib/systemd, step 11.2a): the socket where the
/// engine's `Paths::api_socket` puts it (root's in the runtime's root,
/// a user's in their runtime directory), its owner's alone (0600, in a
/// 0700 directory); the service started by it, ending only its own process
/// (the containers it started run on). Root's and a user's agree but for
/// the socket's place and their descriptions.
#[test]
fn the_api_units_say_what_the_design_needs() {
    let contrib = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contrib/systemd");
    let settings = |path: std::path::PathBuf| -> Vec<(String, String)> {
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
            .lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    };
    let value = |unit: &[(String, String)], key: &str| -> Vec<String> {
        unit.iter()
            .filter(|(k, _)| k == key)
            .map(|(_, value)| value.clone())
            .collect()
    };
    let root = format!("{}/api.sock", shipyard_runtime::container::DEFAULT_ROOT);
    for (dir, place) in [
        (contrib.clone(), root.as_str()),
        (contrib.join("user"), "%t/shipyard/api.sock"),
    ] {
        let socket = settings(dir.join("shipyard.socket"));
        let service = settings(dir.join("shipyard.service"));
        assert_eq!(value(&socket, "ListenStream"), [place], "{dir:?}");
        assert_eq!(value(&socket, "SocketMode"), ["0600"], "{dir:?}");
        assert_eq!(value(&socket, "DirectoryMode"), ["0700"], "{dir:?}");
        assert_eq!(value(&socket, "WantedBy"), ["sockets.target"], "{dir:?}");
        assert_eq!(
            value(&service, "ExecStart"),
            ["/usr/local/bin/shipyard system service"],
            "{dir:?}"
        );
        assert_eq!(value(&service, "KillMode"), ["process"], "{dir:?}");
        assert_eq!(value(&service, "Requires"), ["shipyard.socket"], "{dir:?}");
        assert!(
            value(&service, "WantedBy").is_empty(),
            "{dir:?}: started by its socket alone"
        );
    }
    let apart = |unit: Vec<(String, String)>| -> Vec<(String, String)> {
        unit.into_iter()
            .filter(|(key, _)| !["ListenStream", "Description"].contains(&key.as_str()))
            .collect()
    };
    for name in ["shipyard.socket", "shipyard.service"] {
        assert_eq!(
            apart(settings(contrib.join(name))),
            apart(settings(contrib.join("user").join(name))),
            "{name}"
        );
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};

    use nix::errno::Errno;
    use shipyard_runtime::cgroup::Controller;
    use shipyard_runtime::process::probe::Probe;

    use super::super::Level;
    use super::super::linux::{
        ApiSocket, BootUnits, ProbedAt, StopLimit, api_socket_check, boot_stores_check,
        boot_unit_check, controllers_check, firewall_check, held_off_check, kill_user_processes,
        left_by_gone, linger_check, logout_check, mounts, overlay_check, parse_timespan,
        pasta_check, stop_timeout_check, sudo_logout_check, user_namespaces,
    };
    use super::super::probe::{decode, encode};
    use std::time::Duration;

    /// The Docker API's line names where clients find it; it's never a
    /// limit, but a failed socket unit is said, with the journal to read.
    #[test]
    fn the_api_line_says_where_clients_find_it() {
        let path = Path::new("/run/shipyard/api.sock");
        let socket = |enabled, failed, served| ApiSocket {
            enabled,
            failed,
            served,
        };
        let enabled = api_socket_check(socket(true, false, true), path, false);
        assert_eq!(enabled.level, Level::Ok);
        assert!(
            enabled
                .text
                .contains("DOCKER_HOST=unix:///run/shipyard/api.sock"),
            "{}",
            enabled.text
        );
        let by_hand = api_socket_check(socket(false, false, true), path, false);
        assert_eq!(by_hand.level, Level::Ok);
        assert!(
            by_hand.text.starts_with("served (not by shipyard.socket)"),
            "{}",
            by_hand.text
        );
        let none = api_socket_check(socket(false, false, false), path, false);
        assert_eq!(none.level, Level::Ok);
        assert!(
            none.text
                .starts_with("not served at /run/shipyard/api.sock: ")
                && none.text.contains("enable shipyard.socket"),
            "{}",
            none.text
        );
        let failed = api_socket_check(socket(true, true, false), path, true);
        assert_eq!(failed.level, Level::Warn);
        assert!(
            failed.text.contains("journalctl --user -u shipyard.socket"),
            "{}",
            failed.text
        );
    }

    /// A refused mount fails doctor, whoever runs it, unless it's a
    /// rootless user namespace's want of privileges (that line's).
    #[test]
    fn a_refused_mount_namespace_fails() {
        let refused = |errno| Probe {
            user_namespace: None,
            mount: Some(Err(errno)),
            overlay: None,
        };
        let root = mounts(&refused(Errno::EPERM), false).unwrap();
        assert_eq!(root.level, Level::Fail);
        assert!(root.text.contains("--no-isolation"), "{}", root.text);
        assert_eq!(mounts(&refused(Errno::EPERM), true), None);
        assert_eq!(mounts(&refused(Errno::EACCES), true), None);
        assert_eq!(
            mounts(&refused(Errno::EINVAL), true).unwrap().level,
            Level::Fail
        );
        let fine = Probe {
            mount: Some(Ok(())),
            ..Probe::default()
        };
        assert_eq!(mounts(&fine, false), None);
        assert_eq!(mounts(&Probe::default(), false), None);
    }

    /// A gone doctor's leftovers are recognised by its process ID; a
    /// live one's (this process) and other names aren't.
    #[test]
    fn leftovers_are_a_gone_doctors() {
        let live = format!(".shipyard-doctor-{}-1", std::process::id());
        assert!(!left_by_gone(&live, ".shipyard-doctor-"));
        // Beyond any PID the kernel hands out (pid_max is at most 2^22).
        assert!(left_by_gone(
            ".shipyard-doctor-99999999-1",
            ".shipyard-doctor-"
        ));
        assert!(left_by_gone("doctor-99999999-1", "doctor-"));
        assert!(!left_by_gone("doctor-99999999", "doctor-"));
        assert!(!left_by_gone("run", "doctor-"));
        assert!(!left_by_gone(".shipyard-doctor-x-1", ".shipyard-doctor-"));
    }

    #[test]
    fn the_probes_line_reads_back() {
        for probe in [
            Probe::default(),
            Probe {
                user_namespace: Some(Ok(())),
                mount: Some(Err(Errno::EPERM)),
                overlay: None,
            },
            Probe {
                user_namespace: None,
                mount: Some(Ok(())),
                overlay: Some(Err(Errno::EINVAL)),
            },
        ] {
            assert_eq!(decode(&encode(&probe)), Some(probe));
        }
        for nonsense in [
            "",
            "probe",
            "probe 0 0",
            "probe 0 0 0 0",
            "prob 0 0 0",
            "probe x 0 0",
        ] {
            assert_eq!(decode(nonsense), None, "{nonsense:?}");
        }
    }

    /// Refused outright, or made without privileges (AppArmor's
    /// restriction): both fail, naming the setting; root's isn't
    /// checked.
    #[test]
    fn a_user_namespace_without_privileges_fails() {
        let restriction = Some("AppArmor restricts them");
        let refused = Probe {
            user_namespace: Some(Err(Errno::EPERM)),
            ..Probe::default()
        };
        let check = user_namespaces(&refused, restriction).unwrap();
        assert_eq!(check.level, Level::Fail);
        assert!(
            check.text.contains("AppArmor restricts them"),
            "{}",
            check.text
        );
        // Ubuntu 24.04's restriction refuses with EACCES.
        let stripped_eacces = Probe {
            user_namespace: Some(Ok(())),
            mount: Some(Err(Errno::EACCES)),
            overlay: None,
        };
        let check = user_namespaces(&stripped_eacces, restriction).unwrap();
        assert_eq!(check.level, Level::Fail);
        assert!(check.text.contains("without privileges"), "{}", check.text);
        let stripped = Probe {
            user_namespace: Some(Ok(())),
            mount: Some(Err(Errno::EPERM)),
            overlay: None,
        };
        let check = user_namespaces(&stripped, restriction).unwrap();
        assert_eq!(check.level, Level::Fail);
        assert!(check.text.contains("without privileges"), "{}", check.text);
        let stripped_unexplained = user_namespaces(&stripped, None).unwrap();
        assert!(
            stripped_unexplained.text.contains("security policy"),
            "{}",
            stripped_unexplained.text
        );
        // A mount refused for another reason isn't the namespace's.
        let fine = Probe {
            user_namespace: Some(Ok(())),
            mount: Some(Err(Errno::ENODEV)),
            overlay: None,
        };
        assert_eq!(user_namespaces(&fine, None).unwrap().level, Level::Ok);
        assert_eq!(user_namespaces(&Probe::default(), None), None);
    }

    #[test]
    fn overlay_says_where_it_was_probed() {
        let store = PathBuf::from("/home/u/.local/share/shipyard/images");
        let there = ProbedAt {
            store: store.clone(),
            dir: store.clone(),
        };
        let check = overlay_check(Some(Ok(())), &there, true).unwrap();
        assert_eq!(check.level, Level::Ok);
        assert!(check.text.contains("in the store"), "{}", check.text);
        let above = ProbedAt {
            store,
            dir: PathBuf::from("/home/u/.local/share"),
        };
        let check = overlay_check(Some(Err(Errno::EINVAL)), &above, true).unwrap();
        assert_eq!(check.level, Level::Fail);
        assert!(check.text.contains("not made yet"), "{}", check.text);
        assert!(
            check.text.contains("SHIPYARD_IMAGE_STORE"),
            "{}",
            check.text
        );
        assert_eq!(overlay_check(None, &above, true), None);
    }

    #[test]
    fn missing_controllers_limit_but_dont_fail() {
        let base = Path::new("/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service");
        let all = [
            Controller::Cpu,
            Controller::Io,
            Controller::Memory,
            Controller::Pids,
        ];
        assert_eq!(controllers_check(base, &all, true).level, Level::Ok);
        let check = controllers_check(base, &[Controller::Memory, Controller::Pids], true);
        assert_eq!(check.level, Level::Warn);
        assert!(
            check.text.contains("without the cpu, io controllers"),
            "{}",
            check.text
        );
        assert!(
            check.text.ends_with("need them are refused"),
            "{}",
            check.text
        );
        let one = controllers_check(
            base,
            &[Controller::Cpu, Controller::Memory, Controller::Pids],
            true,
        );
        assert!(
            one.text.contains("without the io controller:"),
            "{}",
            one.text
        );
        assert!(one.text.ends_with("need it are refused"), "{}", one.text);
        // F62's default can't be set without `pids`: said.
        let no_pids = controllers_check(
            base,
            &[Controller::Cpu, Controller::Io, Controller::Memory],
            true,
        );
        assert!(
            no_pids
                .text
                .ends_with("refused, and containers get no default process limit"),
            "{}",
            no_pids.text
        );
    }

    #[test]
    fn rootless_without_pasta_fails() {
        assert_eq!(
            pasta_check(Some(Path::new("/usr/bin/pasta"))).level,
            Level::Ok
        );
        let check = pasta_check(None);
        assert_eq!(check.level, Level::Fail);
        assert!(check.text.contains("passt"), "{}", check.text);
    }

    #[test]
    fn restarts_held_off_is_a_warning() {
        assert!(held_off_check(false).is_none());
        let held = held_off_check(true).unwrap();
        assert_eq!(held.level, Level::Warn);
        assert!(
            held.text.contains("shipyard system recover"),
            "{}",
            held.text
        );
    }

    #[test]
    fn linger_off_is_a_warning() {
        assert_eq!(linger_check(true, 2).level, Level::Ok);
        // Nothing that needs it: not a warning.
        assert_eq!(linger_check(false, 0).level, Level::Ok);
        let off = linger_check(false, 1);
        assert_eq!(off.level, Level::Warn);
        assert!(off.text.contains("loginctl enable-linger"), "{}", off.text);
        assert!(off.text.contains("reboot"), "{}", off.text);
    }

    #[test]
    fn kill_user_processes_is_read_as_logind_reads_it() {
        let configs = |texts: &[&str]| {
            texts
                .iter()
                .map(|&text| text.to_owned())
                .collect::<Vec<_>>()
        };
        // Unset, commented out, or another section's: not said.
        assert_eq!(kill_user_processes(&[]), None);
        assert_eq!(
            kill_user_processes(&configs(&["[Login]\n#KillUserProcesses=yes\n"])),
            None
        );
        assert_eq!(
            kill_user_processes(&configs(&["[Other]\nKillUserProcesses=yes\n"])),
            None
        );
        assert_eq!(
            kill_user_processes(&configs(&["[Login]\nKillUserProcesses=yes\n"])),
            Some(true)
        );
        // Spaces around `=`, any case.
        assert_eq!(
            kill_user_processes(&configs(&["[Login]\nKillUserProcesses = No\n"])),
            Some(false)
        );
        // The last setting wins: a drop-in after the main file.
        assert_eq!(
            kill_user_processes(&configs(&[
                "[Login]\nKillUserProcesses=yes\n",
                "[Login]\nKillUserProcesses=no\n",
            ])),
            Some(false)
        );
        assert_eq!(
            kill_user_processes(&configs(&[
                "[Login]\nKillUserProcesses=no\n",
                "[Login]\n KillUserProcesses=yes \n",
            ])),
            Some(true)
        );
        // Said whenever logind ends a session's processes: linger
        // doesn't keep a monitor that's in a session's scope.
        assert!(logout_check(true, false).is_some());
        let lingering = logout_check(true, true).unwrap();
        assert!(
            lingering.text.contains("systemd-run --user"),
            "{}",
            lingering.text
        );
        assert!(logout_check(false, false).is_none());
        assert!(logout_check(false, true).is_none());
        // Root: only through sudo, from a user's session.
        let sudo = sudo_logout_check(true, true).unwrap();
        assert!(sudo.text.contains("systemd-run --scope"), "{}", sudo.text);
        assert!(sudo_logout_check(true, false).is_none());
        assert!(sudo_logout_check(false, true).is_none());
    }

    /// Containers with a restart policy on stores the environment moved
    /// are a warning naming the variables and the unit's fix (F44); with
    /// no policy, or no moved store, nothing is said.
    #[test]
    fn stores_the_boot_unit_wont_see_are_a_warning() {
        assert!(boot_stores_check(0, &["SHIPYARD_CONTAINER_DIR"], false).is_none());
        assert!(boot_stores_check(2, &[], true).is_none());
        let moved = boot_stores_check(2, &["SHIPYARD_CONTAINER_DIR", "XDG_DATA_HOME"], true)
            .expect("a warning");
        assert_eq!(moved.level, Level::Warn);
        assert!(
            moved
                .text
                .contains("2 containers with a restart policy are on stores")
                && moved.text.contains("SHIPYARD_CONTAINER_DIR, XDG_DATA_HOME")
                && moved
                    .text
                    .contains("`systemctl --user edit shipyard-recover`")
                && moved.text.contains("the same for `shipyard-stop`"),
            "{}",
            moved.text
        );
    }

    /// The boot units as `systemctl` might say them.
    fn units(recover: bool, failed: bool) -> BootUnits {
        BootUnits {
            recover,
            stop: recover,
            failed,
        }
    }

    #[test]
    fn a_restart_policy_without_the_boot_unit_is_a_warning() {
        assert_eq!(
            boot_unit_check(units(true, false), 3, false).level,
            Level::Ok
        );
        assert_eq!(
            boot_unit_check(units(false, false), 0, true).level,
            Level::Ok
        );
        let missing = boot_unit_check(units(false, false), 2, false);
        assert_eq!(missing.level, Level::Warn);
        assert!(missing.text.contains("contrib/systemd"), "{}", missing.text);
        assert!(
            missing.text.contains("2 containers have"),
            "{}",
            missing.text
        );
        let failed = boot_unit_check(units(true, true), 1, false);
        assert_eq!(failed.level, Level::Warn);
        assert!(
            failed.text.contains("`journalctl -u shipyard-recover`"),
            "{}",
            failed.text
        );
        // A rootless user unit's journal is the user's.
        let user = boot_unit_check(units(true, true), 1, true);
        assert_eq!(user.level, Level::Warn);
        assert!(
            user.text
                .contains("`journalctl --user -u shipyard-recover`"),
            "{}",
            user.text
        );
    }

    /// The recover unit enabled without the stop unit (one file, from
    /// before 10.2d1) is a warning: a shutdown stops nothing (F52).
    #[test]
    fn the_recover_unit_without_the_stop_unit_is_a_warning() {
        let alone = BootUnits {
            recover: true,
            stop: false,
            failed: false,
        };
        let check = boot_unit_check(alone, 1, false);
        assert_eq!(check.level, Level::Warn);
        assert!(
            check.text.contains("shipyard-stop.service"),
            "{}",
            check.text
        );
        assert_eq!(
            boot_unit_check(units(true, false), 1, false).level,
            Level::Ok
        );
    }

    /// Timespans as `systemctl show` prints them; `infinity` and anything
    /// else unread.
    #[test]
    fn timespans_are_read_as_systemctl_prints_them() {
        let secs = Duration::from_secs;
        assert_eq!(parse_timespan("5min"), Some(secs(300)));
        assert_eq!(parse_timespan("1min 30s"), Some(secs(90)));
        assert_eq!(parse_timespan("1h 2min 3s"), Some(secs(3723)));
        assert_eq!(parse_timespan("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_timespan("90"), Some(secs(90)));
        assert_eq!(parse_timespan("1.500s"), Some(Duration::from_millis(1500)));
        assert_eq!(
            parse_timespan("1min 30.500000s"),
            Some(Duration::from_millis(90_500))
        );
        assert_eq!(parse_timespan("2w 1d"), Some(secs(15 * 86_400)));
        for unread in ["infinity", "", "5 weeks", "min"] {
            assert_eq!(parse_timespan(unread), None, "{unread:?}");
        }
    }

    /// A container whose stop timeout (and the stop's settling after it)
    /// isn't within the stop unit's is a warning (F46); within it, or
    /// either unknown, nothing is said.
    #[test]
    fn a_stop_timeout_past_the_units_is_a_warning() {
        let unit = Some(StopLimit {
            time: Duration::from_secs(300),
            by_user_manager: false,
        });
        assert!(stop_timeout_check(unit, Some(10), false).is_none());
        assert!(stop_timeout_check(unit, None, false).is_none());
        assert!(stop_timeout_check(None, Some(600), false).is_none());
        let past = stop_timeout_check(unit, Some(600), true).expect("a warning");
        assert_eq!(past.level, Level::Warn);
        assert!(past.text.contains("(600s)"), "{}", past.text);
        assert!(
            past.text.contains("systemctl --user edit shipyard-stop"),
            "{}",
            past.text
        );
        // Its own timeout and the stop's settling (10 s) must fit.
        assert!(stop_timeout_check(unit, Some(290), false).is_none());
        assert!(
            stop_timeout_check(unit, Some(291), false).is_some(),
            "no room left to settle its end"
        );
        // A user's manager's own limit (Ubuntu 24.04's 5 s) is said as
        // its, with its own fix.
        let manager = Some(StopLimit {
            time: Duration::from_secs(5),
            by_user_manager: true,
        });
        let short = stop_timeout_check(manager, Some(10), true).expect("a warning");
        assert!(
            short.text.contains("your user manager's (5s"),
            "{}",
            short.text
        );
        assert!(
            short.text.contains("sudo systemctl edit user@.service"),
            "{}",
            short.text
        );
    }

    #[test]
    fn another_firewall_in_the_way_is_a_warning_until_fixed() {
        use shipyard_network::firewalls::{Blocks, Conflict, Firewall};
        let firewalld = |fixed| Conflict {
            firewall: Firewall::Firewalld,
            bridge: "shipyard0".to_owned(),
            blocks: Blocks {
                way_out: true,
                ..Blocks::default()
            },
            dns: None,
            fixed,
            fix: vec!["sudo firewall-cmd --reload".to_owned()],
        };
        assert_eq!(firewall_check(Ok(&[])).level, Level::Ok);
        assert_eq!(firewall_check(Ok(&[firewalld(true)])).level, Level::Ok);
        let check = firewall_check(Ok(&[firewalld(false), firewalld(false)]));
        assert_eq!(check.level, Level::Warn);
        assert_eq!(
            check.text.lines().count(),
            2,
            "one per line: {}",
            check.text
        );
        // One finding a line, as it is: the caller lays them out.
        assert!(
            !check.text.lines().nth(1).unwrap().starts_with(' '),
            "{}",
            check.text
        );
        assert_eq!(firewall_check(Err("EPERM")).level, Level::Warn);
    }
}

/// Off Linux nothing is checked yet: the report says why, and has no user
/// nor kernel.
#[cfg(not(target_os = "linux"))]
#[test]
fn off_linux_nothing_is_checked_and_its_said_why() {
    let engine = crate::Engine::new(
        crate::Paths::from_env(),
        std::sync::Arc::new(crate::report::Collect::default()),
    );
    let report = checks(&engine);
    assert!(report.checks.is_empty());
    assert_eq!((report.user, report.kernel.as_deref()), (None, None));
    assert!(
        report
            .isolation
            .is_some_and(|why| why.starts_with("none here: commands run as ordinary processes"))
    );
}

/// The guest's checks read back as written; version 1 as first written; a
/// newer version refused; a failed check found.
#[test]
fn the_guests_checks_are_versioned() {
    let report = Report {
        os: "linux",
        arch: "aarch64",
        kernel: Some("6.18.57-0-virt".to_owned()),
        backend: "native (host kernel)",
        user: None,
        isolation: None,
        checks: vec![
            Check::new(Level::Ok, "cgroups", "v2"),
            Check::new(Level::Fail, "network", "no bridge"),
        ],
    };
    let guest = GuestDoctor::of(&report);
    assert!(guest.failed());
    assert_eq!(
        GuestDoctor::decode(&guest.encode().unwrap()).unwrap(),
        guest
    );
    let v1 =
        br#"{"version":1,"kernel":"6.18","checks":[{"level":"ok","name":"cgroups","text":"v2"}]}"#;
    let read = GuestDoctor::decode(v1).unwrap();
    assert!(!read.failed());
    assert_eq!(read.checks[0].name, "cgroups");
    assert_eq!(read.checks[0].level, Level::Ok);
    let newer = br#"{"version":2,"kernel":null,"checks":[],"more":1}"#;
    assert_eq!(GuestDoctor::decode(newer), Err(GuestDoctorError::Newer(2)));
}

//! Planning a run, tested on requests made here (the CLI's own tests go
//! through its flags): what each part of a request comes to, and the kind
//! of each mistake.

#![allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]

use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;

use proptest::prelude::*;
use shipyard_runtime::isolation::{IdMap, IdMaps, RootlessMaps};
use shipyard_runtime::process::Env;
use shipyard_runtime::resources::Resources;
use shipyard_runtime::security::{Capabilities, Capability, CapabilitySet, Seccomp};

use super::command::add_image_env;
use super::network::with_network_defaults;
use super::*;
use crate::network::Joined;
use crate::report::{Collect, WarningKind};
use crate::{ErrorKind, Paths};

/// A request for `command`, changed by `with`.
fn request(command: &[&str], with: impl FnOnce(&mut RunRequest)) -> RunRequest {
    let mut request = RunRequest {
        command: command.iter().map(OsString::from).collect(),
        ..RunRequest::default()
    };
    with(&mut request);
    request
}

/// An engine reporting to a list (its paths the environment's).
fn engine() -> (Engine, Arc<Collect>) {
    let collect = Arc::new(Collect::default());
    let engine = Engine::new(
        Paths::from_env(),
        Arc::clone(&collect) as Arc<dyn crate::report::Reporter>,
    );
    (engine, collect)
}

/// An image's runtime config with this entrypoint and command.
fn image(entrypoint: &[&str], cmd: &[&str]) -> shipyard_image::RuntimeConfig {
    let owned = |list: &[&str]| list.iter().map(|&s| s.to_owned()).collect();
    shipyard_image::RuntimeConfig {
        entrypoint: owned(entrypoint),
        cmd: owned(cmd),
        working_dir: Some("/app".to_owned()),
        ..shipyard_image::RuntimeConfig::default()
    }
}

fn cap(name: &str) -> Capability {
    name.parse().unwrap()
}

#[test]
fn the_command_is_dockers() {
    let command =
        |given: &[&str], entrypoint: Option<&str>, config: &shipyard_image::RuntimeConfig| {
            request(given, |request| {
                request.image = Some("x".to_owned());
                request.entrypoint = entrypoint.map(OsString::from);
            })
            .resolve_command(Some(config))
            .map(|command| command.join(OsStr::new(" ")).into_string().unwrap())
        };
    let both = image(&["/entry", "-q"], &["serve"]);
    assert_eq!(command(&[], None, &both).unwrap(), "/entry -q serve");
    assert_eq!(
        command(&["other", "arg"], None, &both).unwrap(),
        "/entry -q other arg",
        "arguments replace Cmd, after the entrypoint"
    );
    assert_eq!(
        command(&[], Some("/bin/echo"), &both).unwrap(),
        "/bin/echo",
        "--entrypoint drops Cmd too"
    );
    assert_eq!(
        command(&["hi"], Some("/bin/echo"), &both).unwrap(),
        "/bin/echo hi"
    );
    assert_eq!(command(&["ls", "/"], Some(""), &both).unwrap(), "ls /");
    assert_eq!(
        command(&[], Some(""), &both).unwrap(),
        "serve",
        "no entrypoint, the image's Cmd (as Docker)"
    );
    let cmd_only = image(&[], &["/bin/sh"]);
    assert_eq!(command(&[], None, &cmd_only).unwrap(), "/bin/sh");
    assert_eq!(command(&["true"], None, &cmd_only).unwrap(), "true");
    let err = command(&[], None, &image(&[], &[])).unwrap_err();
    assert!(err.to_string().contains("sets no command"), "{err}");
    assert_eq!(err.kind(), ErrorKind::InvalidRequest);
}

#[test]
fn an_explicit_workdir_wins_even_slash() {
    let config = image(&[], &["sh"]);
    let workdir = |given: Option<&str>| {
        request(&["true"], |request| {
            request.image = Some("x".to_owned());
            request.workdir = given.map(PathBuf::from);
        })
        .resolve_workdir(Some(&config))
    };
    assert_eq!(workdir(Some("/")), Path::new("/"));
    assert_eq!(workdir(None), Path::new("/app"), "the image's");
    assert_eq!(
        request(&["true"], |_| {}).resolve_workdir(None),
        Path::new("/")
    );
}

#[test]
fn an_images_env_never_reads_the_host() {
    // HOME is set wherever tests run: an image naming it must not get it.
    assert!(std::env::var_os("HOME").is_some());
    let mut env = Env::new();
    let mut warnings = Vec::new();
    let entries = ["HOME".to_owned(), "LANG=C.UTF-8".to_owned()];
    add_image_env(&mut env, &entries, &mut warnings).unwrap();
    assert_eq!(env.get(OsStr::new("HOME")), None);
    assert_eq!(env.get(OsStr::new("LANG")), Some(OsStr::new("C.UTF-8")));
    assert!(
        warnings[0].message.contains("`HOME` has no value"),
        "{warnings:?}"
    );
    assert_eq!(warnings[0].kind, WarningKind::ImageEnvIgnored);
}

/// A bare `--env KEY` is the caller's value, as the caller gave it: never
/// read from this process's environment (an API's server isn't the user).
#[test]
fn a_bare_env_name_is_the_callers_value() {
    let mut env = Env::new();
    // HOME is set in this process: without the caller's value, it's skipped.
    command::add_env(&mut env, OsStr::new("HOME"), &[]).unwrap();
    assert_eq!(env.get(OsStr::new("HOME")), None, "never this process's");
    let given = [(OsString::from("HOME"), OsString::from("/callers"))];
    command::add_env(&mut env, OsStr::new("HOME"), &given).unwrap();
    assert_eq!(env.get(OsStr::new("HOME")), Some(OsStr::new("/callers")));
    for bad in ["", "A B"] {
        let error = command::add_env(&mut env, OsStr::new(bad), &[]).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidRequest, "{bad:?}");
    }
}

#[test]
fn a_volume_is_a_name_or_an_absolute_host_path_that_exists() {
    let mounts = |spec: &str| {
        request(&["true"], |request| {
            request.rootfs = Some(PathBuf::from("/"));
            request.volume = vec![spec.to_owned()];
        })
        .volume_mounts()
    };
    let relative = mounts("./here:/mnt").unwrap_err();
    assert_eq!(relative.kind(), ErrorKind::InvalidRequest, "{relative:#}");
    let missing = mounts("/no/such/dir:/mnt").unwrap_err();
    assert!(
        format!("{missing:#}").contains("/no/such/dir"),
        "{missing:#}"
    );
    let found = mounts("/tmp:/mnt:ro").unwrap();
    assert!(Path::new(&found[0].source).is_absolute() && found[0].read_only);
    assert_eq!(mounts("data:/data").unwrap()[0].source, "data");
    let no_root = request(&["true"], |request| {
        request.volume = vec!["data:/data".to_owned()];
    })
    .volume_mounts()
    .unwrap_err();
    assert_eq!(no_root.kind(), ErrorKind::InvalidRequest);
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
    let named = |user: &str, group: Option<&str>| Ok((user.to_owned(), group.map(str::to_owned)));
    assert_eq!(parse_user("nobody"), named("nobody", None));
    assert_eq!(parse_user("alice:staff"), named("alice", Some("staff")));
    assert_eq!(parse_user("alice:20"), named("alice", Some("20")));
    for not_a_name in ["a b", "a,b", "a/b"] {
        assert!(parse_user(not_a_name).is_err(), "{not_a_name}");
    }
}

#[test]
fn capability_flags_build_dockers_rule() {
    let security = |with: &dyn Fn(&mut RunRequest)| request(&["true"], with).security().unwrap();
    let rule = |with: &dyn Fn(&mut RunRequest)| match security(with).capabilities {
        Capabilities::Default { kept, added } => (kept, added),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        rule(&|_| {}),
        (CapabilitySet::DOCKER_DEFAULT, CapabilitySet::EMPTY)
    );
    let (kept, added) = rule(&|request| {
        request.cap_drop = vec![CapabilityArg::One(cap("chown"))];
        request.cap_add = vec![CapabilityArg::One(cap("CAP_SYS_ADMIN"))];
    });
    assert!(!kept.contains(cap("CHOWN")) && kept.contains(cap("KILL")));
    assert_eq!(added.iter().collect::<Vec<_>>(), [cap("SYS_ADMIN")]);
    assert_eq!(
        rule(&|request| request.cap_drop = vec![CapabilityArg::All]).0,
        CapabilitySet::EMPTY
    );
    assert_eq!(
        rule(&|request| request.cap_add = vec![CapabilityArg::All]).1,
        CapabilitySet::ALL
    );
    // Dropping everything, then adding one: just that one.
    let (kept, added) = rule(&|request| {
        request.cap_drop = vec![CapabilityArg::All];
        request.cap_add = vec![CapabilityArg::One(cap("KILL"))];
    });
    assert!(kept.is_empty() && added.contains(cap("KILL")));
    assert!(security(&|_| {}).no_new_privileges);
    for on in [true, false] {
        assert_eq!(
            security(&|request| request.security_opt = vec![SecurityOpt::NoNewPrivileges(on)])
                .no_new_privileges,
            on
        );
    }
    let both = request(&["true"], |request| {
        request.cap_add = vec![CapabilityArg::One(cap("KILL"))];
        request.cap_drop = vec![CapabilityArg::One(cap("kill"))];
    })
    .security()
    .unwrap_err();
    assert!(both.to_string().contains("both name CAP_KILL"), "{both}");
    assert_eq!(both.kind(), ErrorKind::InvalidRequest);
}

#[test]
fn seccomp_options_choose_the_filter() {
    let seccomp = |options: Vec<SecurityOpt>| {
        request(&["true"], |request| request.security_opt = options)
            .security()
            .map(|s| s.seccomp)
    };
    assert_eq!(seccomp(Vec::new()).unwrap(), Seccomp::Default);
    assert_eq!(
        seccomp(vec![SecurityOpt::SeccompUnconfined]).unwrap(),
        Seccomp::Unconfined
    );
    let dir = std::env::temp_dir().join(format!("shipyard-engine-seccomp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = |name: &str, json: &str| {
        let path = dir.join(name);
        std::fs::write(&path, json).unwrap();
        SecurityOpt::SeccompProfile(path)
    };
    let good = file(
        "good.json",
        r#"{"defaultAction": "SCMP_ACT_ERRNO",
            "syscalls": [{"names": ["read"], "action": "SCMP_ACT_ALLOW"}]}"#,
    );
    let Seccomp::Profile(profile) = seccomp(vec![good.clone()]).unwrap() else {
        panic!("not a profile");
    };
    assert_eq!(profile.syscalls[0].names, ["read"]);
    // The last one wins, as with Docker.
    assert_eq!(
        seccomp(vec![good, SecurityOpt::SeccompUnconfined]).unwrap(),
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
        let err = format!("{:#}", seccomp(vec![option]).unwrap_err());
        assert!(err.contains(name) && err.contains(wanted), "{err}");
    }
    let missing = format!(
        "{:#}",
        seccomp(vec![SecurityOpt::SeccompProfile(PathBuf::from(
            "/no/such.json"
        ))])
        .unwrap_err()
    );
    assert!(
        missing.contains("seccomp profile /no/such.json"),
        "{missing}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Root's user namespace is the one `--uidmap` asks for (if any); anyone
/// else always gets one, with maps chosen for them, and may not pass
/// `--uidmap`.
#[test]
fn the_user_namespace_depends_on_who_runs() {
    let range = |s: &str| s.parse::<IdMap>().unwrap();
    let chosen = || RootlessMaps {
        maps: IdMaps::single((1000, 1000)),
        shortfall: Some("short".to_owned()),
    };
    let namespace = |uids: &[&str], gids: &[&str], root: bool| {
        request(&["true"], |request| {
            request.uidmap = uids.iter().map(|map| range(map)).collect();
            request.gidmap = gids.iter().map(|map| range(map)).collect();
        })
        .user_namespace(root, chosen)
    };

    let (maps, short) = namespace(&["0:100000:65536"], &[], true).unwrap();
    let maps = maps.unwrap();
    assert_eq!(maps.uids, [range("0:100000:65536")]);
    assert_eq!(maps.gids, maps.uids, "--gidmap defaults to --uidmap");
    assert_eq!(short, None);
    let both = namespace(&["0:100000:1", "1:200000:65535"], &["0:300000:65536"], true)
        .unwrap()
        .0
        .unwrap();
    assert_eq!(both.uids.len(), 2);
    assert_eq!(both.gids, [range("0:300000:65536")]);
    assert_eq!(namespace(&[], &[], true).unwrap(), (None, None), "rootful");

    let (maps, short) = namespace(&[], &[], false).unwrap();
    assert_eq!(maps, Some(IdMaps::single((1000, 1000))), "rootless, always");
    assert_eq!(short.as_deref(), Some("short"));
    let err = namespace(&["0:100000:65536"], &[], false).unwrap_err();
    assert!(err.to_string().contains("need root"), "{err}");
    assert_eq!(err.kind(), ErrorKind::NeedsRoot);
}

/// A change to a request, and what its refusal says.
type Refusal<'a> = (&'a dyn Fn(&mut RunRequest), &'a str);

/// A bridge network named in a request, with its settings.
fn bridge(name: &str, with: impl FnOnce(&mut Joined)) -> NetworkMode {
    let mut joined = Joined {
        name: name.to_owned(),
        ..Joined::default()
    };
    with(&mut joined);
    NetworkMode::Bridges(vec![joined])
}

#[test]
fn several_networks_are_bridge_networks_each_once() {
    let (engine, _) = engine();
    let mode = |with: &dyn Fn(&mut RunRequest)| request(&["true"], with).network_mode(&engine);
    let both = mode(&|request| {
        request.network = vec![
            bridge("front", |_| {}),
            bridge("back", |joined| joined.aliases = vec!["db".to_owned()]),
        ];
    })
    .unwrap();
    let NetworkMode::Bridges(joined) = both else {
        panic!("{both:?}");
    };
    assert_eq!(
        joined
            .iter()
            .map(|one| one.name.as_str())
            .collect::<Vec<_>>(),
        ["front", "back"]
    );
    // A global alias goes to each network that has names.
    let aliased = mode(&|request| {
        request.network = vec![bridge("bridge", |_| {}), bridge("back", |_| {})];
        request.network_alias = vec!["db".to_owned()];
    })
    .unwrap();
    let NetworkMode::Bridges(joined) = aliased else {
        panic!("{aliased:?}");
    };
    assert!(joined[0].aliases.is_empty());
    assert_eq!(joined[1].aliases, ["db"]);
    // --ip for one network goes to it.
    let one = mode(&|request| {
        request.network = vec![bridge("app", |_| {})];
        request.ip = Some("10.89.0.9".parse().unwrap());
    })
    .unwrap();
    let NetworkMode::Bridges(joined) = one else {
        panic!("{one:?}");
    };
    assert_eq!(joined[0].ip, Some("10.89.0.9".parse().unwrap()));
    let refused: [Refusal<'_>; 6] = [
        (
            &|request| request.network = vec![NetworkMode::Host, bridge("app", |_| {})],
            "can't be combined",
        ),
        (
            &|request| request.network = vec![bridge("app", |_| {}), bridge("app", |_| {})],
            "given twice",
        ),
        (
            &|request| {
                request.network = vec![bridge("a", |_| {}), bridge("b", |_| {})];
                request.ip = Some("10.0.0.2".parse().unwrap());
            },
            "with several, give each its own",
        ),
        (
            &|request| {
                request.network = vec![bridge("app", |joined| {
                    joined.ip = Some("10.89.0.2".parse().unwrap());
                })];
                request.ip = Some("10.89.0.3".parse().unwrap());
            },
            "given twice",
        ),
        (
            &|request| {
                request.network = vec![bridge("bridge", |joined| {
                    joined.aliases = vec!["x".to_owned()];
                })];
            },
            "alias= need a network made with",
        ),
        (
            &|request| {
                request.network = vec![NetworkMode::None];
                request.ip = Some("10.0.0.2".parse().unwrap());
            },
            "need a bridge network",
        ),
    ];
    for (with, message) in refused {
        let error = mode(with).unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
        assert_eq!(error.kind(), ErrorKind::InvalidRequest, "{error}");
    }
}

#[test]
fn names_are_dns_labels_and_aliases_need_a_created_network() {
    let (engine, _) = engine();
    let plan = |with: &dyn Fn(&mut RunRequest)| {
        request(&["true"], with).network_plan(&engine, RunId::new().unwrap())
    };
    let error = plan(&|request| {
        request.network = vec![bridge("bridge", |_| {})];
        request.network_alias = vec!["db".to_owned()];
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("alias= need a network made with"),
        "{error}"
    );
    let named = plan(&|request| {
        request.network = vec![bridge("app", |_| {})];
        request.name = Some("db".to_owned());
        request.network_alias = vec!["database".to_owned()];
    })
    .unwrap();
    assert_eq!(named.name.as_deref(), Some("db"));
    let NetworkMode::Bridges(joined) = &named.mode else {
        panic!("{:?}", named.mode);
    };
    assert_eq!(joined[0].aliases, ["database"]);
    // On the default network, a name is only recorded.
    let bridge_named = plan(&|request| {
        request.network = vec![bridge("bridge", |_| {})];
        request.name = Some("db".to_owned());
    })
    .unwrap();
    assert_eq!(bridge_named.name.as_deref(), Some("db"));
}

#[test]
fn a_network_of_its_own_gets_dockers_default_settings() {
    let all = |_: &str| true;
    let pair = |key: &str, value: &str| (key.to_owned(), value.to_owned());
    // Without a user namespace: every group may ping.
    assert_eq!(
        with_network_defaults(&[], None, all),
        [
            pair("net.ipv4.ip_unprivileged_port_start", "0"),
            pair("net.ipv4.ping_group_range", "0 2147483647"),
            pair("net.ipv4.ip_forward", "0"),
            pair("net.ipv6.conf.all.forwarding", "0"),
        ]
    );
    // With one: up to the top of its group map.
    let maps = IdMaps {
        uids: vec![
            IdMap {
                container: 0,
                host: 1000,
                size: 1,
            },
            IdMap {
                container: 1,
                host: 100_000,
                size: 65536,
            },
        ],
        gids: vec![
            IdMap {
                container: 0,
                host: 1000,
                size: 1,
            },
            IdMap {
                container: 1,
                host: 100_000,
                size: 65536,
            },
        ],
    };
    assert_eq!(
        with_network_defaults(&[], Some(&maps), all)[1],
        pair("net.ipv4.ping_group_range", "0 65536")
    );
    // The kernel compares the ends as host IDs: a top group mapped below
    // group 0's host ID would turn ping off, so it's not the top.
    let below = IdMaps {
        uids: maps.uids.clone(),
        gids: vec![
            IdMap {
                container: 0,
                host: 200_000,
                size: 1,
            },
            IdMap {
                container: 1,
                host: 100_000,
                size: 65536,
            },
        ],
    };
    assert_eq!(
        with_network_defaults(&[], Some(&below), all)[1],
        pair("net.ipv4.ping_group_range", "0 0")
    );
    // A map without group 0: no ping default.
    let shifted = IdMaps {
        uids: vec![IdMap {
            container: 1,
            host: 100_000,
            size: 10,
        }],
        gids: vec![IdMap {
            container: 1,
            host: 100_000,
            size: 10,
        }],
    };
    assert!(
        !with_network_defaults(&[], Some(&shifted), all)
            .iter()
            .any(|(key, _)| key == "net.ipv4.ping_group_range")
    );
    // The user's value wins, and comes after (a container that routes
    // asks for it).
    let user = [
        pair("net.ipv4.ip_unprivileged_port_start", "1024"),
        pair("net.ipv4.ip_forward", "1"),
    ];
    assert_eq!(
        with_network_defaults(&user, None, all),
        [
            pair("net.ipv4.ping_group_range", "0 2147483647"),
            pair("net.ipv6.conf.all.forwarding", "0"),
            pair("net.ipv4.ip_unprivileged_port_start", "1024"),
            pair("net.ipv4.ip_forward", "1"),
        ]
    );
    // A setting the kernel doesn't have is left out.
    let no_ports = |key: &str| key != "net.ipv4.ip_unprivileged_port_start";
    assert_eq!(
        with_network_defaults(&[], None, no_ports),
        [
            pair("net.ipv4.ping_group_range", "0 2147483647"),
            pair("net.ipv4.ip_forward", "0"),
            pair("net.ipv6.conf.all.forwarding", "0"),
        ]
    );
}

/// A change to a request, and what it needs.
type Change = fn(&mut RunRequest);

/// Requests planning can't carry out that combine what can't go together
/// (a bundle with more, no isolation with a container's settings): what
/// each is, the change that makes it, and the change that mends it.
fn refused_combinations() -> [(&'static str, Change, Change); 7] {
    [
        (
            "a bundle with an image",
            |r| {
                r.bundle = Some("/b".into());
                r.image = Some("alpine".to_owned());
            },
            |r| {
                r.image = None;
                r.command.clear();
            },
        ),
        (
            "a bundle with a command",
            |r| r.bundle = Some("/b".into()),
            |r| r.command.clear(),
        ),
        (
            "no isolation with an image",
            |r| {
                r.no_isolation = true;
                r.image = Some("alpine".to_owned());
            },
            |r| r.no_isolation = false,
        ),
        (
            "no isolation with a capability dropped",
            |r| {
                r.no_isolation = true;
                r.cap_drop = vec![CapabilityArg::All];
            },
            |r| r.no_isolation = false,
        ),
        (
            "no isolation with a limit",
            |r| {
                r.no_isolation = true;
                r.limits.first_given = Some("--memory");
            },
            |r| r.no_isolation = false,
        ),
        (
            "no isolation with limits only the resources say",
            |r| {
                r.no_isolation = true;
                r.limits.resources = Err("bad".to_owned());
            },
            |r| r.limits.resources = Ok(Resources::default()),
        ),
        (
            "no isolation with a user namespace",
            |r| {
                r.no_isolation = true;
                r.uidmap = vec!["0:1:1".parse().unwrap()];
            },
            |r| r.uidmap.clear(),
        ),
    ]
}

/// Requests that ask for what needs something they don't have.
fn refused_needs() -> [(&'static str, Change, Change); 9] {
    [
        (
            "a rootfs and an image",
            |r| {
                r.rootfs = Some("/".into());
                r.image = Some("alpine".to_owned());
            },
            |r| r.rootfs = None,
        ),
        (
            "an entrypoint without an image",
            |r| r.entrypoint = Some("x".into()),
            |r| r.image = Some("alpine".to_owned()),
        ),
        (
            "a pull policy without an image",
            |r| r.pull = Some(crate::image::PullPolicy::Always),
            |r| r.image = Some("alpine".to_owned()),
        ),
        (
            "publishing all without an image",
            |r| r.publish_all = true,
            |r| r.image = Some("alpine".to_owned()),
        ),
        (
            "read-only without a root",
            |r| r.read_only = true,
            |r| r.rootfs = Some("/".into()),
        ),
        (
            "a gidmap without a uidmap",
            |r| r.gidmap = vec!["0:1:1".parse().unwrap()],
            |r| r.uidmap = vec!["0:1:1".parse().unwrap()],
        ),
        (
            "an address without a network",
            |r| r.ip = Some("10.0.0.2".parse().unwrap()),
            |r| r.network = vec![bridge("app", |_| {})],
        ),
        (
            "an alias without a network",
            |r| r.network_alias = vec!["db".to_owned()],
            |r| r.network = vec![bridge("app", |_| {})],
        ),
        (
            "no command",
            |r| r.command.clear(),
            |r| r.image = Some("alpine".to_owned()),
        ),
    ]
}

/// What planning takes for granted is refused, not quietly left out, when
/// a request (an API's: the CLI's parser refuses these first) asks for it:
/// each refused alone, and allowed with what it needs.
#[test]
fn a_request_that_cant_be_carried_out_is_refused() {
    for (what, change, mend) in refused_combinations().into_iter().chain(refused_needs()) {
        let mut asked = request(&["true"], change);
        let error = asked.validate().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidRequest, "{what}: {error}");
        mend(&mut asked);
        assert!(
            asked.validate().is_ok(),
            "{what}, mended: {:?}",
            asked.validate()
        );
    }
    // A bundle alone, its streams and stop policy its own to take.
    let bundle = request(&[], |r| {
        r.bundle = Some("/b".into());
        r.streams.interactive = true;
        r.stop_timeout = 3;
    });
    assert!(bundle.validate().is_ok());
    // And `check` asks first: nothing is looked at for a refused request.
    let (engine, _) = engine();
    let refused = check(&engine, request(&["true"], |r| r.publish_all = true)).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::InvalidRequest);
}

/// Without the environment's values: `--env`'s and the caller's are
/// often secrets.
#[test]
fn a_request_shows_no_environment_values() {
    let shown = format!(
        "{:?}",
        request(&["true"], |r| {
            r.env = vec!["TOKEN=hunter2".into(), "HOME".into()];
            r.caller_env = vec![("HOME".into(), "/secret/home".into())];
        })
    );
    assert!(shown.contains("TOKEN") && shown.contains("HOME"), "{shown}");
    assert!(
        !shown.contains("hunter2") && !shown.contains("/secret/home"),
        "{shown}"
    );
}

/// The kinds of a request's mistakes, as an API caller sees them.
#[test]
fn a_requests_mistakes_have_kinds() {
    let (engine, _) = engine();
    let kind = |with: &dyn Fn(&mut RunRequest)| {
        check(&engine, request(&["true"], with))
            .map(|_| ())
            .unwrap_err()
            .kind()
    };
    assert_eq!(
        kind(&|request| {
            request.no_isolation = true;
            request.workdir = Some(PathBuf::from("relative"));
        }),
        ErrorKind::InvalidRequest
    );
    if !cfg!(target_os = "linux") {
        assert_eq!(
            kind(&|request| request.rootfs = Some(PathBuf::from("/"))),
            ErrorKind::Unsupported
        );
        assert_eq!(
            kind(&|request| request.image = Some("alpine".to_owned())),
            ErrorKind::Unsupported
        );
        let limited = |request: &mut RunRequest| {
            request.limits.first_given = Some("--memory");
        };
        assert_eq!(kind(&limited), ErrorKind::Unsupported);
    }
    // A limit that couldn't be worked out is raised where the isolation is
    // made (on Linux: off it, the limit itself is refused first).
    if cfg!(target_os = "linux") {
        let error = check(
            &engine,
            request(&["true"], |request| {
                request.limits.resources = Err("--memory 1x: not a size".to_owned());
            }),
        )
        .map(|_| ())
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidRequest);
        assert_eq!(error.to_string(), "--memory 1x: not a size");
    }
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

/// A container that's root on the host's own files is warned of, in the
/// planned run's warnings (whoever runs it): maps of the host's ID 0, or
/// (the host's root) no root filesystem and no user namespace.
#[cfg(target_os = "linux")]
#[test]
fn root_on_the_hosts_files_is_warned_of() {
    if !nix::unistd::geteuid().is_root() || !crate::network::host_root() {
        return;
    }
    let (engine, _) = engine();
    let warned = |with: &dyn Fn(&mut RunRequest)| {
        let Checked::Command(planned) = check(&engine, request(&["true"], with)).unwrap() else {
            panic!("a command's run");
        };
        let (invocation, _held) = planned.finish(&engine, None, None).unwrap();
        invocation
            .warnings()
            .iter()
            .filter(|warning| warning.kind == WarningKind::RootOnHostFiles)
            .count()
    };
    assert_eq!(
        warned(&|r| {
            r.network = vec![NetworkMode::None];
            r.uidmap = vec!["0:0:1".parse().unwrap()];
        }),
        1,
        "maps of host 0"
    );
    assert_eq!(
        warned(&|r| r.network = vec![NetworkMode::None]),
        1,
        "no root, no maps"
    );
    assert_eq!(
        warned(&|r| {
            r.network = vec![NetworkMode::None];
            r.uidmap = vec!["0:100000:65536".parse().unwrap()];
        }),
        0,
        "a user namespace of its own"
    );
}

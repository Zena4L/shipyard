use std::ffi::OsString;
use std::path::PathBuf;

use serde_json::{Value, json};
use shipyard_engine::containers::RestartPolicy;
use shipyard_engine::network::{Joined, NetworkMode};
use shipyard_engine::run::{CapabilityArg, SecurityOpt};

use super::*;

/// What `docker create` (Docker 24's CLI) sends for a plain `docker create
/// alpine`: every default it writes, zeros and empty strings included.
fn plain() -> Value {
    serde_json::from_str(
        r#"{
        "Hostname": "", "Domainname": "", "User": "",
        "AttachStdin": false, "AttachStdout": true, "AttachStderr": true,
        "Tty": false, "OpenStdin": false, "StdinOnce": false,
        "Env": null, "Cmd": null, "Image": "alpine",
        "Volumes": {}, "WorkingDir": "", "Entrypoint": null,
        "OnBuild": null, "Labels": {},
        "HostConfig": {
            "Binds": null, "ContainerIDFile": "",
            "LogConfig": {"Type": "", "Config": {}},
            "NetworkMode": "default", "PortBindings": {},
            "RestartPolicy": {"Name": "no", "MaximumRetryCount": 0},
            "AutoRemove": false, "VolumeDriver": "", "VolumesFrom": null,
            "ConsoleSize": [24, 80], "CapAdd": null, "CapDrop": null,
            "CgroupnsMode": "", "Dns": [], "DnsOptions": [], "DnsSearch": [],
            "ExtraHosts": null, "GroupAdd": null, "IpcMode": "", "Cgroup": "",
            "Links": null, "OomScoreAdj": 0, "PidMode": "", "Privileged": false,
            "PublishAllPorts": false, "ReadonlyRootfs": false, "SecurityOpt": null,
            "UTSMode": "", "UsernsMode": "", "ShmSize": 0, "Isolation": "",
            "CpuShares": 0, "Memory": 0, "NanoCpus": 0, "CgroupParent": "",
            "BlkioWeight": 0, "BlkioWeightDevice": [], "BlkioDeviceReadBps": [],
            "BlkioDeviceWriteBps": [], "BlkioDeviceReadIOps": [],
            "BlkioDeviceWriteIOps": [], "CpuPeriod": 0, "CpuQuota": 0,
            "CpuRealtimePeriod": 0, "CpuRealtimeRuntime": 0, "CpusetCpus": "",
            "CpusetMems": "", "Devices": [], "DeviceCgroupRules": null,
            "DeviceRequests": null, "MemoryReservation": 0, "MemorySwap": 0,
            "MemorySwappiness": null, "OomKillDisable": null, "PidsLimit": null,
            "Ulimits": null, "CpuCount": 0, "CpuPercent": 0,
            "IOMaximumIOps": 0, "IOMaximumBandwidth": 0,
            "MaskedPaths": null, "ReadonlyPaths": null
        },
        "NetworkingConfig": {"EndpointsConfig": {}}
    }"#,
    )
    .unwrap()
}

/// `plain()` with `changes` merged in (`HostConfig`'s merged one level
/// down).
fn with(changes: &Value) -> Value {
    let mut body = plain();
    for (key, value) in changes.as_object().unwrap() {
        if key == "HostConfig" {
            for (key, value) in value.as_object().unwrap() {
                body["HostConfig"][key] = value.clone();
            }
        } else {
            body[key] = value.clone();
        }
    }
    body
}

fn create(body: &Value, name: Option<&str>) -> Result<Create, ApiError> {
    let body: CreateBody = serde_json::from_value(body.clone()).unwrap();
    request(&body, name)
}

fn words(words: &[&str]) -> Vec<OsString> {
    words.iter().map(OsString::from).collect()
}

/// Docker's defaults, as its CLI sends them, ask for nothing: the request
/// is the engine's default but for the image.
#[test]
fn docker_s_defaults_ask_for_nothing() {
    let made = create(&plain(), None).unwrap();
    assert!(!made.auto_remove);
    let request = made.request;
    assert_eq!(request.image.as_deref(), Some("alpine"));
    assert_eq!(request.name, None);
    assert!(request.command.is_empty());
    assert_eq!(request.entrypoint, None);
    assert!(request.network.is_empty(), "default: the engine's");
    assert_eq!(request.restart, RestartPolicy::No);
    assert!(!request.limits.given(), "{:?}", request.limits.first_given);
    assert!(request.publish.is_empty());
    assert!(request.health.is_none());
    assert_eq!(request.oom_score_adj, None);
    assert_eq!(request.stop_timeout, RunRequest::default().stop_timeout);
}

/// The settings `docker run -d` users give most, each where the engine
/// takes it.
#[test]
fn a_create_s_settings_reach_the_request() {
    let body = with(&json!({
        "Hostname": "box", "User": "1000:1000", "Env": ["A=1", "B"],
        "Cmd": ["sleep", "60"], "WorkingDir": "/srv",
        "Labels": {"app": "web"}, "StopSignal": "SIGINT", "StopTimeout": 5,
        "Tty": true, "OpenStdin": true,
        "HostConfig": {
            "Binds": ["data:/data:ro"],
            "PortBindings": {
                "80/tcp": [{"HostIp": "", "HostPort": "8080"}],
                "53/udp": [{"HostIp": "127.0.0.1", "HostPort": ""}],
                "443/tcp": null
            },
            "RestartPolicy": {"Name": "on-failure", "MaximumRetryCount": 3},
            "AutoRemove": false, "CapAdd": ["NET_ADMIN"], "CapDrop": ["ALL"],
            "Dns": ["1.1.1.1"], "DnsSearch": ["example.com"], "DnsOptions": ["ndots:2"],
            "ExtraHosts": ["db:10.0.0.5"], "GroupAdd": ["wheel"],
            "OomScoreAdj": 100, "ReadonlyRootfs": true,
            "SecurityOpt": ["no-new-privileges"], "Sysctls": {"net.ipv4.ip_forward": "1"},
            "Ulimits": [{"Name": "nofile", "Soft": 1024, "Hard": 2048}],
            "Devices": [{"PathOnHost": "/dev/fuse", "PathInContainer": "", "CgroupPermissions": "rwm"}]
        }
    }));
    let request = create(&body, Some("/web")).unwrap().request;
    assert_eq!(
        request.name.as_deref(),
        Some("web"),
        "Docker's `/` is taken off"
    );
    assert_eq!(request.hostname.as_deref(), Some("box"));
    assert_eq!(
        request.user,
        Some(("1000".to_owned(), Some("1000".to_owned())))
    );
    assert_eq!(request.env, words(&["A=1", "B"]));
    assert_eq!(request.command, words(&["sleep", "60"]));
    assert_eq!(request.workdir, Some(PathBuf::from("/srv")));
    assert_eq!(request.labels.get("app").map(String::as_str), Some("web"));
    assert_eq!(request.stop_signal, Signal::INT);
    assert_eq!(request.stop_timeout, 5);
    assert!(request.streams.tty && request.streams.interactive);
    assert_eq!(request.volume, vec!["data:/data:ro".to_owned()]);
    let mut ports: Vec<(Option<u16>, u16)> = request
        .publish
        .iter()
        .map(|port| (port.host_port, port.container_port))
        .collect();
    ports.sort_unstable();
    assert_eq!(ports, [(None, 53), (None, 443), (Some(8080), 80)]);
    assert!(
        request
            .publish
            .iter()
            .any(|port| port.container_port == 53 && port.host_ip == Some([127, 0, 0, 1].into()))
    );
    assert_eq!(request.restart, RestartPolicy::OnFailure { max: Some(3) });
    assert_eq!(
        request.cap_add,
        vec!["NET_ADMIN".parse::<CapabilityArg>().unwrap()]
    );
    assert_eq!(request.cap_drop, vec![CapabilityArg::All]);
    assert_eq!(request.dns, vec![std::net::IpAddr::from([1, 1, 1, 1])]);
    assert_eq!(request.dns_search, vec!["example.com".to_owned()]);
    assert_eq!(request.dns_option, vec!["ndots:2".to_owned()]);
    assert_eq!(request.add_host.len(), 1);
    assert_eq!(request.group_add, vec!["wheel".to_owned()]);
    assert_eq!(request.oom_score_adj, Some(100));
    assert!(request.read_only);
    assert_eq!(
        request.security_opt,
        vec![SecurityOpt::NoNewPrivileges(true)]
    );
    let seccomp = with(
        &json!({"HostConfig": {"SecurityOpt": ["seccomp=/etc/p.json", "seccomp:unconfined"]}}),
    );
    assert_eq!(
        create(&seccomp, None).unwrap().request.security_opt,
        vec![
            SecurityOpt::SeccompProfile(PathBuf::from("/etc/p.json")),
            SecurityOpt::SeccompUnconfined
        ]
    );
    assert_eq!(
        request.sysctl,
        vec![("net.ipv4.ip_forward".to_owned(), "1".to_owned())]
    );
    assert_eq!(request.ulimit, vec!["nofile=1024:2048".parse().unwrap()]);
    assert_eq!(request.device.len(), 1);
    assert_eq!(request.device[0].container, None, "empty: the host's path");
}

/// Docker's entrypoint is a list: its first word is the program, the rest
/// go before `Cmd`; an empty one is none; a string is one word.
#[test]
fn the_entrypoint_s_other_words_go_before_the_command() {
    let request = create(
        &with(&json!({"Entrypoint": ["sh", "-c"], "Cmd": ["echo hi"]})),
        None,
    )
    .unwrap()
    .request;
    assert_eq!(request.entrypoint, Some(OsString::from("sh")));
    assert_eq!(request.command, words(&["-c", "echo hi"]));
    for empty in [json!([]), json!([""])] {
        let request = create(&with(&json!({"Entrypoint": empty})), None)
            .unwrap()
            .request;
        assert_eq!(request.entrypoint, Some(OsString::new()), "{empty}");
    }
    let request = create(&with(&json!({"Entrypoint": "/bin/run", "Cmd": "x"})), None)
        .unwrap()
        .request;
    assert_eq!(request.entrypoint, Some(OsString::from("/bin/run")));
    assert_eq!(request.command, words(&["x"]));
}

/// A network by name takes its one endpoint's settings; the special modes
/// take none; another container's network is refused.
#[test]
fn networks_are_read_with_their_endpoint() {
    let body = with(&json!({
        "HostConfig": {"NetworkMode": "backend"},
        "NetworkingConfig": {"EndpointsConfig": {"backend": {
            "Aliases": ["db"], "MacAddress": "02:42:0a:58:00:02",
            "IPAMConfig": {"IPv4Address": "10.88.0.9", "IPv6Address": ""}
        }}}
    }));
    let network = create(&body, None).unwrap().request.network;
    assert_eq!(
        network,
        vec![NetworkMode::Bridges(vec![Joined {
            name: "backend".to_owned(),
            ip: Some([10, 88, 0, 9].into()),
            ip6: None,
            mac: Some([0x02, 0x42, 0x0a, 0x58, 0x00, 0x02]),
            aliases: vec!["db".to_owned()],
        }])]
    );
    // An older client names the network by its endpoint alone.
    let body = with(&json!({"NetworkingConfig": {"EndpointsConfig": {"backend": {}}}}));
    assert_eq!(
        create(&body, None).unwrap().request.network,
        vec![NetworkMode::bridge("backend")]
    );
    // Docker's CLI (API 1.44 and on) names the default's endpoint
    // `default`: the default network, never one so named.
    let body = with(&json!({
        "HostConfig": {"NetworkMode": "default"},
        "NetworkingConfig": {"EndpointsConfig": {"default": {}}}
    }));
    assert_eq!(create(&body, None).unwrap().request.network, Vec::new());
    // Its settings are refused, never dropped (`docker run --ip …`).
    for settings in [
        json!({"IPAMConfig": {"IPv4Address": "10.88.0.9"}}),
        json!({"Aliases": ["db"]}),
        json!({"MacAddress": "02:42:0a:58:00:02"}),
    ] {
        let body = with(&json!({
            "HostConfig": {"NetworkMode": "default"},
            "NetworkingConfig": {"EndpointsConfig": {"default": settings}}
        }));
        assert!(create(&body, None).is_err(), "{settings}");
    }
    for (mode, expected) in [
        ("host", NetworkMode::Host),
        ("none", NetworkMode::None),
        ("pasta", NetworkMode::Pasta),
        ("bridge", NetworkMode::bridge("bridge")),
    ] {
        let body = with(&json!({"HostConfig": {"NetworkMode": mode}}));
        assert_eq!(
            create(&body, None).unwrap().request.network,
            vec![expected],
            "{mode}"
        );
    }
    let disabled = with(&json!({"NetworkDisabled": true}));
    assert_eq!(
        create(&disabled, None).unwrap().request.network,
        vec![NetworkMode::None]
    );
    // Another container's IPC namespace (compose's `ipc: service:`).
    let ipc = with(&json!({"HostConfig": {"IpcMode": "container:db"}}));
    assert_eq!(
        create(&ipc, None).unwrap().request.ipc,
        shipyard_engine::run::IpcMode::Container("db".to_owned())
    );
    let shareable = with(&json!({"HostConfig": {"IpcMode": "shareable"}}));
    assert_eq!(
        create(&shareable, None).unwrap().request.ipc,
        shipyard_engine::run::IpcMode::Private
    );
    // Another container's (compose's `network_mode: service:`).
    let joined = with(&json!({"HostConfig": {"NetworkMode": "container:db"}}));
    assert_eq!(
        create(&joined, None).unwrap().request.network,
        vec![NetworkMode::Container("db".to_owned())]
    );
    for refused in [
        json!({"HostConfig": {"NetworkMode": "container:"}}),
        json!({"HostConfig": {"NetworkMode": "container:db"}, "MacAddress": "02:42:0a:58:00:02"}),
        json!({"HostConfig": {"NetworkMode": "container:db"},
               "NetworkingConfig": {"EndpointsConfig": {"db": {}}}}),
        json!({"HostConfig": {"NetworkMode": "host"},
               "NetworkingConfig": {"EndpointsConfig": {"host": {}}}}),
        json!({"HostConfig": {"NetworkMode": "a"},
               "NetworkingConfig": {"EndpointsConfig": {"b": {}}}}),
        json!({"NetworkingConfig": {"EndpointsConfig": {"a": {}, "b": {}}}}),
        json!({"MacAddress": "02:42:0a:58:00:02"}),
        json!({"HostConfig": {"NetworkMode": "bad name!"}}),
    ] {
        let error = create(&with(&refused), None).unwrap_err();
        assert_eq!(error.status, hyper::StatusCode::BAD_REQUEST, "{refused}");
    }
}

/// What Shipyard can't do is refused, naming what to use instead; the
/// same fields at Docker's defaults aren't.
#[test]
fn what_shipyard_can_t_do_is_refused() {
    let cases = [
        (json!({"HostConfig": {"Privileged": true}}), "Privileged"),
        (json!({"HostConfig": {"Init": true}}), "Init"),
        (
            json!({"HostConfig": {"VolumesFrom": ["db"]}}),
            "VolumesFrom",
        ),
        (json!({"HostConfig": {"Links": ["db:db"]}}), "Links"),
        (json!({"Volumes": {"data": {}}}), "Volumes"),
        (
            json!({"HostConfig": {"VolumeDriver": "nfs"}}),
            "VolumeDriver",
        ),
        (json!({"HostConfig": {"Runtime": "runsc"}}), "Runtime"),
        (
            json!({"HostConfig": {"LogConfig": {"Type": "syslog"}}}),
            "LogConfig",
        ),
        (json!({"HostConfig": {"PidMode": "host"}}), "PidMode"),
        (json!({"HostConfig": {"IpcMode": "host"}}), "IpcMode"),
        (json!({"HostConfig": {"UTSMode": "host"}}), "UTSMode"),
        (json!({"HostConfig": {"UsernsMode": "host"}}), "UsernsMode"),
        (
            json!({"HostConfig": {"CgroupnsMode": "host"}}),
            "CgroupnsMode",
        ),
        (json!({"Image": ""}), "Image"),
        (json!({"HostConfig": {"ShmSize": 1024}}), "ShmSize"),
        (
            json!({"HostConfig": {"DeviceRequests": [{"Count": -1}]}}),
            "DeviceRequests",
        ),
        (
            json!({"HostConfig": {"StorageOpt": {"size": "10G"}}}),
            "StorageOpt",
        ),
        (
            json!({"HostConfig": {"CgroupParent": "limited.slice"}}),
            "CgroupParent",
        ),
        (
            json!({"HostConfig": {"OomKillDisable": true}}),
            "OomKillDisable",
        ),
        (
            json!({"HostConfig": {"MemorySwappiness": 0}}),
            "MemorySwappiness",
        ),
        (json!({"HostConfig": {"MaskedPaths": []}}), "MaskedPaths"),
        (
            json!({"HostConfig": {"ReadonlyPaths": ["/proc"]}}),
            "ReadonlyPaths",
        ),
        (
            json!({"HostConfig": {"BlkioWeightDevice": [{"Path": "/dev/sda", "Weight": 10}]}}),
            "BlkioWeightDevice",
        ),
        (
            json!({"HostConfig": {"CpuRealtimeRuntime": 950}}),
            "CpuRealtimeRuntime",
        ),
    ];
    for (changes, named) in cases {
        let error = create(&with(&changes), None).unwrap_err();
        assert_eq!(error.status, hyper::StatusCode::BAD_REQUEST, "{changes}");
        assert!(
            error.message.contains(named),
            "{changes}: {}",
            error.message
        );
    }
    for allowed in [
        json!({"HostConfig": {"Tmpfs": {}, "Mounts": [], "Init": false, "Links": []}}),
        json!({"HostConfig": {"IpcMode": "private", "CgroupnsMode": "private"}}),
        json!({"HostConfig": {"LogConfig": {"Type": "json-file"}}}),
        json!({"Volumes": null}),
        json!({"HostConfig": {"MemorySwappiness": -1, "OomKillDisable": false, "ShmSize": 0}}),
        json!({"HostConfig": {"StorageOpt": {}, "CgroupParent": "", "DeviceRequests": []}}),
    ] {
        assert!(create(&with(&allowed), None).is_ok(), "{allowed}");
    }
}

/// Values the CLI's flags would refuse are refused here too (400), naming
/// the field.
#[test]
fn bad_values_are_refused_with_their_field() {
    let cases = [
        (json!({"User": "a:b:c"}), "User"),
        (json!({"StopSignal": "SIGNOPE"}), "StopSignal"),
        (json!({"StopTimeout": -1}), "StopTimeout"),
        (json!({"HostConfig": {"CapAdd": ["NOPE"]}}), "CapAdd"),
        (
            json!({"HostConfig": {"SecurityOpt": ["label=disable"]}}),
            "SecurityOpt",
        ),
        (json!({"HostConfig": {"Dns": ["not-an-ip"]}}), "Dns"),
        (
            json!({"HostConfig": {"ExtraHosts": ["nocolon"]}}),
            "ExtraHosts",
        ),
        (
            json!({"HostConfig": {"PortBindings": {"80/sctp": []}}}),
            "PortBindings",
        ),
        (
            json!({"HostConfig": {"PortBindings": {"80/tcp": [{"HostPort": "x"}]}}}),
            "PortBindings",
        ),
        (
            json!({"HostConfig": {"RestartPolicy": {"Name": "sometimes"}}}),
            "RestartPolicy",
        ),
        (
            json!({"HostConfig": {"RestartPolicy": {"Name": "always", "MaximumRetryCount": 2}}}),
            "RestartPolicy",
        ),
        (
            json!({"HostConfig": {"Ulimits": [{"Name": "nope", "Soft": 1, "Hard": 1}]}}),
            "Ulimits",
        ),
        (
            json!({"HostConfig": {"Devices": [{"PathOnHost": "fuse"}]}}),
            "Devices",
        ),
        (json!({"HostConfig": {"Memory": -5}}), "Memory"),
        (
            json!({"Healthcheck": {"Test": ["CMD", "true"], "Interval": 5}}),
            "Healthcheck",
        ),
        (json!({"Healthcheck": {"Interval": -1}}), "Interval"),
        (json!({"Healthcheck": {"Retries": -1}}), "Retries"),
        (
            json!({"HostConfig": {"SecurityOpt": ["seccomp={\"defaultAction\":\"SCMP_ACT_ALLOW\"}"]}}),
            "SecurityOpt",
        ),
        (
            json!({"HostConfig": {"SecurityOpt": ["seccomp=profile.json"]}}),
            "SecurityOpt",
        ),
    ];
    for (changes, named) in cases {
        let error = create(&with(&changes), None).unwrap_err();
        assert_eq!(error.status, hyper::StatusCode::BAD_REQUEST, "{changes}");
        assert!(
            error.message.starts_with(named),
            "{changes}: {}",
            error.message
        );
    }
}

/// A seccomp profile Docker's CLI sends inline (the file's JSON) is refused
/// as such, not taken for a path.
#[test]
fn an_inline_seccomp_profile_is_refused_as_one() {
    let body = with(
        &json!({"HostConfig": {"SecurityOpt": ["seccomp={\"defaultAction\":\"SCMP_ACT_ALLOW\"}"]}}),
    );
    let error = create(&body, None).unwrap_err();
    assert!(
        error.message.contains("a seccomp profile given inline"),
        "{}",
        error.message
    );
}

/// Limits go through the CLI's own mapping: Docker's units, 0 unset.
#[test]
fn limits_are_mapped_as_the_cli_maps_them() {
    let body = with(&json!({"HostConfig": {
        "Memory": 268_435_456, "NanoCpus": 1_500_000_000, "PidsLimit": 100,
        "CpuShares": 512
    }}));
    let limits = create(&body, None).unwrap().request.limits;
    assert!(limits.given());
    let resources = limits.resources.unwrap();
    assert_eq!(resources.memory_max, Some(268_435_456));
    assert_eq!(resources.pids_max, Some(100));
    assert!(resources.cpu_max.is_some());
    assert!(resources.cpu_weight.is_some());
}

/// A health check: Docker's nanoseconds as milliseconds; no test is the
/// image's; `NONE` is none.
#[test]
fn health_checks_are_read_in_docker_s_units() {
    let body = with(&json!({"Healthcheck": {
        "Test": ["CMD-SHELL", "curl -f localhost"], "Interval": 2_000_000_000,
        "Timeout": 0, "StartPeriod": 500_000_000, "Retries": 4
    }}));
    let check = create(&body, None).unwrap().request.health.unwrap();
    assert_eq!(
        check.test,
        vec!["CMD-SHELL".to_owned(), "curl -f localhost".to_owned()]
    );
    assert_eq!(check.interval_ms, 2000);
    assert_eq!(check.timeout_ms, 0);
    assert_eq!(check.start_period_ms, 500);
    assert_eq!(check.retries, 4);
    let tuned = with(&json!({"Healthcheck": {"Interval": 1_000_000_000}}));
    let check = create(&tuned, None).unwrap().request.health.unwrap();
    assert!(check.test.is_empty(), "the image's test, tuned");
    let none = with(&json!({"Healthcheck": {"Test": ["NONE"]}}));
    assert!(
        create(&none, None)
            .unwrap()
            .request
            .health
            .unwrap()
            .is_disabled()
    );
}

/// `AutoRemove` is the create's own.
#[test]
fn auto_remove_is_kept() {
    let body = with(&json!({"HostConfig": {"AutoRemove": true}}));
    assert!(create(&body, None).unwrap().auto_remove);
}

/// `StdinOnce` (as `docker run -i` creates it) is kept with its input open;
/// with a terminal it isn't (a terminal's input never ends: closing it is a
/// hang-up).
#[test]
fn stdin_once_is_kept_without_a_terminal() {
    let once = |tty: bool| {
        create(
            &with(&json!({"OpenStdin": true, "StdinOnce": true, "Tty": tty})),
            None,
        )
        .unwrap()
        .request
        .streams
        .once
    };
    assert!(once(false));
    assert!(!once(true));
    let closed = with(&json!({"OpenStdin": false, "StdinOnce": true}));
    assert!(!create(&closed, None).unwrap().request.streams.once);
}

/// F62: Docker's CLI sends `PidsLimit: 0` without `--pids-limit`, as null
/// is: neither asks, so the default applies; `-1` asks for none, a number
/// for that.
#[test]
fn an_unset_pids_limit_leaves_the_default() {
    let given = |limit: Value| {
        let made = create(&with(&json!({"HostConfig": {"PidsLimit": limit}})), None).unwrap();
        (
            made.request.limits.pids_given,
            made.request.limits.resources.unwrap().pids_max,
        )
    };
    assert_eq!(given(Value::Null), (false, None));
    assert_eq!(given(json!(0)), (false, None));
    assert_eq!(given(json!(-1)), (true, None));
    assert_eq!(given(json!(100)), (true, Some(100)));
}

/// A body's `ExposedPorts` keys reach the request (the engine checks them).
#[test]
fn exposed_ports_are_kept() {
    let made = create(
        &with(&json!({"ExposedPorts": {"80/tcp": {}, "53/udp": {}}})),
        None,
    )
    .unwrap();
    assert_eq!(made.request.expose, ["53/udp", "80/tcp"]);
}

/// Docker's update body (its CLI sends every resource field, 0 where not
/// given): the limits given, the zeros not; `{}` none at all.
#[test]
fn an_update_body_asks_only_what_it_gives() {
    let body: HostConfig = serde_json::from_str(
        r#"{"Memory": 100663296, "MemorySwap": 201326592, "NanoCpus": 0, "CpuShares": 0,
            "CpuPeriod": 0, "CpuQuota": 0, "PidsLimit": 60, "BlkioWeight": 0,
            "CpusetCpus": "", "CpusetMems": "", "MemoryReservation": 0, "KernelMemory": 0,
            "RestartPolicy": {"Name": "on-failure", "MaximumRetryCount": 2}}"#,
    )
    .unwrap();
    let limits = docker_limits(&body).unwrap();
    let mut expected = DockerLimits::default();
    expected.memory = Some(96 << 20);
    expected.memory_swap = Some(192 << 20);
    expected.pids_limit = Some(60);
    assert_eq!(limits, expected);
    assert_eq!(restart_policy(&body).unwrap().to_string(), "on-failure:2");
    let empty: HostConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(docker_limits(&empty).unwrap(), DockerLimits::default());
}

/// `HostConfig.Tmpfs` and `HostConfig.Mounts` as `--tmpfs` and `--mount`
/// read them: binds and volumes as `-v` strings, tmpfs mounts with their
/// size and mode (Go's `os.FileMode`, a number: `1023` is `1777`).
#[test]
fn tmpfs_and_mounts_are_docker_s() {
    let made = create(
        &with(&json!({"HostConfig": {
            "Binds": ["/a:/a"],
            "Tmpfs": {"/run": "size=64m,exec"},
            "Mounts": [
                {"Type": "bind", "Source": "/srv", "Target": "/s", "ReadOnly": true,
                 "BindOptions": {"Propagation": "rprivate", "CreateMountpoint": true}},
                {"Type": "volume", "Source": "data", "Target": "/data",
                 "VolumeOptions": {"NoCopy": true}},
                {"Type": "tmpfs", "Target": "/t",
                 "TmpfsOptions": {"SizeBytes": 1_048_576, "Mode": 1023}}
            ]
        }})),
        None,
    )
    .unwrap();
    assert_eq!(
        made.request.volume,
        ["/a:/a", "/srv:/s:ro", "data:/data:nocopy"]
    );
    assert_eq!(
        made.request.make_sources,
        [PathBuf::from("/a"), PathBuf::from("/srv")],
        "a `Binds` host path, and a mount asking for it"
    );
    let tmpfs: Vec<(String, Vec<String>)> = made
        .request
        .tmpfs
        .iter()
        .map(|mount| {
            (
                mount.destination().display().to_string(),
                mount.options().to_vec(),
            )
        })
        .collect();
    assert_eq!(
        tmpfs,
        [
            (
                "/run".to_owned(),
                vec!["size=64m".to_owned(), "exec".to_owned()]
            ),
            (
                "/t".to_owned(),
                vec!["size=1048576".to_owned(), "mode=1777".to_owned()]
            ),
        ]
    );
}

/// Go's `os.FileMode` bits (Docker's API's `TmpfsOptions.Mode`) as the
/// kernel's octal mode: the sticky bit Go keeps high up becomes `01000`.
#[test]
fn go_s_modes_are_the_kernel_s() {
    assert_eq!(super::go_mode(0o1777), "1777");
    assert_eq!(super::go_mode((1 << 20) | 0o777), "1777", "Go's sticky bit");
    assert_eq!(
        super::go_mode((1 << 31) | 0o755),
        "755",
        "a directory's type bit"
    );
    assert_eq!(super::go_mode((1 << 23) | (1 << 22) | 0o700), "6700");
}

/// What a mount asks that Shipyard can't do is refused (400), naming it:
/// a tmpfs option, a mount without a target, an anonymous volume at a
/// relative place, another
/// propagation, a volume driver, a non-recursive bind, a source to make,
/// no-copy off, a tmpfs's own options.
#[test]
fn mounts_shipyard_can_t_do_are_refused() {
    let cases = [
        (json!({"HostConfig": {"Tmpfs": {"/run": "x=1"}}}), "Tmpfs"),
        (
            json!({"HostConfig": {"Mounts": [{"Type": "bind"}]}}),
            "Mounts",
        ),
        (
            json!({"HostConfig": {"Mounts": [{"Type": "volume", "Target": "data"}]}}),
            "anonymous volume",
        ),
        (
            json!({"HostConfig": {"Mounts": [{"Type": "bind", "Source": "/srv", "Target": "/s",
                "BindOptions": {"Propagation": "rshared"}}]}}),
            "propagation",
        ),
        (
            json!({"HostConfig": {"Mounts": [{"Type": "volume", "Source": "v", "Target": "/v",
                "VolumeOptions": {"DriverConfig": {"Name": "nfs"}}}]}}),
            "driver",
        ),
        (
            json!({"HostConfig": {"Mounts": [{"Type": "bind", "Source": "/srv", "Target": "/s",
                "BindOptions": {"NonRecursive": true}}]}}),
            "non-recursive",
        ),
        (
            json!({"HostConfig": {"Mounts": [{"Type": "volume", "Source": "v", "Target": "/s",
                "BindOptions": {"CreateMountpoint": true}}]}}),
            "missing source is a bind's",
        ),
        (
            json!({"HostConfig": {"Mounts": [{"Type": "bind", "Source": "/srv", "Target": "/v",
                "VolumeOptions": {"NoCopy": true}}]}}),
            "no-copy is a volume's",
        ),
        (
            json!({"HostConfig": {"Mounts": [{"Type": "tmpfs", "Target": "/t",
                "TmpfsOptions": {"Options": [["noexec"]]}}]}}),
            "TmpfsOptions.Options",
        ),
    ];
    for (changes, named) in cases {
        let error = create(&with(&changes), None).unwrap_err();
        assert_eq!(error.status, hyper::StatusCode::BAD_REQUEST, "{changes}");
        assert!(
            error.message.contains(named),
            "{changes}: {}",
            error.message
        );
    }
}

/// Docker's anonymous volumes: `Volumes` (`docker run -v /data`), a `Binds`
/// entry `/path` and a `Mounts` volume without a `Source` all ask for one
/// (the lone place among the volumes, named at the create); a `Volumes`
/// place a bind, a mount or a tmpfs covers asks for none.
#[test]
fn anonymous_volumes_are_asked_for() {
    let volumes = |body: serde_json::Value| {
        let body: super::CreateBody = serde_json::from_value(body).unwrap();
        super::request(&body, None).unwrap().request.volume
    };
    assert_eq!(
        volumes(json!({"Image": "i", "Volumes": {"/data": {}, "/cache/": {}}})),
        ["/cache/", "/data"]
    );
    assert_eq!(
        volumes(json!({"Image": "i", "HostConfig": {"Binds": ["/data:ro"]}})),
        ["/data:ro"]
    );
    assert_eq!(
        volumes(json!({"Image": "i", "HostConfig": {"Mounts": [
            {"Type": "volume", "Target": "/data", "ReadOnly": true}]}})),
        ["/data:ro"]
    );
    assert_eq!(
        volumes(
            json!({"Image": "i", "Volumes": {"/data": {}, "/run/": {}, "/v": {}},
            "HostConfig": {"Binds": ["/srv:/data/"], "Tmpfs": {"/run": ""},
                "Mounts": [{"Type": "volume", "Source": "v", "Target": "/v"}]}})
        ),
        ["/srv:/data/", "v:/v"],
        "each covered"
    );
    assert_eq!(
        volumes(json!({"Image": "i", "Volumes": {"/data": {}},
            "HostConfig": {"Binds": ["db:/data:nocopy"]}})),
        ["db:/data:nocopy"],
        "covered whatever its modes"
    );
}

/// A `Binds` entry's host path is one to make if missing (Docker's daemon
/// makes it); a named volume's or an anonymous one's isn't.
#[test]
fn a_bind_s_host_path_is_made_if_missing() {
    for (spec, source) in [
        ("/host/dir:/x", Some("/host/dir")),
        ("/host/dir:/x:ro", Some("/host/dir")),
        ("name:/x", None),
        ("/data", None),
        ("/data:ro", None),
    ] {
        assert_eq!(
            shipyard_engine::run::bind_source(spec),
            source.map(PathBuf::from),
            "{spec}"
        );
    }
}

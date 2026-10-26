use std::time::{Duration, SystemTime};

use crate::containers::Network;
use crate::containers::tests::versions::V3_RECORD;

use super::*;

/// The released version 3 record (every field set), exited, as Docker's
/// `inspect` shows it.
fn gathered() -> containers::Gathered {
    let record = Record::parse(V3_RECORD.as_bytes()).unwrap();
    containers::Gathered {
        state: containers::ContainerState::Exited {
            code: Some(1),
            oom: None,
            finished: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_760_000_200)),
        },
        record,
        pid: None,
        log_dir: Some("/logs/1".to_owned()),
        health: None,
        network_lost: None,
    }
}

/// The released record, pinned as Docker's `inspect` shows it: every key
/// Docker has, Docker's zero values where Shipyard has nothing (an empty
/// string, `0`, `null`), `/` before the name, times in nanoseconds.
#[test]
fn a_record_is_inspected_as_docker_shows_a_container() {
    let shown = serde_json::to_value(docker_view(gathered(), &[], &Extras::default())).unwrap();
    let mut expected = serde_json::json!({
        "Id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "Created": "2025-10-09T08:53:20Z",
        "Path": "sh",
        "Args": [],
        "State": {
            "Status": "exited", "Running": false, "Paused": false, "Restarting": false,
            "OOMKilled": false, "Dead": false, "Pid": 0, "ExitCode": 1, "Error": "",
            "StartedAt": "2025-10-09T08:55:00.000000005Z",
            "FinishedAt": "2025-10-09T08:56:40Z"
        },
        "Image": "sha256:0123",
        "LogPath": "/logs/1",
        "Name": "/brave_tugboat",
        "RestartCount": 2,
        "Driver": "overlay", "GraphDriver": {"Name": "overlay", "Data": null},
        "Platform": "linux",
        "Mounts": [],
        "Config": {
            "Hostname": "0123456789ab", "Domainname": "", "User": "",
            "AttachStdin": false, "AttachStdout": true, "AttachStderr": true,
            "Tty": false, "OpenStdin": false, "StdinOnce": false,
            "Env": ["A=1"], "Cmd": ["sh"], "Image": "alpine", "WorkingDir": "",
            "Entrypoint": null, "Labels": {"tier": "web"},
            "ExposedPorts": {"80/tcp": {}},
            "Healthcheck": {
                "Test": ["CMD-SHELL", "test -e /ready"], "Interval": 2_000_000_000_u64,
                "Timeout": 1_500_000_000_u64, "StartPeriod": 7_000_000_000_u64,
                "StartInterval": 500_000_000_u64, "Retries": 4
            },
            "StopSignal": "SIGTERM", "StopTimeout": 10
        },
        "HostConfig": {
            "Binds": null,
            "PortBindings": {"80/tcp": [{"HostIp": "", "HostPort": "8080"}]},
            "RestartPolicy": {"Name": "on-failure", "MaximumRetryCount": 5},
            "AutoRemove": true, "NetworkMode": "pasta", "CapAdd": null, "CapDrop": null,
            "Dns": [], "DnsOptions": [], "DnsSearch": [], "ExtraHosts": null, "LogConfig": {"Type": "json-file", "Config": {}},
            "GroupAdd": null, "OomScoreAdj": 0, "ReadonlyRootfs": false, "SecurityOpt": null,
            "Privileged": false,
            "Memory": 0, "MemoryReservation": 0, "CpuPeriod": 0, "CpuQuota": 0,
            "CpusetCpus": "", "CpusetMems": "", "PidsLimit": null, "Ulimits": [],
            "Sysctls": {}
        },
        "NetworkSettings": {"Ports": {}, "Networks": {}}
    });
    // (Past `json!`'s depth there.)
    expected["HostConfig"]["IpcMode"] = "private".into();
    for path in ["ResolvConfPath", "HostnamePath", "HostsPath"] {
        expected[path] = "".into();
    }
    assert_eq!(shown, expected);
}

/// Docker's state words: a container waiting to restart, or stopping, is
/// still running for Docker (F72: one meaning, Docker's); its last exit is
/// kept; paused is running and paused.
#[test]
fn states_are_docker_s() {
    let state_of = |state: containers::ContainerState, pid: Option<i32>| {
        let mut gathered = gathered();
        gathered.state = state;
        gathered.pid = pid;
        docker_view(gathered, &[], &Extras::default()).state
    };
    let restarting = state_of(
        containers::ContainerState::Restarting {
            code: Some(3),
            finished: None,
            attempt: 1,
            at: None,
        },
        None,
    );
    assert_eq!(
        (restarting.status, restarting.running, restarting.restarting),
        ("restarting", true, true)
    );
    assert_eq!((restarting.exit_code, restarting.pid), (3, 0));
    let stopping = state_of(containers::ContainerState::Stopping, Some(42));
    assert_eq!(
        (stopping.status, stopping.running, stopping.pid),
        ("running", true, 42)
    );
    let paused = state_of(containers::ContainerState::Paused, Some(42));
    assert_eq!(
        (paused.status, paused.running, paused.paused),
        ("paused", true, true)
    );
    let created = state_of(containers::ContainerState::Created, Some(42));
    assert_eq!(
        (created.status, created.running, created.pid),
        ("created", false, 0)
    );
    let oom = state_of(
        containers::ContainerState::Exited {
            code: Some(137),
            oom: Some("killed".to_owned()),
            finished: None,
        },
        None,
    );
    assert!(oom.oom_killed);
}

/// Binds and volumes are Docker's mounts; a network's settings are kept
/// by name while it isn't up.
#[test]
fn mounts_and_networks_are_docker_s() {
    let mut gathered = gathered();
    gathered.record.config.volume =
        vec!["/home/me/app:/app:ro".to_owned(), "data:/data".to_owned()];
    let mut bridge = containers::Bridge::named("app");
    bridge.aliases = vec!["web".to_owned()];
    gathered.record.config.network = vec![Network::Bridges(vec![bridge])];
    let shown = docker_view(gathered, &[], &Extras::default());
    let mounts: Vec<(&str, &str, &str, &str, bool)> = shown
        .mounts
        .iter()
        .map(|m| {
            (
                m.kind,
                m.name.as_str(),
                m.source.as_str(),
                m.destination.as_str(),
                m.rw,
            )
        })
        .collect();
    assert_eq!(
        mounts,
        [
            ("bind", "", "/home/me/app", "/app", false),
            ("volume", "data", "", "/data", true)
        ]
    );
    assert_eq!(shown.host_config.network_mode, "app");
    assert_eq!(
        shown.network_settings.networks["app"].aliases.as_deref(),
        Some(&["web".to_owned()][..])
    );
    assert_eq!(shown.network_settings.networks["app"].ip_address, "");
}

/// Every source of `Config` and `HostConfig` set (the full version 3
/// config), running with its ports bound: each shown as Docker writes it.
#[test]
fn every_setting_is_shown_as_docker_s() {
    let record =
        Record::parse(crate::containers::tests::versions::v3_full_record().as_bytes()).unwrap();
    let mut gathered = gathered();
    gathered.record = record;
    gathered.record.ports = vec![crate::containers::BoundPort::new(
        "127.0.0.1".parse().unwrap(),
        41377,
        53,
        "udp",
    )];
    gathered.state = containers::ContainerState::Running { since: None };
    gathered.pid = Some(42);
    let shown = serde_json::to_value(docker_view(gathered, &[], &Extras::default())).unwrap();
    let config = &shown["Config"];
    assert_eq!(config["Hostname"], "h");
    assert_eq!(config["Domainname"], "d");
    assert_eq!(config["User"], "app:staff");
    assert_eq!(
        (&config["Tty"], &config["OpenStdin"]),
        (&true.into(), &true.into())
    );
    assert_eq!(config["WorkingDir"], "/w");
    assert_eq!(config["Entrypoint"], serde_json::json!(["/init"]));
    assert_eq!(config["Cmd"], serde_json::json!(["sh", "-c", "true"]));
    assert_eq!(config["StopSignal"], "SIGINT");
    assert_eq!(config["ExposedPorts"], serde_json::json!({"53/udp": {}}));
    let host = &shown["HostConfig"];
    assert_eq!(
        host["SecurityOpt"],
        serde_json::json!(["no-new-privileges", "seccomp=unconfined", "seccomp=/p.json"])
    );
    assert_eq!(
        host["Ulimits"],
        serde_json::json!([{"Name": "nofile", "Soft": 1024, "Hard": -1}])
    );
    assert_eq!(host["ExtraHosts"], serde_json::json!(["db:10.0.0.5"]));
    assert_eq!(host["Dns"], serde_json::json!(["1.1.1.1"]));
    assert_eq!(host["DnsSearch"], serde_json::json!(["example.com"]));
    assert_eq!(host["DnsOptions"], serde_json::json!(["ndots:2"]));
    assert_eq!(host["CapAdd"], serde_json::json!(["NET_ADMIN"]));
    assert_eq!(host["CapDrop"], serde_json::json!(["ALL"]));
    assert_eq!(host["GroupAdd"], serde_json::json!(["audio"]));
    assert_eq!(host["OomScoreAdj"], -100);
    assert_eq!(host["ReadonlyRootfs"], true);
    assert_eq!(
        (&host["Memory"], &host["MemoryReservation"]),
        (&1_048_576.into(), &524_288.into())
    );
    assert_eq!(
        (&host["CpuQuota"], &host["CpuPeriod"]),
        (&50_000.into(), &100_000.into())
    );
    assert_eq!(
        (&host["CpusetCpus"], &host["CpusetMems"]),
        (&"0-1".into(), &"0".into())
    );
    assert_eq!(host["PidsLimit"], 64);
    assert_eq!(
        host["Sysctls"],
        serde_json::json!({"net.ipv4.ip_forward": "1"})
    );
    assert_eq!(
        host["RestartPolicy"],
        serde_json::json!({"Name": "unless-stopped", "MaximumRetryCount": 0})
    );
    assert_eq!(host["NetworkMode"], "app");
    assert_eq!(
        host["PortBindings"],
        serde_json::json!({"53/udp": [{"HostIp": "127.0.0.1", "HostPort": ""}]})
    );
    assert_eq!(host["Binds"], serde_json::json!(["data:/data:ro"]));
    assert_eq!(
        shown["Mounts"],
        serde_json::json!([{"Type": "volume", "Name": "data", "Source": "", "Destination": "/data",
                            "Mode": "ro", "RW": false}])
    );
    // Up: its bound ports where it publishes them, and its PID.
    assert_eq!(
        shown["NetworkSettings"]["Ports"],
        serde_json::json!({"53/udp": [{"HostIp": "127.0.0.1", "HostPort": "41377"}]})
    );
    assert_eq!(shown["State"]["Pid"], 42);
    assert_eq!(shown["State"]["Status"], "running");
    assert_eq!(
        shown["NetworkSettings"]["Networks"]["app"]["Aliases"],
        serde_json::json!(["web"])
    );
}

/// What's read besides the record shows: the writable area's places and
/// the image's layers (`GraphDriver`), the image's own ports beside those
/// published, its tmpfs mounts (`HostConfig.Tmpfs` as given, and `Mounts`;
/// none: no `Tmpfs` key, as Docker's).
#[test]
fn what_s_read_besides_the_record_is_shown() {
    let extras = Extras {
        layers: Some(BTreeMap::from([
            (
                "LowerDir".to_owned(),
                "/store/layers/top/diff:/store/layers/base/diff".to_owned(),
            ),
            (
                "UpperDir".to_owned(),
                "/store/containers/ctr-1/upper".to_owned(),
            ),
        ])),
        image_ports: vec!["9090/tcp".to_owned()],
        image: None,
        tmpfs: vec![crate::run::TmpfsMount::parse("/run:size=64m,ro").unwrap()],
        network_from: None,
        ipc_from: None,
        stdin_once: true,
        etc_dir: Some(std::path::PathBuf::from("/run/shipyard/.runs/abc")),
    };
    let shown = serde_json::to_value(docker_view(gathered(), &[], &extras)).unwrap();
    assert_eq!(
        shown["GraphDriver"]["Data"]["LowerDir"],
        "/store/layers/top/diff:/store/layers/base/diff"
    );
    assert_eq!(
        shown["Config"]["ExposedPorts"],
        serde_json::json!({"80/tcp": {}, "9090/tcp": {}})
    );
    assert_eq!(
        shown["HostConfig"]["Tmpfs"],
        serde_json::json!({"/run": "size=64m,ro"})
    );
    let tmpfs = shown["Mounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|mount| mount["Type"] == "tmpfs")
        .expect("its tmpfs in Mounts");
    assert_eq!(tmpfs["Destination"], "/run");
    assert_eq!(tmpfs["RW"], false, "ro");
    // Its input closed once (F116: kept beside the record, not in memory).
    assert_eq!(shown["Config"]["StdinOnce"], true);
    // Its `/etc` files, while it's up (F121); empty without (never absent).
    assert_eq!(
        shown["ResolvConfPath"],
        "/run/shipyard/.runs/abc/resolv.conf"
    );
    assert_eq!(shown["HostnamePath"], "/run/shipyard/.runs/abc/hostname");
    assert_eq!(shown["HostsPath"], "/run/shipyard/.runs/abc/hosts");
    let none = serde_json::to_value(docker_view(gathered(), &[], &Extras::default())).unwrap();
    assert_eq!(none["Config"]["StdinOnce"], false);
    assert_eq!(none["ResolvConfPath"], "");
    assert!(
        none["HostConfig"].get("Tmpfs").is_none(),
        "{}",
        none["HostConfig"]
    );
}

/// Overlay's `lowerdir`: the layers given base first, shown top first.
#[test]
fn lower_dirs_are_top_first() {
    let dirs = [
        "/s/layers/base/diff",
        "/s/layers/mid/diff",
        "/s/layers/top/diff",
    ]
    .map(std::path::PathBuf::from);
    assert_eq!(
        lower_dir(&dirs),
        "/s/layers/top/diff:/s/layers/mid/diff:/s/layers/base/diff"
    );
    assert_eq!(lower_dir(&[]), "");
}

/// F101: what `inspect` shows of the process is what a start runs: its
/// own settings, its image's where it gave none, by the start's rules
/// (an entrypoint it gives drops the image's command; `--entrypoint ""`
/// keeps it; its variables win over the image's, in place).
#[test]
fn the_process_shown_is_the_one_a_start_runs() {
    let image = shipyard_image::RuntimeConfig {
        entrypoint: vec!["/entry.sh".to_owned()],
        cmd: vec!["nginx".to_owned(), "-g".to_owned()],
        env: vec![
            "PATH=/usr/bin".to_owned(),
            "MODE=image".to_owned(),
            "BARE".to_owned(),
        ],
        working_dir: Some("/srv".to_owned()),
        user: Some("www".to_owned()),
        ..shipyard_image::RuntimeConfig::default()
    };
    let shown = |entrypoint: Option<&str>, command: &[&str]| {
        let mut record = Record::parse(V3_RECORD.as_bytes()).unwrap();
        record.config.image = Some("nginx".to_owned());
        record.config.entrypoint = entrypoint.map(str::to_owned);
        record.config.command = command.iter().map(|&word| word.to_owned()).collect();
        record.config.env = vec!["MODE=mine".to_owned(), "MORE=1".to_owned()];
        record.config.workdir = None;
        record.config.user = None;
        let process = Process::of(&record, Some(&image));
        let path_args = crate::containers::shown_command(&record, Some(&image));
        (process, path_args)
    };
    // Nothing given: the image's entrypoint and command.
    let (process, path_args) = shown(None, &[]);
    assert_eq!(process.entrypoint, ["/entry.sh"]);
    assert_eq!(process.cmd, ["nginx", "-g"]);
    assert_eq!(path_args, ["/entry.sh", "nginx", "-g"]);
    assert_eq!(process.env, ["PATH=/usr/bin", "MODE=mine", "MORE=1"]);
    assert_eq!(
        (process.workdir.as_str(), process.user.as_str()),
        ("/srv", "www")
    );
    // A command given: the image's entrypoint, then it.
    let (process, _) = shown(None, &["sh"]);
    assert_eq!(
        (process.entrypoint, process.cmd),
        (vec!["/entry.sh".to_owned()], vec!["sh".to_owned()])
    );
    // An entrypoint given: the image's command goes too.
    let (process, path_args) = shown(Some("/bin/true"), &[]);
    assert_eq!(
        (process.entrypoint, process.cmd),
        (vec!["/bin/true".to_owned()], Vec::new())
    );
    assert_eq!(path_args, ["/bin/true"]);
    // `--entrypoint ""`: none, and the image's command kept.
    let (process, path_args) = shown(Some(""), &[]);
    assert_eq!(
        (process.entrypoint, process.cmd),
        (Vec::new(), vec!["nginx".to_owned(), "-g".to_owned()])
    );
    assert_eq!(path_args, ["nginx", "-g"]);
    // Without the image's config: what it was given.
    let mut record = Record::parse(V3_RECORD.as_bytes()).unwrap();
    record.config.command = vec!["true".to_owned()];
    record.config.entrypoint = None;
    assert_eq!(crate::containers::shown_command(&record, None), ["true"]);
}

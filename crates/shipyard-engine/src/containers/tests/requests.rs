//! A run's request kept in the record and read back, and the values a
//! record refuses.

use std::collections::BTreeMap;
use std::ffi::OsString;

use shipyard_runtime::process::{Rlimit, RlimitKind, RlimitValue};

use super::*;
use crate::ErrorKind;
use crate::network::{Joined, NetworkMode};
use crate::run::{CapabilityArg, DeviceArg, SecurityOpt};

/// Restart policies, in Docker's words, both ways; anything else refused.
#[test]
fn restart_policies_read_as_dockers() {
    use crate::containers::RestartPolicy;
    for (text, policy) in [
        ("no", RestartPolicy::No),
        ("always", RestartPolicy::Always),
        ("unless-stopped", RestartPolicy::UnlessStopped),
        ("on-failure", RestartPolicy::OnFailure { max: None }),
        ("on-failure:3", RestartPolicy::OnFailure { max: Some(3) }),
    ] {
        assert_eq!(text.parse::<RestartPolicy>().unwrap(), policy, "{text}");
        assert_eq!(policy.to_string(), text);
    }
    for bad in [
        "",
        "yes",
        "on-failure:",
        "on-failure:0",
        "on-failure:-1",
        "on-failure:x",
        "Always",
    ] {
        let err = bad.parse::<RestartPolicy>().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidRequest, "{bad}");
    }
}

/// Every value of a request is kept, and comes back.
/// A container without an image reads back without a pull policy: the
/// engine refuses `--pull` without an image, and a record's `missing` only
/// means it wasn't given.
#[test]
fn a_request_without_an_image_reads_back_without_a_pull() {
    let mut request = RunRequest::default();
    request.rootfs = Some("/".into());
    request.command = vec!["true".into()];
    let config = Config::from_request(&request).unwrap();
    assert_eq!(config.to_request(&name()).unwrap().pull, None);
}

#[test]
fn a_request_is_kept_whole() {
    let mut request = RunRequest::default();
    request.streams.interactive = true;
    request.streams.tty = true;
    request.env = vec!["A=1".into(), "FROM_CALLER".into(), "UNSET".into()];
    request.caller_env = vec![("FROM_CALLER".into(), "secret".into())];
    request.workdir = Some("/work".into());
    request.user = Some(("app".to_owned(), Some("staff".to_owned())));
    request.group_add = vec!["audio".to_owned()];
    request.ulimit = vec![
        Rlimit::new(
            RlimitKind::Core,
            RlimitValue::Finite(0),
            RlimitValue::Unlimited,
        )
        .unwrap(),
    ];
    request.oom_score_adj = Some(-5);
    request.device = vec![DeviceArg {
        host: "/dev/fuse".into(),
        container: Some("/dev/f".into()),
        access: shipyard_runtime::devices::Access::ALL,
    }];
    request.device_cgroup_rule = vec![
        serde_json::from_str(r#"{"allow":true,"kind":"Char","major":10,"minor":null,"access":3}"#)
            .unwrap(),
    ];
    request.stop_signal = "SIGUSR1".parse().unwrap();
    request.stop_timeout = 3;
    request.hostname = Some("box".to_owned());
    request.domainname = Some("example".to_owned());
    request.sysctl = vec![("net.ipv4.ip_forward".to_owned(), "1".to_owned())];
    request.network = vec![NetworkMode::Bridges(vec![Joined {
        name: "web".to_owned(),
        ip: Some("10.0.0.5".parse().unwrap()),
        ip6: Some("fd00::5".parse().unwrap()),
        mac: Some([2, 0, 0, 0, 0, 0xab]),
        aliases: vec!["db".to_owned()],
    }])];
    request.name = Some("app".to_owned());
    request.network_alias = vec!["alias".to_owned()];
    request.ip = Some("10.0.0.6".parse().unwrap());
    request.ip6 = Some("fd00::6".parse().unwrap());
    request.mac_address = Some([2, 0, 0, 0, 0, 0xcd]);
    request.add_host = vec![shipyard_network::files::HostEntry {
        name: "db".to_owned(),
        ip: "10.0.0.9".parse().unwrap(),
    }];
    request.dns = vec!["1.1.1.1".parse().unwrap()];
    request.dns_search = vec!["example".to_owned()];
    request.dns_option = vec!["ndots:2".to_owned()];
    request.publish = vec![shipyard_network::ports::PortRequest {
        host_ip: Some("127.0.0.1".parse().unwrap()),
        host_port: None,
        container_port: 53,
        protocol: shipyard_network::ports::Protocol::Udp,
    }];
    request.publish_all = true;
    request.image = Some("alpine:3.20".to_owned());
    request.entrypoint = Some("".into());
    request.pull = Some(crate::image::PullPolicy::Never);
    request.read_only = true;
    request.cap_add = vec![CapabilityArg::One("NET_ADMIN".parse().unwrap())];
    request.cap_drop = vec![CapabilityArg::All];
    request.security_opt = vec![
        SecurityOpt::NoNewPrivileges(false),
        SecurityOpt::SeccompProfile("/p.json".into()),
        SecurityOpt::SeccompUnconfined,
    ];
    request.uidmap = vec![shipyard_runtime::isolation::IdMap {
        container: 0,
        host: 1000,
        size: 1,
    }];
    request.gidmap = request.uidmap.clone();
    request.limits.resources.as_mut().unwrap().pids_max = Some(64);
    request.volume = vec!["data:/data:ro".to_owned()];
    request.command = vec!["sh".into(), "-c".into(), "exit 3".into()];
    request.restart = crate::containers::RestartPolicy::OnFailure { max: Some(3) };
    request.health = Some(crate::health::HealthCheck {
        interval_ms: 1500,
        retries: 5,
        ..crate::health::HealthCheck::shell("curl -f localhost")
    });
    let config = Config::from_request(&request).unwrap();
    // The caller's value is kept; an unset one is left out, as a run does.
    assert_eq!(config.env, ["A=1", "FROM_CALLER=secret"]);
    assert_eq!(config.stop_signal, "SIGUSR1");
    assert_eq!(config.mac_address.as_deref(), Some("02:00:00:00:00:cd"));
    let back = config.to_request(&Name::parse("app").unwrap()).unwrap();
    assert!(back.caller_env.is_empty());
    assert_eq!(back.name.as_deref(), Some("app"), "the record's name");
    // The request that comes back is the one asked, field for field, but
    // for what the record resolved: the caller's environment, read into
    // `env`. (Every field is named: a new one must be compared too.)
    let mut expected = request.clone();
    expected.env = vec!["A=1".into(), "FROM_CALLER=secret".into()];
    let RunRequest {
        default_mode: _,
        detached: _,
        container: _,
        caller_env: _,
        streams: _,
        env: _,
        workdir: _,
        user: _,
        group_add: _,
        ulimit: _,
        oom_score_adj: _,
        device: _,
        device_cgroup_rule: _,
        stop_signal: _,
        stop_timeout: _,
        restart: _,
        health: _,
        no_isolation: _,
        hostname: _,
        domainname: _,
        sysctl: _,
        network: _,
        name: _,
        network_alias: _,
        ip: _,
        ip6: _,
        mac_address: _,
        add_host: _,
        dns: _,
        dns_search: _,
        dns_option: _,
        publish: _,
        publish_all: _,
        rootfs: _,
        image: _,
        entrypoint: _,
        pull: _,
        read_only: _,
        cap_add: _,
        cap_drop: _,
        security_opt: _,
        uidmap: _,
        gidmap: _,
        bundle: _,
        limits: _,
        volume: _,
        command: _,
    } = &expected;
    macro_rules! same {
        ($field:ident) => {
            assert_eq!(
                format!("{:?}", back.$field),
                format!("{:?}", expected.$field),
                stringify!($field)
            );
        };
    }
    same!(streams);
    same!(env);
    same!(workdir);
    same!(user);
    same!(group_add);
    same!(ulimit);
    same!(oom_score_adj);
    same!(device);
    same!(device_cgroup_rule);
    same!(stop_signal);
    same!(stop_timeout);
    same!(restart);
    same!(health);
    same!(no_isolation);
    same!(hostname);
    same!(domainname);
    same!(sysctl);
    same!(network);
    same!(name);
    same!(network_alias);
    same!(ip);
    same!(ip6);
    same!(mac_address);
    same!(add_host);
    same!(dns);
    same!(dns_search);
    same!(dns_option);
    same!(publish);
    same!(publish_all);
    same!(rootfs);
    same!(image);
    same!(entrypoint);
    same!(pull);
    same!(read_only);
    same!(cap_add);
    same!(cap_drop);
    same!(security_opt);
    same!(uidmap);
    same!(gidmap);
    same!(bundle);
    same!(limits);
    same!(volume);
    same!(command);
    assert_eq!(Config::from_request(&back).unwrap(), config);
    // And through JSON.
    let json = serde_json::to_string(&config).unwrap();
    assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), config);
    // The bundle and the root filesystem, which can't go with an image.
    let mut bundle = RunRequest::default();
    bundle.bundle = Some("/b".into());
    bundle.rootfs = Some("/r".into());
    let kept = Config::from_request(&bundle).unwrap();
    let back = kept.to_request(&Name::parse("b").unwrap()).unwrap();
    assert_eq!(
        (back.bundle, back.rootfs),
        (Some("/b".into()), Some("/r".into()))
    );
}

/// What a record can't keep is refused: JSON has no bytes.
#[test]
fn a_value_that_isnt_utf8_is_refused() {
    use std::os::unix::ffi::OsStringExt as _;
    let mut request = RunRequest::default();
    request.command = vec![OsString::from_vec(vec![0xff])];
    let err = Config::from_request(&request).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidRequest);
    let mut request = RunRequest::default();
    request.limits.resources = Err("--memory 1x: not a size".to_owned());
    assert_eq!(
        Config::from_request(&request).unwrap_err().kind(),
        ErrorKind::InvalidRequest
    );
}

/// A record's values this Shipyard doesn't know are the record's fault.
#[test]
fn a_config_with_unknown_values_is_storages_error() {
    let mut config = config();
    config.stop_signal = "SIGNOPE".to_owned();
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
    let mut config = self::config();
    config.cap_add = vec!["CAP_NOPE".to_owned()];
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
    let mut config = self::config();
    config.mac_address = Some("02:00".to_owned());
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
}

/// A bare `--env` name a run would refuse is refused here too, not left
/// out quietly.
#[test]
fn a_bare_env_name_a_run_refuses_is_refused() {
    for bad in ["", "TOKEN secret"] {
        let mut request = RunRequest::default();
        request.env = vec![bad.into()];
        let err = Config::from_request(&request).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidRequest, "{bad:?}");
        assert!(!err.to_string().contains("secret"), "{err}");
    }
}

/// The cgroup limits are the record's own type: every field of the
/// runtime's is kept, and a field the runtime adds fails here, until the
/// record keeps it (and its version says so).
#[test]
fn every_cgroup_limit_is_kept() {
    use shipyard_runtime::resources::{CpuMax, IoMax, Limit, Resources};
    let fields: Vec<String> = serde_json::to_value(Resources::default())
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        fields,
        [
            "cpu_max",
            "cpu_weight",
            "cpuset_cpus",
            "cpuset_mems",
            "io_max",
            "io_weight",
            "memory_low",
            "memory_max",
            "pids_max",
            "swap_is_default",
            "swap_max",
            "unified"
        ],
        "a new limit: keep it in CgroupLimits"
    );
    let io_fields: Vec<String> = serde_json::to_value(IoMax::device(8, 0))
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        io_fields,
        ["major", "minor", "rbps", "riops", "wbps", "wiops"]
    );
    let mut resources = Resources::default();
    resources.memory_max = Some(1 << 30);
    resources.memory_low = Some(1 << 20);
    resources.swap_max = Some(Limit::Max);
    resources.swap_is_default = true;
    resources.cpu_max = Some(CpuMax {
        quota: Some(50_000),
        period: None,
    });
    resources.cpu_weight = Some(200);
    resources.cpuset_cpus = Some("0-1".to_owned());
    resources.cpuset_mems = Some("0".to_owned());
    resources.pids_max = Some(64);
    resources.io_weight = Some(50);
    let mut io = IoMax::device(8, 0);
    io.rbps = Some(1);
    io.wbps = Some(2);
    io.riops = Some(3);
    io.wiops = Some(4);
    resources.io_max = vec![io];
    resources.unified = BTreeMap::from([("memory.high".to_owned(), "1G".to_owned())]);
    let mut request = RunRequest::default();
    request.limits.resources = Ok(resources.clone());
    let config = Config::from_request(&request).unwrap();
    let back = config.to_request(&name()).unwrap();
    assert_eq!(back.limits.resources.unwrap(), resources);
}

/// Values in a record this Shipyard doesn't know: the record's fault.
#[test]
fn unknown_stored_values_are_storages_error() {
    let mut config = config();
    config.pull = "sometimes".to_owned();
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
    let mut config = self::config();
    config.ulimit = vec![Ulimit {
        kind: "nope".to_owned(),
        soft: None,
        hard: None,
    }];
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
    let mut config = self::config();
    config.publish = vec![Port {
        host_ip: None,
        host_port: None,
        container_port: 80,
        protocol: "sctp".to_owned(),
    }];
    assert_eq!(
        config.to_request(&name()).unwrap_err().kind(),
        ErrorKind::Storage
    );
}

use super::*;

fn config(name: &str, created: u64) -> NetworkConfig {
    serde_json::from_value(serde_json::json!({
        "name": name, "subnet": "10.250.1.0/24", "gateway": "10.250.1.1",
        "bridge": "shy-test", "internal": false, "created": created
    }))
    .unwrap()
}

fn options(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// Docker's bridge options pass at their defaults; Shipyard's own turn its
/// features on; another value, or another key, is refused.
#[test]
fn options_are_honoured_or_refused() {
    let defaults: Vec<(&str, &str)> = DOCKER_DEFAULTS.to_vec();
    assert_eq!(
        check_options(&options(&defaults)).unwrap(),
        Honoured::default()
    );
    assert_eq!(
        check_options(&options(&[(ISOLATE_HOST, "true"), (CONNTRACK_LIMIT, "1")])).unwrap(),
        Honoured {
            isolate_host: true,
            conntrack_limit: true
        }
    );
    assert_eq!(
        check_options(&options(&[(ISOLATE_HOST, "false")])).unwrap(),
        Honoured::default()
    );
    for refused in [
        ("com.docker.network.bridge.name", "br0"),
        ("com.docker.network.bridge.enable_ip_masquerade", "false"),
        ("com.docker.network.bridge.enable_icc", "false"),
        ("com.docker.network.driver.mtu", "9000"),
        ("com.docker.network.bridge.host_binding_ipv4", "127.0.0.1"),
        ("com.docker.network.bridge.default_bridge", "true"),
        ("anything", "x"),
        (ISOLATE_HOST, "maybe"),
    ] {
        let error = check_options(&options(&[refused])).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidRequest, "{refused:?}");
    }
}

/// A network's ID is the one kept at its making; one made before has its
/// ID derived (64 hex characters, the same each time asked, another for
/// another name or second); an endpoint's, of its network and container.
#[test]
fn ids_are_kept_or_derived() {
    let kept = NetworkMeta {
        id: Some("ab".repeat(32)),
        ..NetworkMeta::default()
    };
    assert_eq!(network_id(&config("app", 1), &kept), "ab".repeat(32));
    let none = NetworkMeta::default();
    let id = network_id(&config("app", 1_760_000_000), &none);
    assert_eq!(id.len(), 64);
    assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(id, network_id(&config("app", 1_760_000_000), &none));
    assert_ne!(id, network_id(&config("app", 1_760_000_001), &none));
    assert_ne!(id, network_id(&config("ap", 1_760_000_000), &none));
    assert_ne!(builtin_id("host"), builtin_id("none"));
    assert_ne!(endpoint_id(&id, "c1"), endpoint_id(&id, "c2"));
    assert!(is_predefined("bridge") && is_predefined("host") && is_predefined("pasta"));
    assert!(!is_predefined("app"));
}

/// A network has Docker 29.8's keys (its list's: no `Containers`; its
/// inspect's: with them), its IPAM one entry a family, its labels and
/// options as kept, the default one marked.
#[test]
fn a_network_is_shown_as_docker_shows_it() {
    let meta = NetworkMeta {
        labels: options(&[("a", "b")]),
        options: options(&[("com.docker.network.driver.mtu", "1500")]),
        ..NetworkMeta::default()
    };
    let listed = serde_json::to_value(resource(&config("app", 1_760_000_000), meta, None)).unwrap();
    let mut keys: Vec<&String> = listed.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            "Attachable",
            "ConfigFrom",
            "ConfigOnly",
            "Created",
            "Driver",
            "EnableIPv4",
            "EnableIPv6",
            "IPAM",
            "Id",
            "Ingress",
            "Internal",
            "Labels",
            "Name",
            "Options",
            "Scope"
        ]
    );
    assert_eq!(listed["Created"], "2025-10-09T08:53:20Z");
    assert_eq!(listed["Driver"], "bridge");
    assert_eq!(
        listed["IPAM"],
        serde_json::json!({"Driver": "default", "Options": null,
                           "Config": [{"Subnet": "10.250.1.0/24", "Gateway": "10.250.1.1"}]})
    );
    assert_eq!(listed["Labels"], serde_json::json!({"a": "b"}));
    assert_eq!(listed["ConfigFrom"], serde_json::json!({"Network": ""}));
    let default = resource(&config(DEFAULT_NETWORK, 1), NetworkMeta::default(), None);
    assert_eq!(
        default
            .options
            .get("com.docker.network.bridge.default_bridge")
            .map(String::as_str),
        Some("true")
    );
    let inspected = resource(
        &config("app", 1),
        NetworkMeta::default(),
        Some(BTreeMap::new()),
    );
    assert_eq!(
        serde_json::to_value(inspected).unwrap()["Containers"],
        serde_json::json!({})
    );
}

/// The built-ins, as Docker shows its own: `none`'s driver is `null`, no
/// subnets.
#[test]
fn built_ins_are_docker_s() {
    let none = serde_json::to_value(builtin("none")).unwrap();
    assert_eq!(none["Driver"], "null");
    assert_eq!(none["IPAM"]["Config"], serde_json::Value::Null);
    assert_eq!(none["Created"], crate::docker::ZERO_TIME);
    assert_eq!(builtin("host").driver, "host");
    assert_eq!(builtin("pasta").driver, "pasta");
}

/// No network is named `default` (Docker keeps it for the default one),
/// through the CLI either: refused before anything is made (11.6b's
/// clean-code review: the API alone refused it).
#[test]
fn no_network_is_named_default() {
    let (engine, _, dir) = crate::testing::engine_on_a_new_store("network-default");
    let request = NetworkCreate {
        name: "default".to_owned(),
        ..NetworkCreate::default()
    };
    let refused = create_network(&engine, &request).unwrap_err();
    assert_eq!(refused.kind(), crate::ErrorKind::InvalidRequest);
    let _ = std::fs::remove_dir_all(dir);
}

use super::*;

fn body(json: serde_json::Value) -> CreateBody {
    serde_json::from_value(json).unwrap()
}

/// Docker's create body, as its CLI sends it, mapped: a subnet and a
/// gateway a family, labels and options kept, Docker's defaults passing.
#[test]
fn docker_s_create_is_read() {
    let request = network_create(body(serde_json::json!({
        "Name": "app", "CheckDuplicate": true, "Driver": "bridge", "Scope": "local",
        "Internal": true, "Attachable": false, "Ingress": false, "ConfigOnly": false,
        "ConfigFrom": {"Network": ""}, "EnableIPv6": true,
        "IPAM": {"Driver": "default", "Options": {}, "Config": [
            {"Subnet": "10.250.1.0/24", "Gateway": "10.250.1.1"},
            {"Subnet": "fd00:1::/64"}
        ]},
        "Options": {"com.docker.network.driver.mtu": "1500"},
        "Labels": {"com.docker.compose.project": "web"}
    })))
    .unwrap();
    assert_eq!(request.name, "app");
    assert!(request.internal && request.ipv6);
    assert_eq!(
        request.subnets,
        [
            "10.250.1.0/24".parse().unwrap(),
            "fd00:1::/64".parse().unwrap()
        ]
    );
    assert_eq!(
        request.gateways,
        ["10.250.1.1".parse::<std::net::IpAddr>().unwrap()]
    );
    assert_eq!(request.labels["com.docker.compose.project"], "web");
    assert_eq!(request.options["com.docker.network.driver.mtu"], "1500");
    let bare = network_create(body(serde_json::json!({"Name": "plain"}))).unwrap();
    assert!(bare.subnets.is_empty() && !bare.internal && !bare.ipv6);
}

/// What a network of Shipyard's can't be is refused, never dropped.
#[test]
fn the_default_s_name_is_refused() {
    let refused = network_create(body(serde_json::json!({"Name": "default"}))).unwrap_err();
    assert_eq!(refused.status, hyper::StatusCode::FORBIDDEN);
    assert!(network_create(body(serde_json::json!({"Name": "defaults"}))).is_ok());
}

#[test]
fn what_a_bridge_can_t_be_is_refused() {
    for refused in [
        serde_json::json!({"Name": "x", "Driver": "overlay"}),
        serde_json::json!({"Name": "x", "Scope": "swarm"}),
        serde_json::json!({"Name": "x", "Attachable": true}),
        serde_json::json!({"Name": "x", "Ingress": true}),
        serde_json::json!({"Name": "x", "ConfigOnly": true}),
        serde_json::json!({"Name": "x", "ConfigFrom": {"Network": "other"}}),
        serde_json::json!({"Name": "x", "EnableIPv4": false}),
        serde_json::json!({"Name": "x", "IPAM": {"Driver": "custom"}}),
        serde_json::json!({"Name": "x", "IPAM": {"Options": {"a": "b"}}}),
        serde_json::json!({"Name": "x", "IPAM": {"Config": [{"Subnet": "10.1.0.0/16", "IPRange": "10.1.1.0/24"}]}}),
        serde_json::json!({"Name": "x", "IPAM": {"Config": [{"AuxiliaryAddresses": {"a": "10.1.0.5"}}]}}),
        serde_json::json!({"Name": "x", "IPAM": {"Config": [{"Subnet": "not a subnet"}]}}),
        serde_json::json!({"Name": "x", "IPAM": {"Config": [{"Gateway": "nope"}]}}),
    ] {
        assert_eq!(
            network_create(body(refused.clone())).unwrap_err().status,
            StatusCode::BAD_REQUEST,
            "{refused}"
        );
    }
}

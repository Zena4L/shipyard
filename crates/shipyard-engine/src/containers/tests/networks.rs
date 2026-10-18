//! The networks a record joins, gains and loses.

use super::*;

/// `network connect`'s record: a network added last; the default network
/// said first if none was; a lone network's addresses kept as its own; one
/// joined already, or past the most, refused; and `disconnect`'s: one left,
/// the last refused, one it isn't on not found.
#[test]
fn a_record_gains_and_loses_bridge_networks() {
    use crate::containers::config::{Bridge, Network};
    let names = |config: &Config| -> Vec<String> {
        config
            .network
            .iter()
            .flat_map(|network| match network {
                Network::Bridges(bridges) => {
                    bridges.iter().map(|bridge| bridge.name.clone()).collect()
                }
                _ => vec!["?".to_owned()],
            })
            .collect()
    };
    let mut plain = config();
    plain.network = Vec::new();
    plain.ip = Some("10.88.0.9".parse().unwrap());
    let joined = plain.with_bridge(Bridge::named("app"), "bridge").unwrap();
    assert_eq!(names(&joined), ["bridge", "app"]);
    assert_eq!(joined.ip, None, "its address moved to its network");
    let Network::Bridges(first) = &joined.network[0] else {
        panic!("{:?}", joined.network);
    };
    assert_eq!(first[0].ip, Some("10.88.0.9".parse().unwrap()));
    let twice = joined
        .with_bridge(Bridge::named("app"), "bridge")
        .unwrap_err();
    assert_eq!(twice.kind(), crate::ErrorKind::Exists, "{twice:#}");
    let left = joined.without_bridge("bridge", "bridge").unwrap();
    assert_eq!(names(&left), ["app"]);
    let only = left.without_bridge("app", "bridge").unwrap_err();
    assert_eq!(only.kind(), crate::ErrorKind::InvalidRequest, "{only:#}");
    let not_on = left.without_bridge("db", "bridge").unwrap_err();
    assert_eq!(not_on.kind(), crate::ErrorKind::NotFound, "{not_on:#}");
    assert_eq!(
        plain.without_bridge("bridge", "bridge").unwrap_err().kind(),
        crate::ErrorKind::InvalidRequest,
        "the default, alone"
    );
    // At most 100.
    let mut many = config();
    many.network = vec![Network::Bridges(
        (0..crate::run::MAX_NETWORKS)
            .map(|at| Bridge::named(&format!("n{at}")))
            .collect(),
    )];
    assert!(
        many.with_bridge(Bridge::named("one-more"), "bridge")
            .is_err()
    );
    // Not bridges: refused.
    let mut pasta = config();
    pasta.network = vec![Network::Pasta];
    assert!(pasta.with_bridge(Bridge::named("app"), "bridge").is_err());
    assert!(pasta.without_bridge("app", "bridge").is_err());
}

/// What `network rm` asks of a record: the networks it joins, its default
/// one if it says none, and never one through the host, a loopback or
/// pasta.
#[test]
fn a_record_says_which_networks_it_joins() {
    use crate::containers::config::{Bridge, Network};
    let mut plain = config();
    plain.network = Vec::new();
    assert!(plain.joins("bridge", "bridge"), "the default, unsaid");
    assert!(!plain.joins("app", "bridge"));
    let joined = plain.with_bridge(Bridge::named("app"), "bridge").unwrap();
    assert!(joined.joins("app", "bridge"));
    assert!(joined.joins("bridge", "bridge"));
    assert!(!joined.joins("db", "bridge"));
    let left = joined.without_bridge("app", "bridge").unwrap();
    assert!(!left.joins("app", "bridge"), "disconnected");
    for network in [Network::Host, Network::None, Network::Pasta] {
        let mut own = config();
        own.network = vec![network];
        assert!(!own.joins("bridge", "bridge"), "{:?}", own.network);
    }
}

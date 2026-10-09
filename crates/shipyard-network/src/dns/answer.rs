//! What a network's DNS server answers: the names of the containers on the
//! network, and of those on the other networks the asker is on too (it
//! reaches them there), and the names for their addresses; everything else
//! is the upstream resolvers' to answer (or, on an internal network, no
//! one's).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::message::{self, CLASS_ANY, CLASS_IN, Name, Query, RData, rcode, rtype};
use crate::ipnet::IpNet;
use crate::store::Lease;

/// The containers' names on one network, and their addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    /// Each name (in lowercase), and its container's IPv4 address.
    addresses: HashMap<String, Ipv4Addr>,
    /// Each name, and its container's IPv6 address (on a network with
    /// IPv6).
    addresses6: HashMap<String, Ipv6Addr>,
    /// Each address, and its container's first name.
    by_address: HashMap<IpAddr, String>,
}

impl Names {
    /// The names `leases` (the live ones) answer to: each container's
    /// `--name`, its aliases and its run's ID. An address's name is its
    /// container's `--name`, or its run's ID ([`Lease::address_name`]).
    #[must_use]
    pub fn of(leases: &[Lease]) -> Names {
        let mut names = Names::default();
        for lease in leases {
            for ip in lease.addresses() {
                names.by_address.insert(ip, lease.address_name().to_owned());
                names.add(lease.names(), ip);
            }
        }
        names
    }

    /// Adds `names` for `ip`: the first is the address's name (unless it
    /// has one).
    pub fn add<'a>(&mut self, names: impl IntoIterator<Item = &'a str>, ip: IpAddr) {
        for name in names {
            self.by_address.entry(ip).or_insert_with(|| name.to_owned());
            let name = name.to_ascii_lowercase();
            match ip {
                IpAddr::V4(ip) => self.addresses.insert(name, ip).map(drop),
                IpAddr::V6(ip) => self.addresses6.insert(name, ip).map(drop),
            };
        }
    }

    /// The records of type `qtype` for the name `label` (in any case):
    /// `None` if it isn't one of these names; none at all for a type it has
    /// no record of (its `AAAA`, without IPv6).
    fn records(&self, label: &[u8], qtype: u16) -> Option<Vec<RData>> {
        let label = std::str::from_utf8(label).ok()?.to_ascii_lowercase();
        let v4 = self.addresses.get(&label).copied();
        let v6 = self.addresses6.get(&label).copied();
        if v4.is_none() && v6.is_none() {
            return None;
        }
        let a = v4.map(RData::A);
        let aaaa = v6.map(RData::Aaaa);
        Some(match qtype {
            rtype::A => a.into_iter().collect(),
            rtype::AAAA => aaaa.into_iter().collect(),
            rtype::ANY => a.into_iter().chain(aaaa).collect(),
            _ => Vec::new(),
        })
    }
}

/// The names a server knows for one asker: its own network's first, then
/// those of each other network the asker is on, in order. A name on more
/// than one answers its addresses on the first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Known {
    /// Each network's subnets (its IPv4 subnet, and IPv6 prefix if it has
    /// one), and its names.
    networks: Vec<(Vec<IpNet>, Names)>,
}

impl Known {
    /// What's known on the network with `subnets`, whose names are
    /// `names`.
    #[must_use]
    pub fn new(subnets: impl IntoIterator<Item = IpNet>, names: Names) -> Known {
        Known {
            networks: vec![(subnets.into_iter().collect(), names)],
        }
    }

    /// Adds another network the asker is on (after those already known).
    pub fn also(&mut self, subnets: impl IntoIterator<Item = IpNet>, names: Names) {
        self.networks.push((subnets.into_iter().collect(), names));
    }

    /// The records of type `qtype` for `label`, on the first network that
    /// has it.
    fn records(&self, label: &[u8], qtype: u16) -> Option<Vec<RData>> {
        self.networks
            .iter()
            .find_map(|(_, names)| names.records(label, qtype))
    }

    /// The names of the network `ip` is on, if it's one of them.
    fn network_of(&self, ip: IpAddr) -> Option<&Names> {
        self.networks
            .iter()
            .find(|(subnets, _)| subnets.iter().any(|subnet| subnet.contains(ip)))
            .map(|(_, names)| names)
    }
}

/// The network a server answers for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    /// The network's addresses: names for them are the server's to give.
    pub subnet: IpNet,
    /// Its IPv6 prefix, if it has IPv6.
    pub subnet6: Option<IpNet>,
    /// It has no way out: nothing is forwarded.
    pub internal: bool,
}

/// What to do with a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Send this response.
    Answer(Vec<u8>),
    /// Ask the upstream resolvers, and pass their response on.
    Forward,
}

/// The reply to `query`, from what's `known`; anything else is forwarded
/// if the server `forwards` for the asker.
///
/// - A container's name (one label, in any case): its address for an `A`
///   question, its IPv6 address for `AAAA`, both for `ANY`; for any other
///   type (or `AAAA`, without IPv6), no records but no error either, so a
///   resolver doesn't go on to ask the upstreams about it.
/// - A name for an address on one of the networks (`2.0.89.10.in-addr.arpa`,
///   or its IPv6 prefix's `….ip6.arpa`): its container's name, or "no such
///   name".
/// - Anything else is forwarded; refused if the server doesn't forward for
///   the asker (its network is internal, and so are the asker's others).
#[must_use]
pub fn answer(query: &Query, known: &Known, forwards: bool) -> Reply {
    let question = &query.question;
    let class_in = matches!(question.qclass, CLASS_IN | CLASS_ANY);
    if class_in
        && let Some(label) = question.name.single_label()
        && let Some(records) = known.records(label, question.qtype)
    {
        return Reply::Answer(message::response(query, rcode::NOERROR, true, &records));
    }
    if class_in
        && matches!(question.qtype, rtype::PTR | rtype::ANY)
        && let Some(ip) = reverse_address(&question.name)
        && let Some(network) = known.network_of(ip)
    {
        let name = network
            .by_address
            .get(&ip)
            .and_then(|name| Name::from_text(name).ok());
        return Reply::Answer(match name {
            Some(name) => message::response(query, rcode::NOERROR, true, &[RData::Ptr(name)]),
            None => message::response(query, rcode::NXDOMAIN, true, &[]),
        });
    }
    if !forwards {
        return Reply::Answer(message::response(query, rcode::REFUSED, false, &[]));
    }
    Reply::Forward
}

/// The address `name` asks about, if it's a reverse lookup's: IPv4's
/// `d.c.b.a.in-addr.arpa`, or IPv6's 32 hex digits, last first, and
/// `ip6.arpa`.
fn reverse_address(name: &Name) -> Option<IpAddr> {
    let labels = name.labels();
    let (address, zone) = labels.split_at_checked(labels.len().checked_sub(2)?)?;
    let [first, arpa] = zone else {
        return None;
    };
    if !arpa.eq_ignore_ascii_case(b"arpa") {
        return None;
    }
    if first.eq_ignore_ascii_case(b"in-addr") {
        let [d, c, b, a] = address else {
            return None;
        };
        let octet = |label: &[u8]| -> Option<u8> {
            // Decimal, without a sign or leading zeros (`01` isn't an
            // octet).
            let text = std::str::from_utf8(label).ok()?;
            if text.len() > 1 && text.starts_with('0') || !text.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            text.parse().ok()
        };
        return Some(IpAddr::V4(Ipv4Addr::new(
            octet(a)?,
            octet(b)?,
            octet(c)?,
            octet(d)?,
        )));
    }
    if first.eq_ignore_ascii_case(b"ip6") && address.len() == 32 {
        // One hex digit a label, the lowest first.
        let mut bits = 0u128;
        for label in address.iter().rev() {
            let [digit] = label.as_slice() else {
                return None;
            };
            let nibble = char::from(*digit).to_digit(16)?;
            bits = (bits << 4) | u128::from(nibble);
        }
        return Some(IpAddr::V6(Ipv6Addr::from(bits)));
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::dns::message::{Received, read_query, read_response};

    fn names() -> Names {
        let mut names = Names::default();
        names.add(
            ["db", "Database", "a1b2c3d4e5f6"],
            IpAddr::V4(Ipv4Addr::new(10, 89, 0, 2)),
        );
        names.add(["0f0f0f0f0f0f"], IpAddr::V4(Ipv4Addr::new(10, 89, 0, 3)));
        names
    }

    fn ask(name: &str, qtype: u16, internal: bool) -> Reply {
        let bytes = message::query(9, &Name::from_text(name).unwrap(), qtype);
        let Received::Query(query) = read_query(&bytes) else {
            panic!("not a query");
        };
        answer(&query, &known(), !internal)
    }

    /// The asker's network (`names()`), and another it's on: `back`, where
    /// `cache` is, and another `db` (the asker's network's comes first).
    fn known() -> Known {
        let mut own = names();
        own.add(
            ["db", "database"],
            IpAddr::V6("fd00:89::2".parse().unwrap()),
        );
        let mut known = Known::new(
            [
                "10.89.0.0/24".parse().unwrap(),
                "fd00:89::/64".parse().unwrap(),
            ],
            own,
        );
        let mut back = Names::default();
        back.add(["cache"], IpAddr::V4(Ipv4Addr::new(10, 90, 0, 5)));
        back.add(["db"], IpAddr::V4(Ipv4Addr::new(10, 90, 0, 9)));
        known.also(["10.90.0.0/24".parse().unwrap()], back);
        known
    }

    /// The response code and the records of an answer.
    fn answered(reply: Reply) -> (u8, bool, Vec<RData>) {
        let Reply::Answer(bytes) = reply else {
            panic!("forwarded");
        };
        let response = read_response(&bytes).unwrap();
        assert_eq!(response.header.id, 9);
        (
            response.header.rcode(),
            response.header.authoritative(),
            response
                .answers
                .into_iter()
                .map(|answer| answer.data)
                .collect(),
        )
    }

    #[test]
    fn a_containers_names_are_its_address_in_any_case() {
        let db = vec![RData::A(Ipv4Addr::new(10, 89, 0, 2))];
        for name in ["db", "DB", "database", "a1b2c3d4e5f6", "db."] {
            assert_eq!(
                answered(ask(name, rtype::A, false)),
                (rcode::NOERROR, true, db.clone()),
                "{name}"
            );
        }
        // ANY: its IPv4 address first (and its IPv6 one: see below).
        assert_eq!(answered(ask("a1b2c3d4e5f6", rtype::ANY, false)).2, db);
    }

    #[test]
    fn names_on_the_askers_other_networks_have_their_address_there() {
        assert_eq!(
            answered(ask("cache", rtype::A, false)).2,
            [RData::A(Ipv4Addr::new(10, 90, 0, 5))]
        );
        // On both: the asker's own network's address.
        assert_eq!(
            answered(ask("db", rtype::A, false)).2,
            [RData::A(Ipv4Addr::new(10, 89, 0, 2))]
        );
        // The other network's addresses have their names too.
        assert_eq!(
            answered(ask("5.0.90.10.in-addr.arpa", rtype::PTR, false)).2,
            [RData::Ptr(Name::from_text("cache").unwrap())]
        );
        assert_eq!(
            answered(ask("6.0.90.10.in-addr.arpa", rtype::PTR, false)).0,
            rcode::NXDOMAIN
        );
    }

    #[test]
    fn other_types_for_a_containers_name_have_no_records_and_no_error() {
        // No IPv6 for 0f0f…'s container.
        assert_eq!(
            answered(ask("0f0f0f0f0f0f", rtype::AAAA, false)),
            (rcode::NOERROR, true, Vec::new())
        );
        assert_eq!(
            answered(ask("db", 15, false)),
            (rcode::NOERROR, true, Vec::new())
        );
    }

    #[test]
    fn a_container_with_ipv6_has_its_aaaa() {
        let ip6: Ipv6Addr = "fd00:89::2".parse().unwrap();
        assert_eq!(
            answered(ask("database", rtype::AAAA, false)),
            (rcode::NOERROR, true, vec![RData::Aaaa(ip6)])
        );
        assert_eq!(
            answered(ask("db", rtype::ANY, false)).2,
            [RData::A(Ipv4Addr::new(10, 89, 0, 2)), RData::Aaaa(ip6)]
        );
        // Its address's name: 32 digits, the lowest first.
        let reverse = "2.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.9.8.0.0.0.0.d.f.ip6.arpa";
        assert_eq!(
            answered(ask(reverse, rtype::PTR, false)).2,
            [RData::Ptr(Name::from_text("db").unwrap())]
        );
        // Another address of the prefix: no such name.
        let other = reverse.replacen('2', "3", 1);
        assert_eq!(answered(ask(&other, rtype::PTR, true)).0, rcode::NXDOMAIN);
        // Not an address: forwarded.
        assert_eq!(ask("x.ip6.arpa", rtype::PTR, false), Reply::Forward);
    }

    #[test]
    fn a_networks_addresses_have_their_containers_first_name() {
        assert_eq!(
            answered(ask("2.0.89.10.in-addr.arpa", rtype::PTR, false)),
            (
                rcode::NOERROR,
                true,
                vec![RData::Ptr(Name::from_text("db").unwrap())]
            )
        );
        assert_eq!(
            answered(ask("3.0.89.10.IN-ADDR.ARPA", rtype::PTR, false)).2,
            vec![RData::Ptr(Name::from_text("0f0f0f0f0f0f").unwrap())]
        );
        // One of the network's with no container: no such name.
        assert_eq!(
            answered(ask("9.0.89.10.in-addr.arpa", rtype::PTR, true)),
            (rcode::NXDOMAIN, true, Vec::new())
        );
    }

    #[test]
    fn everything_else_is_forwarded_or_refused_on_an_internal_network() {
        for (name, qtype) in [
            ("example.com", rtype::A),
            ("web", rtype::A),
            ("db.example.com", rtype::A),
            // Not the network's address.
            ("2.0.0.10.in-addr.arpa", rtype::PTR),
            // Not an address.
            ("02.0.89.10.in-addr.arpa", rtype::PTR),
            ("2.0.89.10.ip6.arpa", rtype::PTR),
            ("x.0.89.10.in-addr.arpa", rtype::PTR),
        ] {
            assert_eq!(ask(name, qtype, false), Reply::Forward, "{name}");
            assert_eq!(
                answered(ask(name, qtype, true)),
                (rcode::REFUSED, false, Vec::new()),
                "{name}"
            );
        }
    }

    #[test]
    fn leases_name_their_containers() {
        let lease = |ip: &str, run: &str, name: Option<&str>, aliases: &[&str]| Lease {
            ip: ip.parse().unwrap(),
            mac: [2, 0x42, 0, 0, 0, 0],
            run: run.into(),
            hostname: run.into(),
            host_veth: "shyx".into(),
            ports: Vec::new(),
            name: name.map(str::to_owned),
            aliases: aliases.iter().map(|&alias| alias.to_owned()).collect(),
            interface: "eth0".into(),
            ip6: None,
        };
        let names = Names::of(&[
            lease("10.89.0.2", "a1b2c3d4e5f6", Some("db"), &["database"]),
            lease("10.89.0.3", "0f0f0f0f0f0f", None, &[]),
            // An alias, but no name: the address's name is the run's ID.
            lease("10.89.0.4", "abababababab", None, &["web"]),
        ]);
        let mut expected = Names::default();
        expected.add(
            ["db", "database", "a1b2c3d4e5f6"],
            IpAddr::V4(Ipv4Addr::new(10, 89, 0, 2)),
        );
        expected.add(["0f0f0f0f0f0f"], IpAddr::V4(Ipv4Addr::new(10, 89, 0, 3)));
        expected.add(
            ["abababababab", "web"],
            IpAddr::V4(Ipv4Addr::new(10, 89, 0, 4)),
        );
        assert_eq!(names, expected);
        // With IPv6, both addresses.
        let dual = Lease {
            ip6: Some("fd00:89::5".parse().unwrap()),
            ..lease("10.89.0.5", "cdcdcdcdcdcd", Some("v6"), &[])
        };
        let names = Names::of(&[dual]);
        assert_eq!(
            names.records(b"v6", rtype::ANY),
            Some(vec![
                RData::A(Ipv4Addr::new(10, 89, 0, 5)),
                RData::Aaaa("fd00:89::5".parse().unwrap())
            ])
        );
    }
}

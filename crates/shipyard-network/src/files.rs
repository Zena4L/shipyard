//! The files that tell a container about its network: `/etc/hosts`,
//! `/etc/hostname` and `/etc/resolv.conf`, written for each run as Docker
//! writes them, and bound into its root.
//!
//! A container with the host's network gets the host's files (its
//! resolvers are reachable from there, even one on 127.0.0.53). One with a
//! network of its own gets its own: its address and name, and the host's
//! resolvers without the loopback ones, which are the host's own and can't be
//! reached from a container (systemd-resolved's stub, say: its real upstreams
//! are read from `/run/systemd/resolve/resolv.conf` instead).

use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr};

/// Docker's resolvers for a container left with none.
pub const FALLBACK_RESOLVERS: [IpAddr; 2] = [
    IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
    IpAddr::V4(Ipv4Addr::new(8, 8, 4, 4)),
];

/// Where the host's resolvers are listed, the first that exists: systemd's
/// list of the real upstreams (behind its 127.0.0.53 stub), then the usual
/// file.
pub const HOST_RESOLV_CONF: [&str; 2] = ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"];

/// An extra line for `/etc/hosts` (`--add-host NAME:IP`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostEntry {
    /// The name.
    pub name: String,
    /// Its address.
    pub ip: IpAddr,
}

/// `--add-host`'s `NAME:IP` (the address may be IPv6: everything after the
/// first `:` is the address).
pub fn parse_host_entry(text: &str) -> Result<HostEntry, String> {
    let invalid = || format!("{text:?} isn't NAME:IP (like db:10.0.0.5)");
    let (name, ip) = text.split_once(':').ok_or_else(invalid)?;
    let ip: IpAddr = ip.parse().map_err(|_| invalid())?;
    let valid_name = !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    if !valid_name {
        return Err(format!("{text:?}: {name:?} isn't a host name"));
    }
    Ok(HostEntry {
        name: name.to_owned(),
        ip,
    })
}

/// A `--dns-search` domain or `--dns-option`: one word, as resolv.conf
/// takes it (a space or line break would start another).
pub fn parse_resolv_word(text: &str) -> Result<String, String> {
    if text.is_empty() || text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{text:?}: one word, without spaces"));
    }
    Ok(text.to_owned())
}

/// `--dns`, `--dns-search` and `--dns-option`: each replaces the host's.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DnsSettings {
    /// Resolvers.
    pub servers: Vec<IpAddr>,
    /// Search domains.
    pub search: Vec<String>,
    /// `options` (such as `ndots:2`).
    pub options: Vec<String>,
}

/// What a resolv.conf says.
#[derive(Debug, Default, PartialEq, Eq)]
struct Resolv {
    servers: Vec<IpAddr>,
    search: Vec<String>,
    options: Vec<String>,
}

fn parse_resolv(text: &str) -> Resolv {
    let mut resolv = Resolv::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        match words.next() {
            Some("nameserver") => {
                // An address with a scope (`fe80::1%eth0`) doesn't parse,
                // and is skipped: its interface is the host's.
                if let Some(Ok(ip)) = words.next().map(str::parse) {
                    resolv.servers.push(ip);
                }
            }
            // The last search or domain line wins (resolv.conf(5)).
            Some("search" | "domain") => resolv.search = words.map(str::to_owned).collect(),
            Some("options") => resolv.options.extend(words.map(str::to_owned)),
            _ => {}
        }
    }
    resolv
}

/// The resolvers a `resolv.conf` (its text) names, in order.
#[must_use]
pub fn nameservers(text: &str) -> Vec<IpAddr> {
    parse_resolv(text).servers
}

/// Whether a resolver at `ip` is one only the host's own network reaches:
/// on loopback (`::ffff:127.0.0.1` too), or IPv6 link-local (on one of the
/// host's links).
fn hosts_own(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unicast_link_local(),
    }
}

/// The container's `/etc/resolv.conf`, from the host's (`host`, its
/// text) and the run's settings. With a network of its own, the host's
/// loopback resolvers are left out, and [`FALLBACK_RESOLVERS`] used if none
/// is left; with the host's, the host's file as it is, unless a setting
/// replaces part of it.
#[must_use]
pub fn resolv_conf(host: &str, own_network: bool, settings: &DnsSettings) -> String {
    let given = settings != &DnsSettings::default();
    if !own_network && !given {
        return host.to_owned();
    }
    let mut resolv = parse_resolv(host);
    if !settings.servers.is_empty() {
        resolv.servers.clone_from(&settings.servers);
    } else if own_network {
        resolv.servers.retain(|ip| !hosts_own(*ip));
        if resolv.servers.is_empty() {
            resolv.servers = FALLBACK_RESOLVERS.to_vec();
        }
    }
    if !settings.search.is_empty() {
        resolv.search.clone_from(&settings.search);
    }
    if !settings.options.is_empty() {
        resolv.options.clone_from(&settings.options);
    }
    let mut text = String::from("# Written by shipyard for this container.\n");
    for server in &resolv.servers {
        let _ = writeln!(text, "nameserver {server}");
    }
    if !resolv.search.is_empty() {
        let _ = writeln!(text, "search {}", resolv.search.join(" "));
    }
    if !resolv.options.is_empty() {
        let _ = writeln!(text, "options {}", resolv.options.join(" "));
    }
    text
}

/// The container's `/etc/hosts`. With the host's network, the host's file
/// (`host`); with one of its own, the loopback names, and its hostname: on
/// its own `addresses` if it has some (a bridge network's IPv4 address, and
/// IPv6 one), else on `127.0.1.1` (as Debian names a host without one), so
/// that looking it up never goes to a DNS server. Then the extra entries.
#[must_use]
pub fn hosts(
    host: Option<&str>,
    hostname: &str,
    addresses: &[IpAddr],
    extra: &[HostEntry],
) -> String {
    let mut text = if let Some(host) = host {
        let mut text = host.to_owned();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text
    } else {
        let mut text =
            String::from("127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n");
        let none = [IpAddr::V4(Ipv4Addr::new(127, 0, 1, 1))];
        let addresses = if addresses.is_empty() {
            &none[..]
        } else {
            addresses
        };
        for address in addresses {
            let _ = writeln!(text, "{address}\t{hostname}");
        }
        text
    };
    for entry in extra {
        let _ = writeln!(text, "{}\t{}", entry.ip, entry.name);
    }
    text
}

/// The container's `/etc/hostname`.
#[must_use]
pub fn hostname(name: &str) -> String {
    format!("{name}\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    const SYSTEMD_STUB: &str = "nameserver 127.0.0.53\noptions edns0 trust-ad\nsearch lan\n";
    const UPSTREAMS: &str =
        "nameserver 192.168.1.1\nnameserver ::1\nnameserver 2001:db8::53\nsearch lan corp\n";

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn resolv_words() {
        assert_eq!(parse_resolv_word("ndots:2"), Ok("ndots:2".into()));
        for bad in ["", "a b", "a\nnameserver 1.1.1.1", "a\tb"] {
            assert!(parse_resolv_word(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn host_entries() {
        assert_eq!(
            parse_host_entry("db:10.0.0.5"),
            Ok(HostEntry {
                name: "db".into(),
                ip: ip("10.0.0.5")
            })
        );
        assert_eq!(parse_host_entry("v6:fd00::1").unwrap().ip, ip("fd00::1"));
        for bad in [
            "db",
            "db:",
            ":10.0.0.5",
            "db:nope",
            "-db:10.0.0.5",
            "a b:10.0.0.5",
        ] {
            assert!(parse_host_entry(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_hosts_resolvers_as_they_are_with_its_network() {
        assert_eq!(
            resolv_conf(SYSTEMD_STUB, false, &DnsSettings::default()),
            SYSTEMD_STUB
        );
    }

    #[test]
    fn loopback_resolvers_stay_on_the_host() {
        let own = resolv_conf(UPSTREAMS, true, &DnsSettings::default());
        assert_eq!(
            own,
            "# Written by shipyard for this container.\nnameserver 192.168.1.1\nnameserver \
             2001:db8::53\nsearch lan corp\n"
        );
        // Only the host's own (mapped loopback, link-local): Docker's fallback.
        let own_only = resolv_conf(
            "nameserver ::ffff:127.0.0.1\nnameserver fe80::1\n",
            true,
            &DnsSettings::default(),
        );
        assert!(own_only.contains("nameserver 8.8.8.8\n"), "{own_only}");
        assert!(!own_only.contains("fe80"), "{own_only}");
        // Only a stub: Docker's fallback.
        let fallback = resolv_conf(SYSTEMD_STUB, true, &DnsSettings::default());
        assert!(
            fallback.contains("nameserver 8.8.8.8\nnameserver 8.8.4.4\n"),
            "{fallback}"
        );
        assert!(fallback.contains("options edns0 trust-ad\n"), "{fallback}");
        assert!(!fallback.contains("127.0.0.53"), "{fallback}");
    }

    #[test]
    fn settings_replace_the_hosts() {
        let settings = DnsSettings {
            servers: vec![ip("10.0.0.53")],
            search: vec!["example.com".into()],
            options: vec!["ndots:2".into()],
        };
        for own in [false, true] {
            assert_eq!(
                resolv_conf(SYSTEMD_STUB, own, &settings),
                "# Written by shipyard for this container.\nnameserver 10.0.0.53\nsearch \
                 example.com\noptions ndots:2\n"
            );
        }
        // One setting: the rest is the host's (filtered).
        let servers = DnsSettings {
            servers: vec![ip("127.0.0.1")],
            ..DnsSettings::default()
        };
        let text = resolv_conf(UPSTREAMS, true, &servers);
        assert!(
            text.contains("nameserver 127.0.0.1\nsearch lan corp\n"),
            "{text}"
        );
    }

    #[test]
    fn hosts_files() {
        let extra = [HostEntry {
            name: "db".into(),
            ip: ip("10.0.0.5"),
        }];
        assert_eq!(
            hosts(None, "a1b2c3", &[ip("10.88.0.2")], &extra),
            "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n10.88.0.2\ta1b2c3\n\
             10.0.0.5\tdb\n"
        );
        assert_eq!(
            hosts(None, "a1b2c3", &[], &[]),
            "127.0.0.1\tlocalhost\n::1\tlocalhost ip6-localhost ip6-loopback\n127.0.1.1\ta1b2c3\n"
        );
        assert_eq!(
            hosts(Some("127.0.0.1 localhost"), "a1b2c3", &[], &extra),
            "127.0.0.1 localhost\n10.0.0.5\tdb\n"
        );
        assert!(
            hosts(None, "a1b2c3", &[ip("10.88.0.2"), ip("fd00:1::2")], &[])
                .ends_with("10.88.0.2\ta1b2c3\nfd00:1::2\ta1b2c3\n")
        );
        assert_eq!(hostname("a1b2c3"), "a1b2c3\n");
    }
}

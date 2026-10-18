//! Container networking for Shipyard: the engine's network layer.
//!
//! This crate is the netavark-equivalent the engine calls: it gives a
//! container's network namespace its interfaces, addresses and routes, from
//! outside, and keeps the firewall (`nf_tables`) that gives networks a way out
//! and publishes ports. It talks to the kernel over netlink with Shipyard's own
//! messages (its `netlink` module): no `ip` binary and no netlink library.
//! The messages are built and read on any UNIX (so they're tested and fuzzed
//! on macOS too); sending them needs Linux.
//!
//! Built in stage 7; see `docs/stages/07-networking.md`.

// UNIX only, like the rest of Shipyard.
#[cfg(not(unix))]
compile_error!("shipyard-network only supports UNIX platforms (Linux, macOS)");

#[cfg(target_os = "linux")]
pub mod attach;
pub mod conntrack;
pub mod dns;
pub mod files;
pub mod firewalls;
pub mod ipnet;
mod netlink;
pub mod nft;
pub mod observe;
pub mod pasta;
pub mod ports;
pub mod route;
pub mod store;

pub use ipnet::{Family, IpNet, IpNetError};
pub use netlink::{BuildError, NetlinkError, ParseError};

/// What the root tests against the real kernel share.
#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used, reason = "tests")]
pub(crate) mod linux_test {
    use std::fs::File;

    use nix::sched::{CloneFlags, unshare};

    /// Runs `test` in a new thread, in a new network namespace: what it
    /// changes is never the host's, and goes when the thread ends.
    pub(crate) fn in_new_netns(test: impl FnOnce() + Send + 'static) {
        std::thread::spawn(move || {
            unshare(CloneFlags::CLONE_NEWNET).unwrap();
            test();
        })
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    }

    /// This thread's network namespace.
    pub(crate) fn this_netns() -> File {
        File::open("/proc/thread-self/ns/net").unwrap()
    }

    /// Whether `name`, a reference tool the differential tests compare
    /// Shipyard with (`ip`, `nft`), is installed. When not, the test says so
    /// and returns; with `SHIPYARD_TEST_TOOLS=required` (CI, `test-linux.sh`)
    /// it fails instead. (As `shipyard-test-support`'s `reference_tool`,
    /// which this crate can't use: that crate depends on this one.)
    pub(crate) fn reference_tool(name: &str) -> bool {
        let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .chain(["/usr/sbin".into(), "/sbin".into()])
            .any(|dir| dir.join(name).is_file());
        if !found {
            assert!(
                std::env::var("SHIPYARD_TEST_TOOLS").as_deref() != Ok("required"),
                "`{name}` is required here (SHIPYARD_TEST_TOOLS), but isn't installed"
            );
            eprintln!("skipping: no `{name}` here");
        }
        found
    }
}

/// Entry points for the fuzz targets (`fuzz/`) into what isn't public: built
/// only by cargo-fuzz (`--cfg fuzzing`), never in a release.
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing {
    use crate::netlink::message::{Attrs, Messages};
    use crate::route::{Address, Link, Route};

    /// Reads `data` as what the kernel sends over a netlink socket:
    /// messages, their attributes (nested ones too, `depth` levels down),
    /// errors and dump ends, the links, addresses and routes rtnetlink
    /// describes, `nf_tables`' tables, chains and set elements, and
    /// conntrack's connections.
    pub fn netlink(data: &[u8]) {
        for message in Messages::new(data).flatten() {
            let _ = message.error();
            let _ = message.done();
            let _ = Link::parse(&message);
            let _ = Address::parse(&message);
            let _ = Route::parse(&message);
            let _ = crate::nft::fuzzing::parse(&message);
            let _ = crate::conntrack::parse(&message);
            // the family headers: nfgenmsg, ifaddrmsg, rtmsg, ifinfomsg
            for header in [4, 8, 12, 16] {
                if let Ok((_, attrs)) = message.split(header) {
                    walk(attrs, 4);
                }
            }
        }
        walk(Attrs::new(data), 4);
    }

    /// Reads `data` as `-p` and an image's `ExposedPorts` key.
    pub fn ports(data: &[u8]) {
        if let Ok(text) = std::str::from_utf8(data) {
            let _ = crate::ports::parse_publish(text);
            let _ = crate::ports::parse_exposed(text);
        }
    }

    /// Reads `data` as a DNS message, received as a query and as a
    /// response; a query read is answered, and the answer (Shipyard's own)
    /// must read back as a response to it.
    pub fn dns(data: &[u8]) {
        use crate::dns::answer::{Known, Names, Reply, answer};
        use crate::dns::message::{Received, read_name, read_query, read_response};
        let _ = read_response(data);
        for start in 0..data.len().min(32) {
            let _ = read_name(data, start);
        }
        let Received::Query(query) = read_query(data) else {
            return;
        };
        let net = |a: u8| {
            crate::IpNet::new(std::net::IpAddr::from([10, a, 0, 0]), 24)
                .unwrap_or_else(|_| unreachable!())
        };
        let ip6 = |last: u16| std::net::IpAddr::from([0xfd00, 0x89, 0, 0, 0, 0, 0, last]);
        let mut own = Names::default();
        own.add(
            ["db", "a1b2c3d4e5f6"],
            std::net::IpAddr::from([10, 89, 0, 2]),
        );
        own.add(["db"], ip6(2));
        let mut other = Names::default();
        other.add(["cache"], std::net::IpAddr::from([10, 90, 0, 5]));
        let prefix6 = crate::IpNet::new(ip6(0), 64).unwrap_or_else(|_| unreachable!());
        let mut known = Known::new([net(89), prefix6], own);
        known.also([net(90)], other);
        for forwards in [false, true] {
            if let Reply::Answer(bytes) = answer(&query, &known, forwards) {
                let read = read_response(&bytes).expect("an answer reads back");
                assert_eq!(read.header.id, query.header.id);
            }
        }
    }

    /// Reads `data` as each of `/proc/<pid>/net`'s socket tables.
    pub fn proc_net(data: &[u8]) {
        if let Ok(text) = std::str::from_utf8(data) {
            for table in crate::observe::Table::ALL {
                let _ = crate::observe::parse(text, table);
            }
        }
    }

    fn walk(attrs: Attrs<'_>, depth: u32) {
        for attr in attrs.flatten() {
            let _ = (
                attr.u8(),
                attr.u32(),
                attr.string(),
                attr.be16(),
                attr.be32(),
            );
            if depth > 0 {
                walk(attr.nested(), depth - 1);
            }
        }
    }
}

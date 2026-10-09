//! What the kernel sends over a netlink socket: messages, their attributes
//! (nested ones too), errors with extended ACKs, the links, addresses and
//! routes rtnetlink describes, nf_tables' tables, chains and set elements,
//! and conntrack's connections. A hostile or broken datagram must be an
//! error, never a read past its end or a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;
use shipyard_network::fuzzing;

fuzz_target!(|data: &[u8]| {
    fuzzing::netlink(data);
});

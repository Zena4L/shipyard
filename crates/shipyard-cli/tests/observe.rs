//! End-to-end test of reading a container's sockets (Linux, root): through
//! its init's `/proc/<pid>/net`, the container's own network namespace's
//! listeners and connections, and none of the host's.

#![cfg(target_os = "linux")]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};

use shipyard_network::observe::{self, Protocol, State};
use shipyard_test_support::net::{NetworkDir, container_init, in_netns};
use shipyard_test_support::{Shipyard, in_new_netns, start_command};

const SHIPYARD: Shipyard = Shipyard::at(env!("CARGO_BIN_EXE_shipyard"));

#[test]
#[ignore = "needs root"]
fn a_containers_sockets_are_read_through_its_init() {
    in_new_netns(|| {
        let networks = NetworkDir::new(SHIPYARD);
        let running = start_command(networks.command(&[
            "run",
            "--no-image",
            "--network",
            "bridge",
            "--name",
            "box",
            "sleep",
            "30",
        ]));
        let (init, netns) = container_init(running.pid(), 1);
        let ip = networks.address("bridge", "box");
        // A listener in the container's namespace (made by a thread joined
        // to it: the socket stays the namespace's), and one on the host.
        let inside = in_netns(&netns, || TcpListener::bind("0.0.0.0:7200").unwrap());
        let _udp = in_netns(&netns, || UdpSocket::bind("0.0.0.0:7201").unwrap());
        let _host = TcpListener::bind("0.0.0.0:7300").unwrap();
        // A connection from the host, held open.
        let client = TcpStream::connect(SocketAddr::from((ip, 7200))).unwrap();
        let (_accepted, from) = inside.accept().unwrap();

        let listening = observe::listening(init).unwrap();
        assert!(
            listening
                .iter()
                .any(|socket| socket.protocol == Protocol::Tcp && socket.local.port() == 7200),
            "{listening:?}"
        );
        assert!(
            listening
                .iter()
                .any(|socket| socket.protocol == Protocol::Udp && socket.local.port() == 7201),
            "{listening:?}"
        );
        assert!(
            !listening.iter().any(|socket| socket.local.port() == 7300),
            "the host's listener is in the container's table: {listening:?}"
        );
        let all = observe::sockets(init).unwrap();
        let established = all
            .iter()
            .find(|socket| socket.state == State::Established && socket.local.port() == 7200)
            .unwrap_or_else(|| panic!("no connection: {all:?}"));
        assert_eq!(established.remote, from);
        assert_eq!(established.remote, client.local_addr().unwrap());
        running.stop();
    });
}

//! The host's sockets' one way of serving (`vm.sock`, the CLI's;
//! `docker.sock`, Docker's API, step 11.5b1): made the user's alone, each
//! connection's peer checked (`getpeereid`), counted as a client for the
//! idle rule, bounded, and served on a thread of its own. What differs is
//! how each says no and what each does with a connection ([`Served`]).

use shipyard_runtime::attach::forward;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::VmError;
use crate::host::{ACCEPT_REST, log};
use crate::idle::Clients;
use crate::machine::Machine;

/// The most connections the host serves at once, `vm.sock`'s,
/// `docker.sock`'s and the forwarded ports' together (F152: each costs
/// threads and descriptors from one process's): past it, one is closed
/// unserved. (`host.sock` has a bound of its own, a few: it must answer a
/// stop when everything else is full.)
pub(crate) const MAX_OPEN: usize = 1024;

/// The most of [`MAX_OPEN`] the forwarded ports may take: the rest is kept
/// for `vm.sock` and `docker.sock` (a load test on a published port never
/// shuts out the user's commands).
pub(crate) const PORTS_SHARE: usize = 768;

/// The host's open connections, counted against [`MAX_OPEN`].
#[derive(Debug, Default)]
pub(crate) struct Budget {
    open: AtomicUsize,
}

impl Budget {
    /// One for a connection to one of the host's sockets, if there's room.
    pub(crate) fn take_socket(&self) -> bool {
        self.take_within(MAX_OPEN)
    }

    /// One for a forwarded port's connection, if there's room in its share.
    pub(crate) fn take_port(&self) -> bool {
        self.take_within(PORTS_SHARE)
    }

    /// How many are taken.
    #[cfg(test)]
    pub(crate) fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    /// One given back (each one taken is given back once).
    pub(crate) fn give_back(&self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }

    /// One taken while fewer than `most` are.
    fn take_within(&self, most: usize) -> bool {
        if self.open.fetch_add(1, Ordering::SeqCst) < most {
            return true;
        }
        self.open.fetch_sub(1, Ordering::SeqCst);
        false
    }
}

/// A connection from `listener`, blocking whatever the listener's flags
/// (F147: on macOS an accepted socket inherits its listener's
/// `O_NONBLOCK`, and launchd hands each host the same listener).
pub(crate) fn accepted(listener: &UnixListener) -> std::io::Result<UnixStream> {
    let (connection, _) = listener.accept()?;
    connection.set_nonblocking(false)?;
    Ok(connection)
}

/// What a socket's connections are.
pub(crate) struct Served {
    /// What one is called in the host's log.
    pub(crate) what: &'static str,
    /// Says why it isn't served, as its protocol says it.
    pub(crate) refuse: fn(UnixStream, &forward::Refusal),
    /// Serves it, until it ends.
    pub(crate) serve: fn(&Machine, UnixStream),
}

/// A socket at `path`: the stale one of a host that died removed (the
/// VM's lock is held, so no other host serves it), the new one the user's
/// alone (in a directory that's theirs alone too).
pub(crate) fn bind_private(path: &Path) -> Result<UnixListener, VmError> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).map_err(|e| VmError::io(path, e))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| VmError::io(path, e))?;
    Ok(listener)
}

/// What every connection is served with.
pub(crate) struct Serving {
    pub(crate) machine: Arc<Machine>,
    /// The user the VM is: the only one served.
    pub(crate) uid: u32,
    pub(crate) clients: Arc<Clients>,
    /// Told at each connection's end: the ports' poller looks again (a
    /// container just published is forwarded at once).
    pub(crate) ended: std::sync::mpsc::Sender<()>,
    /// The host's open connections, the forwarded ports' too.
    pub(crate) budget: Arc<Budget>,
}

/// Serves `listener` as `served` says, for as long as the host runs; or,
/// `leave_when_stopping` (launchd's socket), until the host is stopping:
/// then a connection is left in the socket's backlog, where it makes
/// launchd start the next host, never refused.
pub(crate) fn accept(
    listener: &UnixListener,
    served: &Served,
    serving: &Serving,
    leave_when_stopping: bool,
) {
    loop {
        if leave_when_stopping && !waiting_until_stopping(listener, &serving.clients) {
            return;
        }
        let connection = match accepted(listener) {
            Ok(connection) => connection,
            Err(error) => {
                log(&format!("accepting {}: {error}", served.what));
                std::thread::sleep(ACCEPT_REST);
                continue;
            }
        };
        let peer = crate::peer::peer_uid(&connection);
        if !peer.is_some_and(|peer| crate::same_user(peer, serving.uid)) {
            log(&format!(
                "refused {} of uid {peer:?}: only uid {} is served",
                served.what, serving.uid
            ));
            continue;
        }
        if !serving.budget.take_socket() {
            log(&format!("refused {}: {MAX_OPEN} open already", served.what));
            continue;
        }
        // Stopping (idle): refused before it reaches the guest (its
        // client starts the VM again, or is told to).
        let admitted = serving.clients.come();
        let (machine, ended, counted, held) = (
            Arc::clone(&serving.machine),
            serving.ended.clone(),
            Arc::clone(&serving.clients),
            Arc::clone(&serving.budget),
        );
        let (refuse, serve, what) = (served.refuse, served.serve, served.what);
        let spawned = std::thread::Builder::new().spawn(move || {
            if admitted {
                serve(&machine, connection);
                counted.gone();
                let _ = ended.send(());
            } else {
                log(&format!("{what} refused: stopping"));
                refuse(connection, &forward::Refusal::Stopping);
            }
            held.give_back();
        });
        if let Err(error) = spawned {
            // (Its closure, which would have said it gone, never ran.)
            if admitted {
                serving.clients.gone();
            }
            serving.budget.give_back();
            log(&format!("{} not served: {error}", served.what));
        }
    }
}

/// Waits for a connection on `listener` while the host isn't stopping:
/// `true` once one waits, `false` once the host stops (looked at every
/// 200 ms).
fn waiting_until_stopping(listener: &UnixListener, clients: &Clients) -> bool {
    use std::os::fd::AsFd as _;
    loop {
        if clients.stopping() {
            return false;
        }
        let mut waiting = [nix::poll::PollFd::new(
            listener.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        if nix::poll::poll(&mut waiting, nix::poll::PollTimeout::from(200_u16)).is_ok_and(|n| n > 0)
        {
            return true;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// The forwarded ports take at most their share; the host's sockets the
    /// rest too; one given back is free again.
    #[test]
    fn the_budget_keeps_room_for_the_sockets() {
        let budget = Budget::default();
        for _ in 0..PORTS_SHARE {
            assert!(budget.take_port());
        }
        assert!(!budget.take_port(), "past the ports' share");
        for _ in PORTS_SHARE..MAX_OPEN {
            assert!(budget.take_socket(), "the sockets' room");
        }
        assert!(!budget.take_socket(), "past the bound in all");
        budget.give_back();
        assert!(budget.take_socket(), "one given back");
    }

    /// A connection accepted from a non-blocking listener (launchd's, after
    /// a host that couldn't serve) is blocking (F147).
    #[test]
    fn an_accepted_connection_is_blocking() {
        use std::os::fd::AsFd as _;
        let dir = std::env::temp_dir().join(format!("shipyard-accepted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let _client = UnixStream::connect(&path).unwrap();
        let connection = accepted(&listener).unwrap();
        let flags = nix::fcntl::fcntl(connection.as_fd(), nix::fcntl::FcntlArg::F_GETFL).unwrap();
        let flags = nix::fcntl::OFlag::from_bits_truncate(flags);
        assert!(!flags.contains(nix::fcntl::OFlag::O_NONBLOCK));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

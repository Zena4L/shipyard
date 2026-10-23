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

/// The most connections a socket serves at once: past it, one is closed
/// unserved (a client in a loop that never closes can't take the host's
/// threads and descriptors, shared with the forwarded ports).
pub(crate) const MAX_OPEN: usize = 1024;

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
    let open = Arc::new(AtomicUsize::new(0));
    loop {
        if leave_when_stopping && !waiting_until_stopping(listener, &serving.clients) {
            return;
        }
        let connection = listener.accept().map(|(connection, _)| connection);
        let connection = match connection {
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
        if open.fetch_add(1, Ordering::SeqCst) >= MAX_OPEN {
            open.fetch_sub(1, Ordering::SeqCst);
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
            Arc::clone(&open),
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
            held.fetch_sub(1, Ordering::SeqCst);
        });
        if let Err(error) = spawned {
            // (Its closure, which would have said it gone, never ran.)
            if admitted {
                serving.clients.gone();
            }
            open.fetch_sub(1, Ordering::SeqCst);
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

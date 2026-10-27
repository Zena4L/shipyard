//! The host's own control (step 13.6b1, F93, F100): its stop signals taken
//! on a thread of their own from its first line, its phase, and its own
//! socket (`host.sock`), bound from its lock to its end, booting or not,
//! where a CLI asks where it stands or asks it to stop. A stop, however it
//! comes (a signal, the socket, the idle rule's own SIGTERM), is noted here
//! once; the host's main thread takes it between its phases, so nothing
//! cuts a boot's disk check, and nothing is admitted only to be cut. And its
//! own log kept short (`keep_log_small`).

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
use shipyard_runtime::attach::Frame;
use shipyard_runtime::attach::forward::{self, Connection, Request};

use crate::VmError;
use crate::host::log;
use crate::phase::{HostPhase, PhaseAnswer};

/// How long a client of `host.sock` has to say what it asks.
const HELLO_WAIT: Duration = Duration::from_secs(5);

/// A stop asked of the host, once: why, as its log says it.
#[derive(Default)]
pub(crate) struct Stops {
    asked: Mutex<Option<String>>,
    changed: Condvar,
}

impl Stops {
    /// The stop signals (SIGTERM: `vm stop`'s older way, a logout,
    /// launchd's; SIGINT; SIGHUP) blocked in this thread, before any other
    /// exists (each inherits the mask), and taken from here on by a thread
    /// of their own: no signal ends the host before its stop runs.
    pub(crate) fn watched() -> Result<Arc<Self>, VmError> {
        let mut set = SigSet::empty();
        for signal in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP] {
            // Its default first: one a shell had the CLI ignore (`nohup`, a
            // background job's SIGINT) would be dropped as it's sent, and
            // never reach the wait (13.6b1's review).
            // SAFETY: the default action installs no handler; no other
            // thread exists yet to race the change.
            let _ =
                unsafe { nix::sys::signal::signal(signal, nix::sys::signal::SigHandler::SigDfl) };
            set.add(signal);
        }
        pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None)
            .map_err(|e| VmError::Host(format!("blocking signals: {e}")))?;
        let stops = Arc::new(Self::default());
        let theirs = Arc::clone(&stops);
        std::thread::Builder::new()
            .spawn(move || {
                loop {
                    match set.wait() {
                        Ok(signal) => theirs.ask(&format!("{signal:?}")),
                        Err(error) => {
                            log(&format!("waiting for a stop signal: {error}"));
                            return;
                        }
                    }
                }
            })
            .map_err(|e| VmError::Host(format!("no thread for signals: {e}")))?;
        Ok(stops)
    }

    /// A stop asked (`why`); the first one's reason is kept.
    pub(crate) fn ask(&self, why: &str) {
        let mut asked = self.asked.lock().unwrap_or_else(PoisonError::into_inner);
        if asked.is_none() {
            *asked = Some(why.to_owned());
        }
        self.changed.notify_all();
    }

    /// Why a stop was asked, if one has been.
    pub(crate) fn asked(&self) -> Option<String> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits for a stop to be asked: why.
    pub(crate) fn wait(&self) -> String {
        let asked = self.asked.lock().unwrap_or_else(PoisonError::into_inner);
        let asked = self
            .changed
            .wait_while(asked, |asked| asked.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        asked.clone().unwrap_or_default()
    }
}

/// Where the host is, and when that phase's bound ends.
pub(crate) struct Phase {
    now: Mutex<(HostPhase, Instant)>,
}

impl Phase {
    /// Preparing, within `bound`.
    pub(crate) fn preparing(bound: Duration) -> Arc<Self> {
        Arc::new(Self {
            now: Mutex::new((HostPhase::Preparing, Instant::now() + bound)),
        })
    }

    /// Now `phase`, within `bound` from now.
    pub(crate) fn set(&self, phase: HostPhase, bound: Duration) {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner) = (phase, Instant::now() + bound);
    }

    /// Now `to` (within `bound`), only if it's still `from`: a later phase
    /// set meanwhile (a stop) is never undone.
    pub(crate) fn advance(&self, from: HostPhase, to: HostPhase, bound: Duration) -> bool {
        let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
        let moved = now.0 == from;
        if moved {
            *now = (to, Instant::now() + bound);
        }
        moved
    }

    /// Where it is now.
    pub(crate) fn get(&self) -> HostPhase {
        self.now.lock().unwrap_or_else(PoisonError::into_inner).0
    }

    /// Its answer now: the phase, and what's left of its bound.
    pub(crate) fn answer(&self) -> PhaseAnswer {
        let (phase, ends) = *self.now.lock().unwrap_or_else(PoisonError::into_inner);
        PhaseAnswer {
            phase,
            left: match phase {
                HostPhase::Up => Duration::ZERO,
                _ => ends.saturating_duration_since(Instant::now()),
            },
        }
    }
}

/// Serves `host.sock` (`listener`) for as long as the host runs: each
/// connection from `uid` only, on a thread of its own, its ask answered
/// ([`forward::HOST_PHASE_REQUEST`], [`forward::HOST_STOP_REQUEST`]).
pub(crate) fn serve(listener: UnixListener, uid: u32, phase: Arc<Phase>, stops: Arc<Stops>) {
    let accepting = move || {
        let open = Arc::new(AtomicUsize::new(0));
        for connection in listener.incoming() {
            let Ok(connection) = connection else {
                std::thread::sleep(crate::host::ACCEPT_REST);
                continue;
            };
            if !crate::peer::peer_uid(&connection).is_some_and(|peer| crate::same_user(peer, uid)) {
                log("a connection to host.sock from another user: refused");
                continue;
            }
            // A few at once, each a moment (its hello's wait at most): one
            // more is closed unanswered.
            if open.fetch_add(1, Ordering::SeqCst) >= MAX_ASKING {
                open.fetch_sub(1, Ordering::SeqCst);
                continue;
            }
            let (phase, stops, theirs) =
                (Arc::clone(&phase), Arc::clone(&stops), Arc::clone(&open));
            let spawned = std::thread::Builder::new().spawn(move || {
                answer(connection, &phase, &stops);
                theirs.fetch_sub(1, Ordering::SeqCst);
            });
            if spawned.is_err() {
                open.fetch_sub(1, Ordering::SeqCst);
            }
        }
    };
    if let Err(error) = std::thread::Builder::new().spawn(accepting) {
        log(&format!("host.sock isn't served (no thread: {error})"));
    }
}

/// The most connections to `host.sock` answered at once.
const MAX_ASKING: usize = 16;

/// One connection to `host.sock`: its hello read (within [`HELLO_WAIT`]),
/// its ask answered.
fn answer(connection: UnixStream, phase: &Phase, stops: &Stops) {
    // (An accepted socket inherits its listener's flags on macOS.)
    let _ = connection.set_nonblocking(false);
    let _ = connection.set_read_timeout(Some(HELLO_WAIT));
    let Ok(writer) = connection.try_clone() else {
        return;
    };
    let mut frames = Connection::new(connection);
    let Ok(Some(Frame::HelloRun { version, request })) = frames.next_frame() else {
        return;
    };
    let Some(agreed) = shipyard_runtime::attach::negotiate(version) else {
        return;
    };
    let mut writer = writer;
    let asked = Request::decode(&request).ok();
    let args = asked.as_ref().map(|request| request.args.as_slice());
    let frame = |frame: &Frame, writer: &mut UnixStream| forward::send(writer, frame).is_ok();
    match args {
        Some([ask]) if ask == forward::HOST_PHASE_REQUEST => {}
        Some([ask]) if ask == forward::HOST_STOP_REQUEST => {
            log("asked to stop over its own socket");
            stops.ask("asked over its own socket");
        }
        _ => {
            let refusal = forward::Refusal::Other("not a request this host answers".to_owned());
            let _ = frame(&refusal.frame(agreed), &mut writer);
            return;
        }
    }
    let said = frame(
        &Frame::Welcome {
            version: agreed,
            terminal: false,
            stdin: false,
        },
        &mut writer,
    ) && frame(
        &Frame::Stdout(phase.answer().encode().into_bytes()),
        &mut writer,
    );
    if said {
        let _ = frame(&Frame::Exited(0), &mut writer);
    }
}

/// The host's log's size past which it's moved aside.
const LOG_KEPT: u64 = 1 << 20;

/// How often a running host looks at its log's size.
const LOG_EVERY: Duration = Duration::from_secs(3600);

/// The host's log (`log`, its descriptors 1 and 2) kept short by the host
/// alone (F106: a CLI that moved it aside sent a running host's lines to
/// the old file, and read why a start failed from an empty one): moved
/// aside now if it's past [`LOG_KEPT`], and every [`LOG_EVERY`] after.
pub(crate) fn keep_log_small(log_path: &std::path::Path) {
    let path = log_path.to_owned();
    rotate_log(&path);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(LOG_EVERY);
            rotate_log(&path);
        }
    });
}

/// [`keep_log_small`]'s one look: the log moved aside ([`moved_aside`]),
/// the new one put on descriptors 1 and 2.
fn rotate_log(path: &std::path::Path) {
    match moved_aside(path, LOG_KEPT) {
        Ok(Some(new)) => {
            // One that took descriptor 1 or 2 (the host started without
            // them) is that descriptor already: kept open, never dropped.
            if std::os::fd::AsRawFd::as_raw_fd(&new) <= 2 {
                std::mem::forget(new);
                return;
            }
            if nix::unistd::dup2_stdout(&new).is_err() || nix::unistd::dup2_stderr(&new).is_err() {
                log("the new log isn't this host's output");
            }
        }
        Ok(None) => {}
        Err(error) => log(&format!("the log isn't moved aside: {error}")),
    }
}

/// The log at `path`, past `kept` bytes, renamed to `….log.1` and a new
/// one made in its place (the user's alone), to be written to: `None` if
/// it's within its bound.
fn moved_aside(path: &std::path::Path, kept: u64) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt as _;
    if !std::fs::metadata(path).is_ok_and(|meta| meta.len() > kept) {
        return Ok(None);
    }
    std::fs::rename(path, path.with_extension("log.1"))?;
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .map(Some)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a panic in a test is a clear failure")]
mod tests {
    use super::*;

    /// A stop asked keeps its first reason, and a waiter is woken by it.
    #[test]
    fn a_stop_is_asked_once_and_wakes_its_waiter() {
        let stops = Arc::new(Stops::default());
        assert_eq!(stops.asked(), None);
        let theirs = Arc::clone(&stops);
        let waiter = std::thread::spawn(move || theirs.wait());
        stops.ask("SIGTERM");
        stops.ask("asked over its own socket");
        assert_eq!(waiter.join().unwrap(), "SIGTERM");
        assert_eq!(stops.asked().as_deref(), Some("SIGTERM"));
    }

    /// A phase advances only from the one it's in (a stop set meanwhile
    /// stays); up has nothing left; another phase, what's left of its bound.
    #[test]
    fn a_phase_moves_on_only_from_where_it_is() {
        let phase = Phase::preparing(Duration::from_secs(60));
        assert_eq!(phase.answer().phase, HostPhase::Preparing);
        assert!(phase.answer().left > Duration::from_secs(50));
        phase.set(HostPhase::Checking, Duration::from_secs(600));
        phase.set(HostPhase::Stopping, Duration::from_secs(90));
        assert!(!phase.advance(HostPhase::Checking, HostPhase::Up, Duration::ZERO));
        assert_eq!(phase.get(), HostPhase::Stopping);
        phase.set(HostPhase::Checking, Duration::from_secs(600));
        assert!(phase.advance(HostPhase::Checking, HostPhase::Up, Duration::ZERO));
        assert_eq!(phase.answer().left, Duration::ZERO);
    }

    /// A log past its bound is moved aside whole and a new one (0600) made
    /// to write to; one within it is left be.
    #[test]
    fn a_big_log_is_moved_aside() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("shipyard-hostlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.log");
        std::fs::write(&path, b"small\n").unwrap();
        assert!(moved_aside(&path, 100).unwrap().is_none());
        std::fs::write(&path, vec![b'x'; 200]).unwrap();
        let mut new = moved_aside(&path, 100).unwrap().unwrap();
        new.write_all(b"after\n").unwrap();
        assert_eq!(std::fs::read(dir.join("host.log.1")).unwrap().len(), 200);
        assert_eq!(std::fs::read(&path).unwrap(), b"after\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The socket answers its two asks (a stop noted), refuses anything else.
    #[test]
    fn the_host_socket_answers_its_asks() {
        let dir = std::env::temp_dir().join(format!("shipyard-hostsock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (phase, stops) = (
            Phase::preparing(Duration::from_secs(60)),
            Arc::new(Stops::default()),
        );
        serve(
            listener,
            nix::unistd::getuid().as_raw(),
            Arc::clone(&phase),
            Arc::clone(&stops),
        );
        let ask = |what: &str| {
            let mut stream = UnixStream::connect(&path).unwrap();
            let hello = forward::hello(&Request::internal(&[what])).unwrap();
            forward::send(&mut stream, &hello).unwrap();
            let mut connection = Connection::new(stream);
            std::iter::from_fn(|| connection.next_frame().ok().flatten()).collect::<Vec<_>>()
        };
        let frames = ask(forward::HOST_PHASE_REQUEST);
        let Some(Frame::Stdout(said)) = frames.get(1) else {
            panic!("{frames:?}");
        };
        let said = PhaseAnswer::decode(std::str::from_utf8(said).unwrap()).unwrap();
        assert_eq!(said.phase, HostPhase::Preparing);
        assert_eq!(frames.last(), Some(&Frame::Exited(0)));
        assert_eq!(stops.asked(), None, "a phase's ask stops nothing");
        let frames = ask(forward::HOST_STOP_REQUEST);
        assert_eq!(frames.last(), Some(&Frame::Exited(0)));
        assert!(stops.asked().is_some(), "the stop noted");
        assert!(matches!(ask("ps").first(), Some(Frame::Refused(_))));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

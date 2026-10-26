//! An exec's start: its session with the client (a terminal's stream or
//! Docker's frames over pipes), pumped on threads of its own, and its end
//! (its code kept, the client let go, a hang-up passed on).

use std::io::Write as _;
use std::os::fd::{AsFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Instant;

use bytes::Bytes;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper::upgrade::OnUpgrade;
use shipyard_runtime::attach::DetachKeys;
use shipyard_runtime::logs::cri::Stream;
use shipyard_runtime::process::EXIT_ENGINE_ERROR;
use shipyard_runtime::terminal::{DEFAULT_SIZE, Size};
use tokio::sync::OwnedSemaphorePermit;

use crate::Shared;
use crate::errors::ApiError;
use crate::idle::Busy;
use crate::reply;
use crate::streams;

use super::spawn::{Io, Mode, read_pid, spawn};
use super::{DETACHED_HOLD, DRAIN_WAIT, HANG_UP_GRACE, Instance, LOOK, Reply, StartBody, running};

/// `POST /exec/{id}/start` (`Detach`, `ConsoleSize`; an empty body asks
/// neither): detached, 200 at once; else the connection taken over (101)
/// and the exec's streams on it. Started once only (409).
pub(crate) async fn start(
    shared: &Arc<Shared>,
    exec_id: String,
    body: Incoming,
    upgrade: Option<OnUpgrade>,
) -> Reply {
    let body: StartBody = crate::read_json_or_default(body).await?;
    let instance = shared.execs.get(&exec_id)?;
    // Its container still up, as Docker's daemon asks (409): a container
    // gone since the create is said here, not as the exec's exit code.
    running(shared, instance.container.clone()).await?;
    let attached = if body.detach.unwrap_or(false) {
        None
    } else {
        let upgrade = upgrade.ok_or_else(|| {
            ApiError::bad("an exec's start takes the connection over: ask with `Upgrade: tcp`")
        })?;
        Some((upgrade, streams::stream_permit(shared)?))
    };
    if !instance.claim() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!("Exec command {exec_id} has already run"),
        ));
    }
    let mode = match (&attached, instance.config.tty) {
        (None, _) => Mode::Detached,
        (Some(_), true) => Mode::Terminal(
            body.console_size
                .map_or(DEFAULT_SIZE, |[rows, cols]| Size::new(rows, cols)),
        ),
        (Some(_), false) => Mode::Pipes,
    };
    let busy = shared.idle.connection();
    let (child, io) = spawn(&instance, mode).map_err(|error| {
        // Never run: startable again (F148), never "started" for good.
        instance.unclaim();
        ApiError::internal(format!("the exec couldn't start: {error}"))
    })?;
    let pid = child.id();
    let watched = Arc::clone(&instance);
    let Some((upgrade, permit)) = attached else {
        if thread(move || finish(&instance, child, None, busy)).is_none() {
            unwatched(&watched, pid);
            return Err(ApiError::internal(
                "the system gave no thread to watch the exec: it was ended",
            ));
        }
        return Ok(reply::empty(StatusCode::OK));
    };
    let terminal = instance.config.tty;
    tokio::spawn(async move {
        let client = crate::hijack::take_over(upgrade).await;
        if thread(move || session(&instance, child, io, client, busy, permit)).is_none() {
            unwatched(&watched, pid);
        }
    });
    Ok(crate::hijack::switching(streams::content_type(terminal)))
}

/// A thread of the exec's own, or `None` if the system refuses one (a task
/// limit): never std's panic, which would end the whole service, every
/// session with it (11.6b's API review).
fn thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<JoinHandle<T>> {
    std::thread::Builder::new().spawn(work).ok()
}

/// An exec no thread could watch: ended (its process group killed: it's
/// its own), said ended with Shipyard's failure, never "running" for good.
fn unwatched(instance: &Instance, pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let mut state = instance.state();
    state.running = false;
    state.exit_code = Some(i32::from(EXIT_ENGINE_ERROR));
    state.master = None;
    state.since = Instant::now();
}

/// A session: the exec's output to the client, the client's input to the
/// exec, until it ends or the client types its detach keys (a terminal's:
/// the session ends, the exec goes on); a client gone before it began, or
/// that hangs up, hangs the exec up. `busy` holds the service up, `permit`
/// is its stream's place.
fn session(
    instance: &Instance,
    child: Child,
    io: Io,
    client: Option<(tokio::net::UnixStream, Bytes)>,
    busy: Busy,
    permit: OwnedSemaphorePermit,
) {
    let taken = client.and_then(|(client, typed)| {
        let client = client.into_std().ok()?;
        client.set_nonblocking(false).ok()?;
        Some((client, typed))
    });
    let Some((client, typed)) = taken else {
        hang_up(&child);
        return finish(instance, child, Some(Watched::gone()), busy);
    };
    let ended = Arc::new(AtomicBool::new(false));
    let keys_typed = Arc::new(AtomicBool::new(false));
    let outputs = match io {
        Io::Terminal(master) => terminal(
            &client,
            typed,
            &master,
            &instance.config.detach_keys,
            &keys_typed,
            &ended,
        ),
        Io::Pipes {
            stdin,
            stdout,
            stderr,
        } => pipes(
            &client,
            typed,
            stdin,
            [OwnedFd::from(stdout), OwnedFd::from(stderr)],
            instance.config.streams,
            &ended,
        ),
        Io::Detached => Vec::new(),
    };
    let watched = Watched {
        client: Some(client),
        outputs,
        ended,
        hung_up: false,
        keys_typed,
        permit: Some(permit),
    };
    finish(instance, child, Some(watched), busy);
}

/// An attached exec's client and output pumps, for [`finish`].
struct Watched {
    /// Its client: `None` if it was gone before the session began.
    client: Option<UnixStream>,
    outputs: Vec<JoinHandle<()>>,
    /// Set once the exec has ended: its outputs drain for [`DRAIN_WAIT`].
    ended: Arc<AtomicBool>,
    /// Hung up already (its client was gone before the session began).
    hung_up: bool,
    /// Set once its client typed its detach keys: it runs on, detached.
    keys_typed: Arc<AtomicBool>,
    /// Its stream's place, let go of once it's detached.
    permit: Option<OwnedSemaphorePermit>,
}

impl Watched {
    /// A session whose client was gone before it began: hung up at once.
    fn gone() -> Self {
        Self {
            client: None,
            outputs: Vec::new(),
            ended: Arc::new(AtomicBool::new(false)),
            hung_up: true,
            keys_typed: Arc::new(AtomicBool::new(false)),
            permit: None,
        }
    }
}

/// A terminal's pumps: the master's bytes to the client, as they are; the
/// client's (its first ones first) to the master, watched for its detach
/// `keys`: typed, `keys_typed` is set, the client's stream ends both ways
/// and the exec runs on (its output still read, and dropped: a full
/// terminal would stop it). The output pump's handle.
fn terminal(
    client: &UnixStream,
    typed: Bytes,
    master: &Arc<OwnedFd>,
    keys: &DetachKeys,
    keys_typed: &Arc<AtomicBool>,
    ended: &Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    use std::io::Read as _;
    let (Ok(from_master), Ok(mut to_master), Ok(mut from_client), Ok(mut to_client)) = (
        master.try_clone(),
        master.try_clone().map(std::fs::File::from),
        client.try_clone(),
        client.try_clone(),
    ) else {
        return Vec::new();
    };
    let Ok(closing) = client.try_clone() else {
        return Vec::new();
    };
    let mut matcher = keys.matcher();
    let detaching = Arc::clone(keys_typed);
    // (Refused a thread, it takes no input: its output's still passed on.)
    let _ = thread(move || {
        let mut passed = Vec::new();
        let mut buffer = vec![0_u8; 8 * 1024];
        let mut bytes = typed.to_vec();
        loop {
            passed.clear();
            let matched = matcher.feed(&bytes, &mut passed);
            if to_master.write_all(&passed).is_err() {
                return;
            }
            if matched {
                detaching.store(true, Ordering::Release);
                let _ = closing.shutdown(std::net::Shutdown::Both);
                return;
            }
            match from_client.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(read) => bytes = buffer.get(..read).unwrap_or_default().to_vec(),
            }
        }
    });
    let (ended, detached) = (Arc::clone(ended), Arc::clone(keys_typed));
    // The master's end (EIO once the child's side is closed) ends it.
    thread(move || {
        pump(&from_master, &ended, |bytes| {
            if detached.load(Ordering::Acquire) {
                return Ok(());
            }
            to_client.write_all(bytes).or_else(|error| {
                if detached.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err(error)
                }
            })
        });
    })
    .into_iter()
    .collect()
}

/// The client's bytes to the command's input as they come: a read, then
/// its write, never `std::io::copy`, whose `splice` between a socket and a
/// pipe held a line typed into `docker exec -i` until the client's input
/// ended (found by 12.5a's probe).
fn pass_on(from: &mut impl std::io::Read, to: &mut impl std::io::Write) {
    let mut buffer = vec![0_u8; 8 * 1024];
    loop {
        match from.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(0) | Err(_) => return,
            Ok(read) => {
                let Some(bytes) = buffer.get(..read) else {
                    return;
                };
                if to.write_all(bytes).is_err() {
                    return;
                }
            }
        }
    }
}

/// Pipes' pumps: stdout and stderr framed to the client (each only if
/// attached, else read and dropped), the client's bytes to stdin (its
/// first ones first), stdin closed at the client's half-close. The output
/// pumps' handles.
fn pipes(
    client: &UnixStream,
    typed: Bytes,
    stdin: Option<std::process::ChildStdin>,
    [stdout, stderr]: [OwnedFd; 2],
    streams: [bool; 2],
    ended: &Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    let Ok(writer) = client.try_clone() else {
        return Vec::new();
    };
    let writer = Arc::new(Mutex::new(writer));
    if let (Some(mut stdin), Ok(mut from_client)) = (stdin, client.try_clone()) {
        let _ = thread(move || {
            if stdin.write_all(&typed).is_ok() {
                pass_on(&mut from_client, &mut stdin);
            }
            // Dropped: its end of file.
        });
    }
    let out = |from: OwnedFd, stream: Stream| {
        let (writer, ended) = (Arc::clone(&writer), Arc::clone(ended));
        let wanted = streams[stream.index()];
        thread(move || {
            pump(&from, &ended, |bytes| {
                if !wanted {
                    return Ok(());
                }
                let framed = streams::frame(stream, bytes);
                let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
                writer.write_all(&framed)
            });
        })
    };
    [out(stdout, Stream::Stdout), out(stderr, Stream::Stderr)]
        .into_iter()
        .flatten()
        .collect()
}

/// Reads `from` and hands each piece to `write`, until its end, a write
/// that fails, or, once `ended` is set (the exec's end), a quiet look or
/// [`DRAIN_WAIT`]: what a background process the exec left holds open is
/// never waited for.
pub(super) fn pump(
    from: &OwnedFd,
    ended: &AtomicBool,
    mut write: impl FnMut(&[u8]) -> std::io::Result<()>,
) {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let mut buffer = vec![0_u8; 32 * 1024];
    let mut draining_since = None;
    let look = PollTimeout::try_from(LOOK).unwrap_or(PollTimeout::MAX);
    loop {
        let ready = {
            let mut fds = [PollFd::new(from.as_fd(), PollFlags::POLLIN)];
            poll(&mut fds, look).is_ok_and(|ready| ready > 0)
        };
        if ended.load(Ordering::Acquire) {
            let since = *draining_since.get_or_insert_with(Instant::now);
            if !ready || since.elapsed() >= DRAIN_WAIT {
                return;
            }
        }
        if !ready {
            continue;
        }
        let read = match nix::unistd::read(from, &mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        if write(buffer.get(..read).unwrap_or_default()).is_err() {
            return;
        }
    }
}

/// Waits for the exec's end, its command's PID read as soon as it's
/// written, watching its client (a hang-up hangs it up: SIGHUP, then one
/// SIGKILL after [`HANG_UP_GRACE`]); then keeps its code, drains its
/// outputs, closes the client's stream for writing, and lets the service go
/// (`busy`). Detached at its start, it lets the service go before its end,
/// after [`DETACHED_HOLD`]; detached from by its keys, its stream's place
/// goes at once, the service when it ends.
fn finish(instance: &Instance, mut child: Child, mut watched: Option<Watched>, busy: Busy) {
    let mut hung_up_at = watched
        .as_ref()
        .is_some_and(|watched| watched.hung_up)
        .then(Instant::now);
    let mut killed = false;
    let began = Instant::now();
    let mut busy = Some(busy);
    let status = loop {
        read_pid(instance);
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if released(watched.as_ref(), began) {
            busy = None;
        }
        let keys_typed = watched
            .as_ref()
            .is_some_and(|watched| watched.keys_typed.load(Ordering::Acquire));
        if keys_typed && let Some(watched) = &mut watched {
            watched.permit = None;
        }
        // Its keys read after the hang-up's look: they're set before the
        // stream is shut, so a shut seen here is never taken for a hang-up.
        if hung_up_at.is_none()
            && let Some(watched) = watched.as_ref()
            && let Some(client) = watched.client.as_ref()
            && crate::hijack::hung_up(client.as_fd())
            && !watched.keys_typed.load(Ordering::Acquire)
        {
            hang_up(&child);
            hung_up_at = Some(Instant::now());
        }
        if !killed && hung_up_at.is_some_and(|at| at.elapsed() >= HANG_UP_GRACE) {
            let _ = child.kill();
            killed = true;
        }
        std::thread::sleep(LOOK);
    };
    let code = exit_code(status);
    // A command that ended at once may have said its PID only now; a file
    // never read whole (garbled) goes all the same.
    read_pid(instance);
    if let Some(file) = &instance.pid_file {
        let _ = std::fs::remove_file(file);
    }
    // Kept before the client sees its stream end: Docker's CLI asks for the
    // code at once.
    {
        let mut state = instance.state();
        state.running = false;
        state.exit_code = Some(code);
        state.master = None;
        state.since = Instant::now();
    }
    if let Some(watched) = watched {
        watched.ended.store(true, Ordering::Release);
        for output in watched.outputs {
            let _ = output.join();
        }
        // Both ways: the client reads the stream's end, and the thread
        // passing its input on (a copy of it, and of the terminal's master)
        // reads its end and goes, never waiting for the client to close
        // (F148).
        if let Some(client) = watched.client {
            let _ = client.shutdown(std::net::Shutdown::Both);
        }
    }
    drop(busy);
}

/// Whether the exec lets the service go before its end: detached at its
/// start (none watched), once [`DETACHED_HOLD`] has passed. One detached
/// from by its client's keys never does: its child's terminal is the
/// service's, and the service's exit would hang it up, and so the command
/// (11.6a3's Murphy review); its stream's place goes at the detach.
fn released(watched: Option<&Watched>, began: Instant) -> bool {
    watched.is_none() && began.elapsed() >= DETACHED_HOLD
}

/// The code an exec's `status` says, Docker's way: its own, or 128 + its
/// signal; Shipyard's failure if it can't be told.
fn exit_code(status: Option<std::process::ExitStatus>) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    status
        .and_then(|status| {
            status
                .code()
                .or_else(|| status.signal().map(|signal| 128 + signal))
        })
        .unwrap_or(i32::from(EXIT_ENGINE_ERROR))
}

/// SIGHUP to the exec's process group (its own: a session or a group of
/// its own), as a terminal's hang-up would.
fn hang_up(child: &Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGHUP,
        );
    }
}

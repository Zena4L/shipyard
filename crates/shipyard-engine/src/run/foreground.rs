//! A foreground run: the container attached to, its signals passed on, its
//! end awaited ([`attached`]).

use std::os::fd::{AsFd as _, OwnedFd};

use nix::sys::signal::SigSet;
use shipyard_runtime::container as runtime;

use super::start::{Input, detached};
use crate::Engine;
use crate::containers::{self, ContainerId, Record};
use crate::error::{Error, ErrorKind, Result, bail};
use crate::report::WarningKind;
use crate::run::RunRequest;

/// Let in by container `id`'s monitor (its runtime state under `root`)
/// before it starts, as `options` say: what [`start_attached`](super::start_attached) and
/// [`attached`] do between its creation and its start gate's opening, so
/// nothing it writes is missed. Why not, if it isn't.
pub(super) fn attach_before_start(
    root: &std::path::Path,
    id: &ContainerId,
    options: &crate::attach::AttachOptions,
) -> std::result::Result<crate::attach::Welcomed, String> {
    shipyard_runtime::container::attach_stream(root, id.as_str())
        .map_err(|error| error.to_string())
        .and_then(|stream| {
            crate::attach::welcomed(stream, options).map_err(|error| error.to_string())
        })
}

/// How a foreground run attaches (its held client's options), and how
/// its container takes its input: by its `-i` and `-t`.
fn foreground_streams(
    request: &RunRequest,
    detach_keys: Option<shipyard_runtime::attach::DetachKeys>,
    output: crate::attach::Output,
) -> (crate::attach::AttachOptions, Input) {
    use shipyard_runtime::attach::DetachKeys;
    let (stdin, terminal) = (request.streams.interactive, request.streams.tty);
    let attaching = crate::attach::AttachOptions {
        // Typed into a terminal (`-it`): Docker's keys, or those asked
        // for. Otherwise none: any bytes may be input.
        detach_keys: if stdin && terminal {
            detach_keys.unwrap_or_else(DetachKeys::default_keys)
        } else {
            DetachKeys::none()
        },
        stdin,
        // The container waits for it: a slow reader loses no output, a
        // fast writer no input.
        held: true,
        output,
    };
    // Its input ends at the caller's end of file, unless it's a
    // terminal's (closing a terminal is a hang-up; its Ctrl-D is a key).
    let input = if stdin && !terminal {
        Input::OnceItEnds
    } else {
        Input::AsMade
    };
    (attaching, input)
}

pub(super) fn attached(
    engine: &Engine,
    request: RunRequest,
    auto_remove: bool,
    options: super::Foreground,
) -> Result<super::Finished> {
    use shipyard_runtime::process::SignalWatch;
    let root = engine.paths.runtime_root()?.to_owned();
    let super::Foreground {
        sig_proxy,
        detach_keys,
        output,
    } = options;
    let terminal = request.streams.tty;
    // `-it` needs a terminal to type on (Docker's rule): refused before
    // anything is made.
    if request.streams.interactive && terminal && !crate::streams::Terminals::detect().stdin {
        return Err(crate::StreamsError::InputNotATerminal.into());
    }
    let (attaching, input) = foreground_streams(&request, detach_keys, output);
    // A terminal's window resizes, from the watch (it has SIGWINCH): to
    // the session as a pipe, never to the container as a signal. Made
    // before the container: a failure leaves nothing behind.
    let resizes = resize_pipe(terminal && sig_proxy)?;
    // Put back at the end: the watch's own restore is the mask under
    // the Ctrl-C hold it's made in.
    let mask = SigSet::thread_get_mask().ok();
    let mut early = None;
    let mut watch = None;
    let mut waiter = None;
    let made = detached(engine, request, auto_remove, input, |id| {
        // Told its exit code however its attachment ends (`--rm` may
        // remove it before this would read its record).
        waiter = runtime::waiter(&root, id.as_str()).unwrap_or_else(|error| {
            engine.warn(
                WarningKind::Runtime,
                format!(
                    "couldn't wait for container {} as it ends: {error}",
                    id.short()
                ),
            );
            None
        });
        // Once it exists: a Ctrl-C before ends the run cleanly (it's
        // undone); from here on it's the container's.
        if sig_proxy {
            watch = SignalWatch::install()
                .map_err(|error| {
                    engine.warn(
                        WarningKind::Runtime,
                        format!("signals won't be passed on to the container: {error}"),
                    );
                })
                .ok();
        }
        early = Some(attach_before_start(&root, id, &attaching));
    });
    let (made, record) = match made {
        Ok(made) => made,
        Err(error) => {
            drop(watch);
            restore_mask(mask.as_ref());
            return Err(error);
        }
    };
    // Its hold on Ctrl-C let go (the watch, if any, has them now).
    drop(made);
    let interrupted = std::sync::atomic::AtomicI32::new(0);
    let forwarding = Forwarding {
        root: &root,
        record: &record,
        resizes: resizes.as_ref().map(|(_, write)| write),
        interrupted: &interrupted,
    };
    let ending = std::thread::scope(|scope| {
        let forwarder = watch
            .as_ref()
            .map(|watch| Forwarder::start(scope, watch, forwarding))
            .transpose();
        // The forwarder stops (dropped) once its end is known.
        forwarder.and_then(|_forwarder| {
            // (Only with the watch: without, the session has its own.)
            let resized = resizes
                .as_ref()
                .filter(|_| watch.is_some())
                .map(|(read, _)| read.as_fd());
            attached_until_its_end(engine, &root, &record, early, waiter, resized)
        })
    });
    // Signals as they were before the run, before its end is waited
    // for: a Ctrl-C now ends this process (the container has ended).
    drop(watch);
    if matches!(ending, Ok(Ending::HungUp)) {
        ignore_hangups();
    }
    restore_mask(mask.as_ref());
    let code = match ending? {
        // It runs on: nothing to wait for.
        Ending::Detached => return Ok(left(record, 0)),
        // As the in-process run's (128 + SIGHUP).
        Ending::HungUp => return Ok(left(record, 129)),
        Ending::Exited(code) => {
            // Its end recorded (and, with `--rm`, it removed) before
            // this returns: `ps -a` right after says how it ended.
            containers::wait_settled(engine, &record.id, containers::SETTLE_WAIT);
            code
        }
        Ending::Settled(code) => code,
    };
    if auto_remove {
        warn_if_left(engine, &record);
    }
    let code = u8::try_from(code).unwrap_or(shipyard_runtime::process::EXIT_ENGINE_ERROR);
    let interrupted = shipyard_runtime::process::Signal::from_raw(interrupted.into_inner())
        .ok()
        .filter(|signal| i32::from(code) == 128 + signal.as_raw());
    Ok(super::Finished {
        code,
        id: record.id,
        name: record.name,
        interrupted,
    })
}

/// A foreground run's container has ended: said if it ran without a
/// network for a while (its pasta died). Read as soon as the run's
/// session ends, before its end is waited for: with `--rm`, its logs go
/// with it.
fn say_if_its_network_was_lost(engine: &Engine, record: &Record) {
    if let Some(lost) = containers::last_start_lost(engine, record) {
        engine.warn(
            WarningKind::NetworkLost,
            format!(
                "container {} lost its network at {}: {}",
                record.name, lost.time, lost.how
            ),
        );
    }
}

/// A foreground run left with `code`, its container running on.
fn left(record: Record, code: u8) -> super::Finished {
    super::Finished {
        code,
        id: record.id,
        name: record.name,
        interrupted: None,
    }
}

/// How a foreground run's attachment ended.
enum Ending {
    /// Its detach keys were typed: the container runs on.
    Detached,
    /// The user's terminal hung up: the container runs on (it was
    /// passed SIGHUP with `--sig-proxy`).
    HungUp,
    /// The container ended, with this code: its clean-up still to wait
    /// for.
    Exited(i32),
    /// With this code, its end settled already.
    Settled(i32),
}

/// A foreground run's container `record`, attached to (`early`: let in
/// before it started, or why not) until it ends, `waiter` told its code.
fn attached_until_its_end(
    engine: &Engine,
    root: &std::path::Path,
    record: &Record,
    early: Option<std::result::Result<crate::attach::Welcomed, String>>,
    waiter: Option<runtime::Waiter>,
    resizes: Option<std::os::fd::BorrowedFd<'_>>,
) -> Result<Ending> {
    use crate::attach::{AttachError, Ended, Settled};
    let name = &record.name;
    let ended = match early {
        Some(Ok(welcomed)) => match resizes {
            Some(resizes) => welcomed.session_resized(resizes),
            None => welcomed.session(),
        },
        Some(Err(why)) => {
            engine.warn(
                WarningKind::AttachedLate,
                format!(
                    "container {name} couldn't be attached to before it started ({why}): \
                     its output is in its logs"
                ),
            );
            say_if_its_network_was_lost(engine, record);
            return end_code(engine, root, record, waiter).map(Ending::Settled);
        }
        // Its start never let it in: up already, which a container just
        // made can't be.
        None => bail!(
            Runtime,
            "container {name} was up before this run started it: it runs on (`shipyard \
             attach {name}`)"
        ),
    };
    // Ended (not left running): said at once if it ran without a
    // network, before its end is waited for (with `--rm`, its logs go).
    if !matches!(ended, Ok(Ended::Detached | Ended::HungUp | Ended::Left(_))) {
        say_if_its_network_was_lost(engine, record);
    }
    match ended {
        Ok(Ended::Exited(code)) => Ok(Ending::Exited(code)),
        // (A foreground run's session isn't a signalled one: never left.)
        Ok(Ended::Detached | Ended::Left(_)) => Ok(Ending::Detached),
        Ok(Ended::HungUp) => Ok(Ending::HungUp),
        // Held, it's never let go of for falling behind; else, the rest
        // of its output is in the logs.
        Ok(Ended::Dropped(reason)) => {
            engine.warn(
                WarningKind::FellBehind,
                format!(
                    "container {name}'s monitor let go of this run ({reason}): the rest of \
                     its output is in its logs"
                ),
            );
            end_code(engine, root, record, waiter).map(Ending::Settled)
        }
        // Its output can't be written any more (`run … | head`): as in a
        // pipeline, the container's writer gets SIGPIPE (it writes to
        // its monitor, which never closes on it).
        Err(AttachError::Io(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            if let Ok(pipe) = shipyard_runtime::process::Signal::from_raw(libc::SIGPIPE) {
                pass_on(root, record, pipe);
            }
            end_code(engine, root, record, waiter).map(Ending::Settled)
        }
        ended => {
            let settled =
                crate::attach::settle(Ok(ended), || crate::attach::recorded_end(engine, record))
                    .map_err(|error| {
                        if containers::has_monitor(engine, &record.id) {
                            error.context(format!(
                                "container {name} runs on (`shipyard stop {name}` ends it)"
                            ))
                        } else {
                            error
                        }
                    })?;
            Ok(match settled {
                Settled::Exited(code) => Ending::Settled(code),
                Settled::Left => Ending::Settled(end_code(engine, root, record, waiter)?),
            })
        }
    }
}

/// A pipe for a terminal's window resizes (`wanted`): non-blocking, a
/// byte written for each SIGWINCH, drained for one resize.
fn resize_pipe(wanted: bool) -> Result<Option<(OwnedFd, OwnedFd)>> {
    use nix::fcntl::OFlag;
    Ok(if wanted {
        Some(nix::unistd::pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)?)
    } else {
        None
    })
}

/// What a [`Forwarder`] passes signals on to, and notes.
#[derive(Clone, Copy)]
struct Forwarding<'scope> {
    /// The runtime's state root.
    root: &'scope std::path::Path,
    /// The container.
    record: &'scope Record,
    /// Where a terminal's window resizes go (a byte each), instead of
    /// to the container as SIGWINCH; and this process's resumes
    /// (SIGCONT).
    resizes: Option<&'scope std::os::fd::OwnedFd>,
    /// The last SIGINT or SIGQUIT passed on.
    interrupted: &'scope std::sync::atomic::AtomicI32,
}

/// Passes the signals `watch` catches on to a foreground run's
/// container (`--sig-proxy`), from a thread of its own, until dropped:
/// also while this process waits on a slow reader of its output, or for
/// the container's end. SIGTERM is a stop (the container's stop signal,
/// then SIGKILL once its stop timeout is up: `docker stop`'s); SIGWINCH
/// of a terminal container is a resize (its session's); the others go
/// as they are. The last SIGINT or SIGQUIT is noted.
struct Forwarder<'scope> {
    stop: std::os::fd::OwnedFd,
    thread: Option<std::thread::ScopedJoinHandle<'scope, ()>>,
}

impl<'scope> Forwarder<'scope> {
    fn start<'env>(
        scope: &'scope std::thread::Scope<'scope, 'env>,
        watch: &'scope shipyard_runtime::process::SignalWatch,
        to: Forwarding<'scope>,
    ) -> Result<Self> {
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
        let (stopped, stop) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
        // SIGTSTP held off on the thread (it inherits this mask): the
        // terminal's raw mode handles it, on this thread only.
        let mask = SigSet::thread_get_mask()?;
        let mut held = mask;
        held.add(nix::sys::signal::Signal::SIGTSTP);
        held.thread_set_mask()?;
        let thread = scope.spawn(move || {
            loop {
                let mut fds = [
                    PollFd::new(watch.fd(), PollFlags::POLLIN),
                    PollFd::new(std::os::fd::AsFd::as_fd(&stopped), PollFlags::POLLIN),
                ];
                match poll(&mut fds, PollTimeout::NONE) {
                    Ok(_) => {}
                    Err(nix::errno::Errno::EINTR) => continue,
                    // Can't watch: nothing more is passed on.
                    Err(_) => return,
                }
                let asked_to_stop = fds
                    .get(1)
                    .and_then(PollFd::revents)
                    .is_some_and(|events| !events.is_empty());
                let taken = watch.take();
                // Resumed (`fg`, after the shell set the terminal as it
                // keeps it for the job): the session looks at raw mode
                // again, and at the window's size.
                if taken.continued
                    && let Some(resizes) = to.resizes
                {
                    let _ = nix::unistd::write(resizes, &[1]);
                }
                for signal in taken.signals {
                    if signal.as_raw() == libc::SIGWINCH
                        && let Some(resizes) = to.resizes
                    {
                        // Full already: a resize is due anyway.
                        let _ = nix::unistd::write(resizes, &[1]);
                        continue;
                    }
                    if matches!(signal.as_raw(), libc::SIGINT | libc::SIGQUIT) {
                        to.interrupted
                            .store(signal.as_raw(), std::sync::atomic::Ordering::Relaxed);
                    }
                    pass_on(to.root, to.record, signal);
                }
                if asked_to_stop {
                    return;
                }
            }
        });
        mask.thread_set_mask()?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Forwarder<'_> {
    fn drop(&mut self) {
        let _ = nix::unistd::write(&self.stop, &[1]);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Passes `signal` on to container `record` (under the runtime state
/// `root`): SIGTERM as its stop, the others to its init. One that has
/// ended meanwhile takes none.
fn pass_on(root: &std::path::Path, record: &Record, signal: shipyard_runtime::process::Signal) {
    tracing::debug!(%signal, "forwarding a signal to the container");
    let passed = if signal.as_raw() == libc::SIGTERM {
        crate::run::stop::StopOptions::default()
            .policy(record)
            .and_then(|policy| crate::run::stop::stop_up(root, record, policy))
    } else {
        runtime::kill(root, record.id.as_str(), signal).map_err(Error::from)
    };
    if let Err(error) = passed
        && !matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotRunning)
    {
        tracing::debug!("couldn't pass {signal} on: {error:#}");
    }
}

/// Container `record`'s exit code, once it ends: from its monitor (a
/// waiter's: `waiter`, let in before it started, else one now; told
/// before its clean-up, so even a `--rm` one's), its end then let go
/// of; else, ended already, from its record.
///
/// # Errors
///
/// Its monitor refusing to be waited on; ended and removed (`--rm`)
/// before its code could be read ([`ErrorKind::NotFound`]).
fn end_code(
    engine: &Engine,
    root: &std::path::Path,
    record: &Record,
    waiter: Option<runtime::Waiter>,
) -> Result<i32> {
    let told = match waiter {
        Some(waiter) => waiter.exit_code(),
        None => runtime::wait(root, record.id.as_str()),
    };
    match told {
        Ok(Some(code)) => {
            containers::wait_settled(engine, &record.id, containers::SETTLE_WAIT);
            return Ok(code);
        }
        // Up still, and its monitor won't say (too many waiters).
        Err(error) if containers::has_monitor(engine, &record.id) => {
            return Err(
                Error::from(error).context(format!("waiting for container {} to end", record.name))
            );
        }
        _ => {}
    }
    if let Some(code) = crate::attach::recorded_end(engine, record) {
        return Ok(code);
    }
    bail!(
        NotFound,
        "container {} ended and was removed (--rm) before its exit code could be read",
        record.name
    )
}

/// Says so if `--rm` didn't remove `record`'s container as it ended.
fn warn_if_left(engine: &Engine, record: &Record) {
    if containers::read_one(engine, &record.id).is_ok() {
        engine.warn(
            WarningKind::LeftForLater,
            format!(
                "container {} wasn't removed as it ended (--rm)",
                record.name
            ),
        );
    }
}

/// After a hang-up: a later SIGHUP (the terminal's session leader sends
/// its foreground group one as it ends) mustn't end this process before
/// it says how the run went. Called while SIGHUP is still held off (the
/// watch put back the mask it was made under): ignored, a pending one
/// goes too.
fn ignore_hangups() {
    // SAFETY: ignoring a signal installs no handler.
    let _ = unsafe {
        nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGHUP,
            nix::sys::signal::SigHandler::SigIgn,
        )
    };
}

/// Puts the signal mask `mask` back, if there is one.
fn restore_mask(mask: Option<&SigSet>) {
    if let Some(mask) = mask {
        let _ = mask.thread_set_mask();
    }
}

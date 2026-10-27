//! A command forwarded to the guest: tried until the guest is ready, again
//! while a stopping VM ends, and run as one session.

use std::time::{Duration, Instant};

use shipyard_runtime::attach::Frame;
use shipyard_runtime::attach::forward::{self, Connection, Request};
use shipyard_runtime::process::SignalWatch;

use crate::{VmError, VmPaths};

use super::host::last_lines;
use super::session::{Writer, on_terminal, open_control, pass_signals, pump_stdin, relay, send};
use super::{
    NOT_READY_REST, Outcome, SAY_NOT_READY_AFTER, START_LIMIT, Tried, connect_watching,
    interrupted, status, stopping,
};

/// Runs `request` in the guest: what it writes goes to this process's
/// stdout and stderr; with its `input`, what this process reads from stdin
/// goes to it (without, stdin isn't read: a script's next lines stay
/// there). The signals this process gets (a Ctrl-C, a kill) are passed on
/// to it, as a `docker run`'s are to its container; a third Ctrl-C, or a
/// SIGTERM it doesn't end within [`TERM_GRACE`](super::TERM_GRACE), ends this process instead
/// (the command, its connection gone, is hung up on in the VM). The
/// request's time is this Mac's clock as it's sent (after the VM's start).
/// While it waits for the VM (its start, a stop it found under way), a
/// SIGINT, SIGTERM or SIGHUP ends it before the command is sent:
/// [`VmError::Interrupted`].
///
/// # Errors
///
/// [`VmError::Refused`] if the guest refused it, [`VmError::Gone`] if the VM
/// went before it ended, [`VmError::Host`] if the VM can't be reached.
pub fn forward(paths: &VmPaths, request: &Request) -> Result<Outcome, VmError> {
    // Installed before anything is read: a Ctrl-C from here on is the
    // command's. (One per process: both tries share it.)
    let watch =
        SignalWatch::install().map_err(|e| VmError::Host(format!("watching for signals: {e}")))?;
    tries(paths, request, &mut Some(watch))
}

/// [`forward`](fn@forward)'s tries: a second, once the VM has stopped, if the first
/// was refused as it stopped; never a third. `watch` is the process's
/// signal watch, taken by the try that runs the command.
pub(super) fn tries(
    paths: &VmPaths,
    request: &Request,
    watch: &mut Option<SignalWatch>,
) -> Result<Outcome, VmError> {
    match until_ready(paths, request, watch) {
        // The host was stopping (idle): the command never reached the
        // guest. Once that host is gone, once more (starting the VM).
        Err(Tried::Stopping(host)) => {
            wait_until_stopped(paths, host, watch.as_ref())?;
            until_ready(paths, request, watch).map_err(Tried::into_error)
        }
        tried => tried.map_err(Tried::into_error),
    }
}

/// [`attempt`]s, again and again while the guest isn't ready (its data
/// disk checked or repaired: D10.4-4, so no bound cuts a repair short),
/// said on stderr once it's taken a few seconds; given up on as `watch`
/// catches a signal that ends a wait.
fn until_ready(
    paths: &VmPaths,
    request: &Request,
    watch: &mut Option<SignalWatch>,
) -> Result<Outcome, Tried> {
    let began = Instant::now();
    let mut said = false;
    loop {
        match attempt(paths, request, watch) {
            Err(Tried::NotReady) => {}
            tried => return tried,
        }
        if let Some(code) = interrupted(watch.as_ref()) {
            return Err(VmError::Interrupted(code).into());
        }
        if !said && began.elapsed() >= SAY_NOT_READY_AFTER {
            said = true;
            let _ = std::io::Write::write_fmt(
                &mut std::io::stderr(),
                format_args!(
                    "shipyard: {}: waiting (Ctrl-C to stop waiting)\n",
                    forward::NOT_READY
                ),
            );
        }
        std::thread::sleep(NOT_READY_REST);
    }
}

/// Waits for the stopping host (`host`, its PID) to be gone: its lock free,
/// or held by another (a new host another CLI started meanwhile), within
/// [`START_LIMIT`], or past it while the host says it's still within its
/// own bound (a stop that waits out a disk check, up to the longest stop:
/// the whole-stage review); given up on as `watch` catches a signal that
/// ends a wait.
fn wait_until_stopped(
    paths: &VmPaths,
    host: Option<u32>,
    watch: Option<&SignalWatch>,
) -> Result<(), VmError> {
    let began = Instant::now();
    let mut deadline = began + START_LIMIT;
    loop {
        if super::gone_or_replaced(status(paths)?, host) {
            return Ok(());
        }
        if let Some(code) = interrupted(watch) {
            return Err(VmError::Interrupted(code));
        }
        deadline = super::waited_on(paths, began, deadline);
        if Instant::now() > deadline {
            return Err(VmError::Host(format!(
                "the VM was stopping, and still is after {} s: {}",
                began.elapsed().as_secs(),
                last_lines(&paths.host_log(), 5)
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// One try at [`forward`](fn@forward): refused as the host stops, nothing of this
/// process's input read yet (`watch` taken only once it's welcomed).
fn attempt(
    paths: &VmPaths,
    request: &Request,
    watch: &mut Option<SignalWatch>,
) -> Result<Outcome, Tried> {
    let stream = connect_watching(paths, watch.as_ref())?;
    let mut request = request.clone();
    request.time = forward::clock();
    let hello = forward::hello(&request).map_err(|e| VmError::Host(e.to_string()))?;
    if request.terminal.is_some() {
        let internal = forward::is_internal(&request.args);
        return on_terminal(paths, stream, (&hello, internal), request.input, watch);
    }
    let writer: Writer = std::sync::Arc::new(std::sync::Mutex::new(
        stream
            .try_clone()
            .map_err(|e| VmError::Host(format!("the VM's connection: {e}")))?,
    ));
    // A host that's stopping may refuse, and close, before the hello is
    // written: its answer is read either way.
    let sent = send(&writer, &hello);
    let mut connection = Connection::new(stream);
    let first = connection.next_frame();
    match first
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .and_then(forward::Refusal::of)
    {
        Some(forward::Refusal::Stopping) => return Err(stopping(paths)),
        Some(forward::Refusal::NotReady) => return Err(Tried::NotReady),
        _ => {}
    }
    sent?;
    let mut first =
        first.map_err(|e| VmError::Gone(format!("the VM's answer can't be read: {e}")))?;
    // A newer guest's token (F94) follows its welcome: the command's
    // control connection, opened before any signal is passed on, takes its
    // signals (a command that reads no input still gets them).
    let mut control = None;
    if let Some(Frame::Welcome { version, .. }) = first
        && version >= shipyard_runtime::attach::SESSION_VERSION
        && !forward::is_internal(&request.args)
    {
        first = connection
            .next_frame()
            .map_err(|e| VmError::Gone(format!("the VM's answer can't be read: {e}")))?;
        if let Some(Frame::Session(token)) = first {
            control = (token != 0)
                .then(|| open_control(paths, token))
                .flatten()
                .map(|control| std::sync::Arc::new(std::sync::Mutex::new(control)));
        }
    }
    if let Some(watch) = watch.take() {
        pass_signals(watch, &writer, control);
    }
    if request.input {
        let writer = std::sync::Arc::clone(&writer);
        std::thread::spawn(move || pump_stdin(&writer));
    }
    Ok(relay(first, connection)?)
}

//! A forwarded command's session: on a terminal (the attach client's) or
//! relayed frame by frame, its signals passed on, and how the CLI leaves one
//! that doesn't end.

use std::io::Read as _;
use std::os::unix::net::UnixStream;
use std::time::Instant;

use shipyard_runtime::attach::Frame;
use shipyard_runtime::attach::client::{self, Act, AttachError, AttachOptions, Ended};
use shipyard_runtime::attach::forward::{self, Connection};
use shipyard_runtime::process::SignalWatch;

use crate::{VmError, VmPaths};

use super::{Outcome, Tried, refusal_of, stopping};

/// The writing half of a forwarded command's connection, shared by what
/// sends input and what sends signals: one frame at a time, whole.
pub(super) type Writer = std::sync::Arc<std::sync::Mutex<UnixStream>>;

/// [`forward`](fn@super::forward) for a command on a terminal in the guest (step 10.3b2,
/// D10.3-7): the attach client's session, as `shipyard attach` has, its
/// hello `hello`. With `input`, this terminal is raw (each key a byte: a
/// Ctrl-C is the guest terminal's to make a SIGINT of); its window's size
/// follows this one's; the signals this process gets are passed on, with
/// [`forward`](fn@super::forward)'s ways out, the terminal set back first.
pub(super) fn on_terminal(
    paths: &VmPaths,
    stream: UnixStream,
    hello: &Frame,
    input: bool,
    watch: &mut Option<SignalWatch>,
) -> Result<Outcome, Tried> {
    let options = AttachOptions {
        detach_keys: shipyard_runtime::attach::DetachKeys::none(),
        stdin: input,
        held: false,
        output: client::Output::BOTH,
    };
    let welcomed = match client::welcomed_with(stream, hello, &options) {
        Ok(welcomed) => welcomed,
        Err(error) => {
            return Err(match refusal_of(&error) {
                Some(forward::Refusal::Stopping) => stopping(paths),
                Some(forward::Refusal::NotReady) => Tried::NotReady,
                _ => session_error(error).into(),
            });
        }
    };
    let Some(watch) = watch.take() else {
        return Err(VmError::Host("no signal watch left".to_owned()).into());
    };
    let mut leaving = Leaving::default();
    let mut left = None;
    let over = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut decide = |signal: shipyard_runtime::process::Signal| {
        let (raw, now) = (signal.as_raw(), Instant::now());
        let grace_was_on = leaving.terminated_at.is_some();
        if let Some(way) = leaving.on(raw, now).or_else(|| leaving.over(now)) {
            left = Some(way);
            return Act::Leave(way.code());
        }
        if !grace_was_on && leaving.terminated_at.is_some() {
            wake_at_grace_end(std::sync::Arc::clone(&over));
        }
        linux_number(raw).map_or(Act::Skip, Act::Send)
    };
    let ended = welcomed.session_signalled(&watch, &mut decide);
    // No wake-up from here: the session's watch, its handlers, are going,
    // and a SIGTERM would end this process.
    over.store(true, std::sync::atomic::Ordering::SeqCst);
    let ended = ended.map_err(session_error)?;
    Ok(match ended {
        Ended::Exited(code) => Outcome::Exited(code),
        // This terminal closed: the command is hung up on (the agent sees
        // the connection go), as a shell's would be. The watch's handlers
        // stay until this process exits: the shell that owned the terminal
        // ends too, and the kernel sends this group a second SIGHUP then,
        // which the default its drop puts back would die of (a vm_terminal
        // run saw `Signaled(1)` for 129).
        Ended::HungUp => {
            std::mem::forget(watch);
            Outcome::Exited(129)
        }
        Ended::Left(code) => {
            if let Some(way) = left {
                say_left(way);
            }
            Outcome::Exited(code)
        }
        // No detach keys, and the agent never lets go.
        Ended::Detached | Ended::Dropped(_) => {
            return Err(VmError::Gone(
                "the guest let go of the command before it ended".to_owned(),
            )
            .into());
        }
    })
}

/// A terminal session's failure, as a forwarded command's.
fn session_error(error: AttachError) -> VmError {
    match error {
        AttachError::Refused(why) => VmError::Refused(why),
        AttachError::Version(version) => VmError::Guest(format!(
            "the guest speaks attach protocol version {version}: rebuild the guest \
             (scripts/guest/build.sh)"
        )),
        AttachError::NoAnswer => {
            VmError::Gone("the guest didn't answer the command in time".to_owned())
        }
        // This terminal's, or the VM's connection's.
        AttachError::Io(error) => VmError::Host(format!("the command's session: {error}")),
        AttachError::NotLetIn | AttachError::MonitorGone => VmError::Gone(
            "the VM went before the command ended (its console log may say why)".to_owned(),
        ),
        AttachError::Unexpected(_) | AttachError::Protocol(_) | _ => VmError::Gone(
            "the VM sent something a forwarded command's answer can't hold".to_owned(),
        ),
    }
}

/// After [`TERM_GRACE`], a SIGTERM to this process: the wake-up the
/// terminal's session (which sees only signals) needs to find the grace
/// over, and leave with its terminal set back. None once the session is
/// `over`.
fn wake_at_grace_end(over: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
        std::thread::sleep(TERM_GRACE);
        if !over.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM);
        }
    });
}

/// What the guest sends, to this process's stdout and stderr, until the
/// command's end. Only stdout's reader going (`… | head`) is the quiet end
/// a pipe's reader makes; stderr's going only stops what's written there.
pub(super) fn relay(
    first: Option<Frame>,
    mut connection: Connection<UnixStream>,
) -> Result<Outcome, VmError> {
    let mut stderr_open = true;
    let rest = std::iter::repeat_with(|| {
        connection
            .next_frame()
            .map_err(|e| VmError::Gone(format!("the VM's answer can't be read: {e}")))
    });
    for frame in std::iter::once(Ok(first)).chain(rest) {
        match frame? {
            Some(Frame::Stdout(bytes)) => match write_out(&mut std::io::stdout(), &bytes) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {
                    return Ok(Outcome::ReaderGone);
                }
                Err(error) => return Err(VmError::Host(format!("writing the output: {error}"))),
            },
            Some(Frame::Stderr(bytes)) if stderr_open => {
                stderr_open = write_out(&mut std::io::stderr(), &bytes).is_ok();
            }
            Some(Frame::Welcome { .. } | Frame::Stderr(_)) => {}
            Some(Frame::Exited(code)) => return Ok(Outcome::Exited(code)),
            // A refusal of any kind, a newer peer's too: its words.
            Some(frame) if let Some(refusal) = forward::Refusal::of(&frame) => {
                return Err(VmError::Refused(refusal.to_string()));
            }
            Some(_) => {
                return Err(VmError::Gone(
                    "the VM sent something a forwarded command's answer can't hold".to_owned(),
                ));
            }
            None => {
                return Err(VmError::Gone(
                    "the VM went before the command ended (its console log may say why)".to_owned(),
                ));
            }
        }
    }
    // (Never reached: the frames end in a return.)
    Err(VmError::Gone("the VM's answer ended".to_owned()))
}

/// How many Ctrl-C's end this process rather than being passed on.
const INTERRUPTS_TO_LEAVE: usize = 3;

/// How long a SIGTERM passed on may take before this process ends anyway.
pub const TERM_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// How this process lets go of a forwarded command (the agent, its
/// connection gone, hangs it up).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Way {
    /// A third Ctrl-C ([`INTERRUPTS_TO_LEAVE`]).
    Interrupted,
    /// A SIGTERM passed on that the command didn't end by ([`TERM_GRACE`]).
    Unterminated,
}

impl Way {
    /// This process's exit code: the signal's, as a shell gives.
    const fn code(self) -> i32 {
        match self {
            Self::Interrupted => 130,
            Self::Unterminated => 143,
        }
    }
}

/// The ways out of a forwarded command, counted the same on both paths:
/// each signal this process gets, and the time.
#[derive(Debug, Default)]
struct Leaving {
    interrupts: usize,
    /// When the first SIGTERM came (a later one is only passed on).
    terminated_at: Option<Instant>,
}

impl Leaving {
    /// Counts the signal `raw`, come at `now`: the way out it makes.
    fn on(&mut self, raw: i32, now: Instant) -> Option<Way> {
        if raw == libc::SIGINT {
            self.interrupts += 1;
            if self.interrupts >= INTERRUPTS_TO_LEAVE {
                return Some(Way::Interrupted);
            }
        }
        if raw == libc::SIGTERM && self.terminated_at.is_none() {
            self.terminated_at = Some(now);
        }
        None
    }

    /// The way out the time makes at `now`: a SIGTERM's grace over.
    fn over(&self, now: Instant) -> Option<Way> {
        self.terminated_at
            .is_some_and(|at| now.duration_since(at) >= TERM_GRACE)
            .then_some(Way::Unterminated)
    }
}

/// Says on stderr how this process let go of the command.
fn say_left(way: Way) {
    use std::io::Write as _;
    let why = match way {
        Way::Interrupted => format!("interrupted {INTERRUPTS_TO_LEAVE} times"),
        Way::Unterminated => format!(
            "the command didn't end within {} s of the SIGTERM passed on",
            TERM_GRACE.as_secs()
        ),
    };
    let _ = writeln!(
        std::io::stderr(),
        "shipyard: {why}: letting go of the command (in the VM, it's hung up on)"
    );
}

/// The Linux number of a signal this Mac gave (Linux numbers are the
/// wire's): the common ones only. One Linux hasn't, or numbers differently
/// with no twin (SIGINFO, Ctrl-T's), is `None`: not passed on.
#[must_use]
pub fn linux_number(signal: i32) -> Option<u8> {
    Some(match signal {
        libc::SIGHUP => 1,
        libc::SIGINT => 2,
        libc::SIGQUIT => 3,
        libc::SIGUSR1 => 10,
        libc::SIGUSR2 => 12,
        libc::SIGALRM => 14,
        libc::SIGTERM => 15,
        libc::SIGWINCH => 28,
        _ => return None,
    })
}

/// Passes the signals `watch` catches on to the command, from a thread
/// that owns the watch for as long as this process runs (its handlers stay
/// till the process ends: the forwarding is the process's last work). The
/// way out: a third Ctrl-C ends this process at once (130), and so does a
/// SIGTERM the command hasn't ended by within [`TERM_GRACE`] (143).
pub(super) fn pass_signals(watch: SignalWatch, writer: &Writer) {
    let writer = std::sync::Arc::clone(writer);
    std::thread::spawn(move || {
        let mut leaving = Leaving::default();
        loop {
            let mut fds = [nix::poll::PollFd::new(
                watch.fd(),
                nix::poll::PollFlags::POLLIN,
            )];
            match nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(1000_u16)) {
                Ok(_) | Err(nix::errno::Errno::EINTR) => {}
                Err(_) => return,
            }
            if let Some(way) = leaving.over(Instant::now()) {
                leave(way);
            }
            for signal in watch.take().signals {
                let raw = signal.as_raw();
                if let Some(way) = leaving.on(raw, Instant::now()) {
                    leave(way);
                }
                let Some(number) = linux_number(raw) else {
                    continue;
                };
                if send(&writer, &Frame::Signal(number)).is_err() {
                    return;
                }
            }
        }
    });
}

/// Ends this process the `way` it lets go, saying so.
fn leave(way: Way) -> ! {
    say_left(way);
    std::process::exit(way.code())
}

/// Sends `frame` on the shared writer.
pub(super) fn send(writer: &Writer, frame: &Frame) -> Result<(), VmError> {
    let mut stream = writer
        .lock()
        .map_err(|_| VmError::Host("the connection's writer, poisoned".to_owned()))?;
    forward::send(&mut *stream, frame).map_err(|e| VmError::Gone(format!("sending to the VM: {e}")))
}

/// Sends what this process reads from stdin, then its end.
pub(super) fn pump_stdin(writer: &Writer) {
    let mut buffer = vec![0; 32 * 1024];
    let mut stdin = std::io::stdin().lock();
    loop {
        let frame = match stdin.read(&mut buffer) {
            Ok(0) => Frame::StdinEnd,
            Ok(read) => Frame::Stdin(buffer[..read].to_vec()),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => Frame::StdinEnd,
        };
        let end = frame == Frame::StdinEnd;
        if send(writer, &frame).is_err() || end {
            return;
        }
    }
}

/// Writes `bytes` to `out`, flushed.
fn write_out(out: &mut impl std::io::Write, bytes: &[u8]) -> std::io::Result<()> {
    out.write_all(bytes).and_then(|()| out.flush())
}

#[cfg(test)]
mod tests;

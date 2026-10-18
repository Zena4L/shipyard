//! The Mac CLI's side (step 10.3): starting the VM's host if it isn't
//! running, forwarding a command through it, and its status and stop.

use std::io::Read as _;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use shipyard_runtime::attach::Frame;
use shipyard_runtime::attach::client::{self, Act, AttachError, AttachOptions, Ended};
use shipyard_runtime::attach::forward::{self, Connection, Request};
use shipyard_runtime::process::SignalWatch;

use crate::{HOST_ARG, VmError, VmPaths};

/// How long a first command waits for the VM: longer than its host waits
/// for the guest (`host::BOOT_LIMIT`), and the VM's own start.
const START_LIMIT: Duration = Duration::from_secs(150);

/// How long `vm stop` waits for the host to be gone.
const STOP_LIMIT: Duration = Duration::from_secs(120);

/// The host's log's size past which it's moved aside at a start.
const HOST_LOG_KEPT: u64 = 1 << 20;

/// The most the lock file's PID is read as.
const PID_CAP: u64 = 64;

/// Whether the VM's host is running, and its PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// It runs (its lock is held), with this PID if it wrote one.
    Running(Option<u32>),
    /// It doesn't.
    Stopped,
}

/// Whether the host runs: its lock held is the only sign (a PID alone
/// could be another process's by now).
///
/// # Errors
///
/// [`VmError::Io`] if the lock file can't be read.
pub fn status(paths: &VmPaths) -> Result<Status, VmError> {
    let path = paths.lock();
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Status::Stopped),
        Err(error) => return Err(VmError::io(&path, error)),
    };
    // (A host starting in this moment finds the lock taken, and leaves:
    // its CLI starts another, `connect` below.)
    match file.try_lock_shared() {
        Ok(()) => Ok(Status::Stopped),
        Err(std::fs::TryLockError::WouldBlock) => {
            let pid = shipyard_sys::fs::read_bounded(&path, PID_CAP)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|text| text.trim().parse().ok());
            Ok(Status::Running(pid))
        }
        Err(std::fs::TryLockError::Error(error)) => Err(VmError::io(&path, error)),
    }
}

/// A connection to the host, starting it first if it isn't running (and
/// waiting for its guest to boot). A host started here that finds the lock
/// taken leaves: then this waits for that other host's socket, or, if it
/// was stopping, for its lock to be free, and starts one again.
///
/// # Errors
///
/// [`VmError::Host`]: it didn't start, or its socket didn't come; with its
/// log's last lines.
pub fn connect(paths: &VmPaths) -> Result<UnixStream, VmError> {
    connect_watching(paths, None)
}

/// [`connect`], given up on as `watch` catches a signal that ends a
/// wait ([`interrupted`]).
fn connect_watching(paths: &VmPaths, watch: Option<&SignalWatch>) -> Result<UnixStream, VmError> {
    paths.prepare(nix::unistd::getuid().as_raw())?;
    if let Ok(stream) = UnixStream::connect(paths.socket()) {
        return Ok(stream);
    }
    let deadline = Instant::now() + START_LIMIT;
    let mut host = start_host(paths)?;
    loop {
        if let Ok(stream) = UnixStream::connect(paths.socket()) {
            return Ok(stream);
        }
        let ended = host.try_wait().ok().flatten();
        let failed = ended.is_some_and(|status| !status.success());
        if failed || Instant::now() > deadline {
            if !failed {
                // Not killed: asked to stop, it stops its guest first.
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(i32::try_from(host.id()).unwrap_or(-1)),
                    nix::sys::signal::Signal::SIGTERM,
                );
            }
            return Err(VmError::Host(format!(
                "the VM didn't start{}: {}",
                if failed { "" } else { " in time" },
                last_lines(&paths.host_log(), 5)
            )));
        }
        // Ours left (another host had the lock), and there's still no
        // socket: once no host holds the lock, start one again.
        if ended.is_some() && status(paths)? == Status::Stopped {
            host = start_host(paths)?;
        }
        if let Some(code) = interrupted(watch) {
            return Err(VmError::Interrupted(code));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Starts the host: this binary again, in a session of its own (a closed
/// terminal doesn't end it), in `/` (it holds no directory of the
/// caller's), with nothing the CLI inherited, its output to its log.
fn start_host(paths: &VmPaths) -> Result<std::process::Child, VmError> {
    let exe = std::env::current_exe().map_err(|e| VmError::Host(format!("this binary: {e}")))?;
    let log_path = paths.host_log();
    if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > HOST_LOG_KEPT) {
        let _ = std::fs::rename(&log_path, log_path.with_extension("log.1"));
    }
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .map_err(|e| VmError::io(&log_path, e))?;
    let err = log.try_clone().map_err(|e| VmError::io(&log_path, e))?;
    let bound = descriptor_bound();
    let mut command = Command::new(exe);
    command
        .arg(HOST_ARG)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err);
    // SAFETY: between fork and exec, `setsid` and `fcntl` only: both
    // async-signal-safe, no allocation, no lock. The CLI is single-threaded
    // here, and has used no Objective-C: the host process is a fresh exec.
    unsafe {
        command.pre_exec(move || {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            // Whatever the CLI inherited open (a caller's pipe) closes at
            // the exec: the host outlives the CLI, and would hold it.
            for fd in 3..bound {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            Ok(())
        });
    }
    command
        .spawn()
        .map_err(|e| VmError::Host(format!("starting the VM's host: {e}")))
}

/// The numbers a descriptor may have here: the soft limit, bounded.
fn descriptor_bound() -> i32 {
    nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_NOFILE)
        .map_or(1024, |(soft, _)| {
            i32::try_from(soft.min(65_536)).unwrap_or(1024)
        })
}

/// The last `count` lines of `path` (or why there are none).
fn last_lines(path: &std::path::Path, count: usize) -> String {
    std::fs::read_to_string(path).map_or_else(
        |error| format!("no log ({error})"),
        |text| {
            let lines: Vec<&str> = text.lines().collect();
            lines[lines.len().saturating_sub(count)..].join(" / ")
        },
    )
}

/// How a forwarded command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// With this exit code.
    Exited(i32),
    /// This process's stdout reader went (`… | head`): not a failure.
    ReaderGone,
}

/// The writing half of a forwarded command's connection, shared by what
/// sends input and what sends signals: one frame at a time, whole.
type Writer = std::sync::Arc<std::sync::Mutex<UnixStream>>;

/// How long an internal request's answer may take ([`ask`]).
const ASK_LIMIT: Duration = Duration::from_secs(30);

/// The output of an internal request (`args`: `__guest-doctor`, say) to the
/// guest, read up to `cap`, if the VM runs: never started for it (`None`
/// when it isn't running, or is stopping), so asking changes nothing.
///
/// # Errors
///
/// [`VmError::Gone`]: the guest didn't answer, or answered as an older
/// guest does (the request unknown to it).
pub fn ask(paths: &VmPaths, args: &[&str], cap: usize) -> Result<Option<Vec<u8>>, VmError> {
    if status(paths)? == Status::Stopped {
        return Ok(None);
    }
    let Ok(mut stream) = UnixStream::connect(paths.socket()) else {
        return Ok(None);
    };
    // (macOS refuses a timeout, EINVAL, on a socket whose peer has closed
    // already: its reads end at once then, and need none.)
    let _ = stream.set_read_timeout(Some(ASK_LIMIT));
    let hello =
        forward::hello(&Request::internal(args)).map_err(|e| VmError::Host(e.to_string()))?;
    // (A host that's stopping refuses, maybe before this is written.)
    let _ = forward::send(&mut stream, &hello);
    match crate::forwarding::output(&mut Connection::new(stream), cap) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(why) if why == forward::STOPPING => Ok(None),
        Err(why) => Err(VmError::Gone(why)),
    }
}

/// Runs `request` in the guest: what it writes goes to this process's
/// stdout and stderr; with its `input`, what this process reads from stdin
/// goes to it (without, stdin isn't read: a script's next lines stay
/// there). The signals this process gets (a Ctrl-C, a kill) are passed on
/// to it, as a `docker run`'s are to its container; a third Ctrl-C, or a
/// SIGTERM it doesn't end within [`TERM_GRACE`], ends this process instead
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

/// [`forward`]'s tries: a second, once the VM has stopped, if the first
/// was refused as it stopped; never a third. `watch` is the process's
/// signal watch, taken by the try that runs the command.
fn tries(
    paths: &VmPaths,
    request: &Request,
    watch: &mut Option<SignalWatch>,
) -> Result<Outcome, VmError> {
    match attempt(paths, request, watch) {
        // The host was stopping (idle): the command never reached the
        // guest. Once that host is gone, once more (starting the VM).
        Err(Tried::Stopping(host)) => {
            wait_until_stopped(paths, host, watch.as_ref())?;
            attempt(paths, request, watch).map_err(Tried::into_error)
        }
        tried => tried.map_err(Tried::into_error),
    }
}

/// Why a try at [`forward`] didn't run its command.
enum Tried {
    /// The host refused it as it stopped: that host's PID, if known.
    Stopping(Option<u32>),
    /// Anything else.
    Failed(VmError),
}

impl Tried {
    fn into_error(self) -> VmError {
        match self {
            Self::Stopping(_) => VmError::Refused(forward::STOPPING.to_owned()),
            Self::Failed(error) => error,
        }
    }
}

impl From<VmError> for Tried {
    fn from(error: VmError) -> Self {
        Self::Failed(error)
    }
}

/// Waits for the stopping host (`host`, its PID) to be gone: its lock free,
/// or held by another (a new host another CLI started meanwhile), within
/// [`START_LIMIT`]; given up on as `watch` catches a signal that ends a
/// wait.
fn wait_until_stopped(
    paths: &VmPaths,
    host: Option<u32>,
    watch: Option<&SignalWatch>,
) -> Result<(), VmError> {
    let deadline = Instant::now() + START_LIMIT;
    loop {
        match status(paths)? {
            Status::Stopped => return Ok(()),
            Status::Running(Some(pid)) if host.is_some_and(|host| host != pid) => return Ok(()),
            Status::Running(_) => {}
        }
        if let Some(code) = interrupted(watch) {
            return Err(VmError::Interrupted(code));
        }
        if Instant::now() > deadline {
            return Err(VmError::Host(format!(
                "the VM was stopping, and still is after {} s: {}",
                START_LIMIT.as_secs(),
                last_lines(&paths.host_log(), 5)
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The exit code for a signal `watch` caught that ends a wait for the VM
/// (SIGINT, SIGTERM, SIGHUP: 128 + its number), the others dropped (none
/// reaches a command not yet sent).
fn interrupted(watch: Option<&SignalWatch>) -> Option<i32> {
    let caught = watch?.take().signals;
    caught
        .iter()
        .map(|signal| signal.as_raw())
        .find(|&raw| [libc::SIGINT, libc::SIGTERM, libc::SIGHUP].contains(&raw))
        .map(|raw| 128 + raw)
}

/// [`Tried::Stopping`], with the stopping host's PID.
fn stopping(paths: &VmPaths) -> Tried {
    match status(paths) {
        Ok(Status::Running(pid)) => Tried::Stopping(pid),
        _ => Tried::Stopping(None),
    }
}

/// One try at [`forward`]: refused as the host stops, nothing of this
/// process's input read yet (`watch` taken only once it's welcomed).
fn attempt(
    paths: &VmPaths,
    request: &Request,
    watch: &mut Option<SignalWatch>,
) -> Result<Outcome, Tried> {
    let stream = connect_watching(paths, watch.as_ref())?;
    let mut request = request.clone();
    request.time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs());
    let hello = forward::hello(&request).map_err(|e| VmError::Host(e.to_string()))?;
    if request.terminal.is_some() {
        return on_terminal(paths, stream, &hello, request.input, watch);
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
    if matches!(&first, Ok(Some(Frame::Refused(why))) if why == forward::STOPPING) {
        return Err(stopping(paths));
    }
    sent?;
    let first = first.map_err(|e| VmError::Gone(format!("the VM's answer can't be read: {e}")))?;
    if let Some(watch) = watch.take() {
        pass_signals(watch, &writer);
    }
    if request.input {
        let writer = std::sync::Arc::clone(&writer);
        std::thread::spawn(move || pump_stdin(&writer));
    }
    Ok(relay(first, connection)?)
}

/// [`forward`] for a command on a terminal in the guest (step 10.3b2,
/// D10.3-7): the attach client's session, as `shipyard attach` has, its
/// hello `hello`. With `input`, this terminal is raw (each key a byte: a
/// Ctrl-C is the guest terminal's to make a SIGINT of); its window's size
/// follows this one's; the signals this process gets are passed on, with
/// [`forward`]'s ways out, the terminal set back first.
fn on_terminal(
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
    };
    let welcomed = match client::welcomed_with(stream, hello, &options) {
        Ok(welcomed) => welcomed,
        Err(AttachError::Refused(why)) if why == forward::STOPPING => {
            return Err(stopping(paths));
        }
        Err(error) => return Err(session_error(error).into()),
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
        // the connection go), as a shell's would be.
        Ended::HungUp => Outcome::Exited(129),
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
fn relay(first: Option<Frame>, mut connection: Connection<UnixStream>) -> Result<Outcome, VmError> {
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
            Some(Frame::Refused(why)) => return Err(VmError::Refused(why)),
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
fn pass_signals(watch: SignalWatch, writer: &Writer) {
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
fn send(writer: &Writer, frame: &Frame) -> Result<(), VmError> {
    let mut stream = writer
        .lock()
        .map_err(|_| VmError::Host("the connection's writer, poisoned".to_owned()))?;
    forward::send(&mut *stream, frame).map_err(|e| VmError::Gone(format!("sending to the VM: {e}")))
}

/// Sends what this process reads from stdin, then its end.
fn pump_stdin(writer: &Writer) {
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

/// Stops the VM in order (its host asked by SIGTERM: the guest stops its
/// containers and powers off), and waits for that host to be gone (a host
/// another command starts meanwhile is another VM, not waited for).
/// Nothing to do if it isn't running.
///
/// # Errors
///
/// [`VmError::Host`]: it's still running after the wait.
pub fn stop(paths: &VmPaths) -> Result<(), VmError> {
    let deadline = Instant::now() + STOP_LIMIT;
    // (A host that has just taken the lock writes its PID next.)
    let pid = loop {
        match status(paths)? {
            Status::Stopped => return Ok(()),
            Status::Running(Some(pid)) => break pid,
            Status::Running(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Status::Running(None) => {
                return Err(VmError::Host(
                    "the VM's host runs, but its PID isn't known".to_owned(),
                ));
            }
        }
    };
    let pid = i32::try_from(pid).map_err(|_| VmError::Host(format!("a PID of {pid}")))?;
    // (The lock was held just now: the PID is still its holder's.)
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    );
    wait_for_host_gone(paths, u32::try_from(pid).ok(), deadline)
}

/// Waits for the host `pid` to be gone, by `deadline`: its lock free, or
/// held by another (a host a command started since is a VM this wait
/// wasn't asked about).
fn wait_for_host_gone(paths: &VmPaths, pid: Option<u32>, deadline: Instant) -> Result<(), VmError> {
    let began = Instant::now();
    loop {
        match status(paths)? {
            Status::Stopped => return Ok(()),
            Status::Running(Some(now)) if Some(now) != pid => return Ok(()),
            Status::Running(_) => {}
        }
        if Instant::now() > deadline {
            return Err(VmError::Host(format!(
                "the VM's host (PID {}) still runs after {} s",
                pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
                began.elapsed().as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake host on `paths`' socket: each connection, in turn, answered
    /// with the frames `answers` gives for it, its hello read first as the
    /// guest's agent reads it (a refusal, as a stopping host's, comes
    /// unread).
    fn fake_host(paths: &VmPaths, answers: Vec<Vec<Frame>>) -> std::thread::JoinHandle<()> {
        let listener = std::os::unix::net::UnixListener::bind(paths.socket()).unwrap();
        std::thread::spawn(move || {
            for frames in answers {
                let (mut client, _) = listener.accept().unwrap();
                if !matches!(frames.first(), Some(Frame::Refused(_))) {
                    let _ = Connection::new(client.try_clone().unwrap()).next_frame();
                }
                for frame in frames {
                    forward::send(&mut client, &frame).unwrap();
                }
            }
        })
    }

    fn scratch(name: &str) -> VmPaths {
        let dir = std::path::PathBuf::from(format!("/tmp/sy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = VmPaths::at(dir.clone(), dir.join("guest")).unwrap();
        paths.prepare(nix::unistd::getuid().as_raw()).unwrap();
        paths
    }

    /// Asking never starts the VM: a stopped one's answer is none, and
    /// no host is started; a running one's is its output.
    #[test]
    fn asking_never_starts_the_vm() {
        let paths = scratch("ask");
        assert_eq!(ask(&paths, &["__x"], 1024).unwrap(), None);
        assert!(!paths.host_log().exists(), "a host was started");

        // A host up (its lock held, as a running one's is), answering.
        let lock = std::fs::File::create(paths.lock()).unwrap();
        lock.lock().unwrap();
        let welcome = Frame::Welcome {
            version: shipyard_runtime::attach::VERSION,
            terminal: false,
            stdin: false,
        };
        let host = fake_host(
            &paths,
            vec![vec![
                welcome,
                Frame::Stdout(b"answer".to_vec()),
                Frame::Exited(0),
            ]],
        );
        assert_eq!(
            ask(&paths, &["__x"], 1024).unwrap().as_deref(),
            Some(&b"answer"[..])
        );
        host.join().unwrap();
        // Up, its socket not there yet (booting): no answer, still none
        // started.
        std::fs::remove_file(paths.socket()).unwrap();
        assert_eq!(ask(&paths, &["__x"], 1024).unwrap(), None);
        assert!(!paths.host_log().exists(), "a host was started");
        drop(lock);
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// `vm stop` waits for the host it stopped, not for no host at all:
    /// another command's new host (another PID in the lock) ends the wait;
    /// its own still there doesn't, to the deadline. (No process stands in
    /// for the host: a test that forks hands other tests' sockets to its
    /// child until the exec, AGENTS §10.)
    #[test]
    fn a_stop_waits_for_its_own_host_only() {
        let paths = scratch("stop");
        let lock = std::fs::File::create(paths.lock()).unwrap();
        lock.lock().unwrap();
        // The stopped host's PID, never signalled here.
        let stopped = 999_999_u32;
        std::fs::write(paths.lock(), format!("{stopped}\n")).unwrap();
        let soon = Instant::now() + Duration::from_millis(300);
        assert!(
            wait_for_host_gone(&paths, Some(stopped), soon).is_err(),
            "its own host still runs"
        );
        std::fs::write(paths.lock(), "1\n").unwrap();
        let began = Instant::now();
        wait_for_host_gone(&paths, Some(stopped), began + Duration::from_secs(10)).unwrap();
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "waited for any host"
        );
        drop(lock);
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// A wait for the VM ends at a SIGTERM (143), a SIGINT, a SIGHUP; any
    /// other signal is let pass; no watch, nothing.
    #[test]
    fn a_wait_is_ended_by_a_stopping_signal() {
        assert_eq!(interrupted(None), None);
        // (The only test in this binary that installs the process's watch.)
        let watch = SignalWatch::install().unwrap();
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGUSR1).unwrap();
        assert_eq!(
            interrupted(Some(&watch)),
            None,
            "SIGUSR1 doesn't end a wait"
        );
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGTERM).unwrap();
        assert_eq!(interrupted(Some(&watch)), Some(128 + libc::SIGTERM));
    }

    /// A host that's stopping (idle) refuses a command before it reaches the
    /// guest: it's sent again, once the host is gone, and runs; refused
    /// again, it isn't tried a third time.
    #[test]
    fn a_command_refused_as_the_vm_stops_is_sent_again_once() {
        let stopping = || vec![Frame::Refused(forward::STOPPING.to_owned())];
        let welcome = Frame::Welcome {
            version: shipyard_runtime::attach::VERSION,
            terminal: false,
            stdin: false,
        };
        let request = Request::new(vec!["ps".to_owned()], "/".to_owned(), Vec::new());

        let paths = scratch("retry");
        let host = fake_host(&paths, vec![stopping(), vec![welcome, Frame::Exited(7)]]);
        // (No signal watch: one per process, and this is a test's.)
        let tried = tries(&paths, &request, &mut None);
        assert!(matches!(tried, Ok(Outcome::Exited(7))), "{tried:?}");
        host.join().unwrap();
        std::fs::remove_file(paths.socket()).unwrap();

        let host = fake_host(&paths, vec![stopping(), stopping()]);
        assert!(matches!(
            tries(&paths, &request, &mut None),
            Err(VmError::Refused(why)) if why == forward::STOPPING
        ));
        host.join().unwrap();
        let _ = std::fs::remove_dir_all(&paths.dir);
    }

    /// Three Ctrl-C's let go (130), and so does a SIGTERM's grace from the
    /// first one (143); fewer, or within the grace, are passed on.
    #[test]
    fn the_ways_out_are_a_third_interrupt_and_a_terms_grace() {
        let start = Instant::now();
        let mut leaving = Leaving::default();
        assert_eq!(leaving.on(libc::SIGINT, start), None);
        assert_eq!(leaving.on(libc::SIGINT, start), None);
        assert_eq!(leaving.on(libc::SIGINT, start), Some(Way::Interrupted));
        assert_eq!(Way::Interrupted.code(), 130);

        let mut leaving = Leaving::default();
        assert_eq!(leaving.over(start + TERM_GRACE * 2), None, "no SIGTERM yet");
        assert_eq!(leaving.on(libc::SIGTERM, start), None);
        // A second one, in the grace, is only passed on: the grace runs
        // from the first.
        let later = start + TERM_GRACE / 2;
        assert_eq!(leaving.on(libc::SIGTERM, later), None);
        assert_eq!(leaving.over(later), None);
        let end = start + TERM_GRACE;
        let just_before = start + TERM_GRACE.checked_sub(Duration::from_millis(1)).unwrap();
        assert_eq!(leaving.over(just_before), None);
        assert_eq!(leaving.over(end), Some(Way::Unterminated));
        assert_eq!(Way::Unterminated.code(), 143);
    }

    /// The wire's numbers are Linux's: the Mac's that differ (USR1, USR2)
    /// are translated, the same stay, and one without a Linux twin (INFO,
    /// Ctrl-T's) isn't passed on.
    #[test]
    fn signals_go_by_their_linux_numbers() {
        assert_eq!(linux_number(libc::SIGINT), Some(2));
        assert_eq!(linux_number(libc::SIGTERM), Some(15));
        assert_eq!(linux_number(libc::SIGUSR1), Some(10));
        assert_eq!(linux_number(libc::SIGUSR2), Some(12));
        assert_eq!(linux_number(libc::SIGINFO), None);
        assert_eq!(linux_number(libc::SIGKILL), None);
    }
}

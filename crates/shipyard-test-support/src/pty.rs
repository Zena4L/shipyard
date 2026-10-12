//! A fake interactive shell on a pseudo-terminal, for end-to-end terminal
//! tests (TLPI ch. 34 and 64).
//!
//! A pseudo-terminal is a pair of descriptors: whatever is written to the
//! *master* looks to programs on the *slave* like typing on a real terminal,
//! and their output comes back out of the master. Ctrl-C typed into the
//! master makes the kernel send SIGINT to the terminal's foreground process
//! group, exactly as with a real terminal.
//!
//! [`Session::start`] forks a process that plays the shell: it starts a new
//! session, makes the slave its controlling terminal, and then runs a script
//! (a [`Shell`] function) that starts shipyard as a job and does job control
//! on it the way bash does: foreground and background process groups,
//! `tcsetpgrp`, `waitpid(WUNTRACED)`, `fg`, `bg`. The test, in the original
//! process, types into the master, reads the output, and receives the
//! shell's observations (job PIDs, exit or stop statuses) over a pipe.
//!
//! The shell is forked from the test process and keeps running Rust code, so
//! the test process must be single-threaded (see `shipyard-cli`'s
//! `tests/terminal.rs`).

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::{Duration, Instant};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::pty::{Winsize, openpty};

/// How long to wait for any one expected thing before failing.
const DEADLINE: Duration = Duration::from_secs(20);
/// The `TERM` the shell gives shipyard, so tests don't depend on the
/// developer's own terminal.
pub const TERM: &str = "xterm-shipyard-test";

/// Set by the shell's SIGHUP handler: the terminal hung up.
static HUNG_UP: AtomicBool = AtomicBool::new(false);

/// What the timeout alarm must kill if a test hangs (see
/// [`kill_everything_on_alarm`]): the current session's shell and its job
/// groups. 0 means an empty slot.
static ALARM_SHELL: AtomicI32 = AtomicI32::new(0);
static ALARM_GROUPS: [AtomicI32; 4] = [const { AtomicI32::new(0) }; 4];

/// Makes SIGALRM kill the current session's shell and jobs, then this
/// process. `Drop` doesn't run when a signal kills a process, so without
/// this a hung test would leave shipyard and its command running.
pub fn kill_everything_on_alarm() {
    extern "C" fn on_alarm(_signal: libc::c_int) {
        const MESSAGE: &[u8] = b"\ntest timed out; killed its processes\n";
        // SAFETY: only async-signal-safe calls: `killpg`, `kill`, `write`
        // and `_exit`, on values read from atomics.
        unsafe {
            for group in &ALARM_GROUPS {
                let group = group.load(Ordering::Acquire);
                if group > 0 {
                    libc::killpg(group, libc::SIGKILL);
                }
            }
            let shell = ALARM_SHELL.load(Ordering::Acquire);
            if shell > 0 {
                libc::kill(shell, libc::SIGKILL);
            }
            libc::write(2, MESSAGE.as_ptr().cast(), MESSAGE.len());
            libc::_exit(1);
        }
    }
    let handler: extern "C" fn(libc::c_int) = on_alarm;
    // SAFETY: the handler only makes async-signal-safe calls.
    unsafe { libc::signal(libc::SIGALRM, handler as libc::sighandler_t) };
}

/// How a job ended or paused, as the shell's `waitpid` saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Exited with this code.
    Exited(i32),
    /// Killed by this signal.
    Signaled(i32),
    /// Stopped by this signal.
    Stopped(i32),
}

impl Status {
    /// Decodes a `waitpid(WUNTRACED)` status: without WCONTINUED, anything
    /// that isn't an exit or a death is a stop.
    fn from_wait_status(raw: libc::c_int) -> Self {
        if libc::WIFEXITED(raw) {
            Self::Exited(libc::WEXITSTATUS(raw))
        } else if libc::WIFSIGNALED(raw) {
            Self::Signaled(libc::WTERMSIG(raw))
        } else {
            Self::Stopped(libc::WSTOPSIG(raw))
        }
    }

    fn to_event(self) -> String {
        match self {
            Self::Exited(code) => format!("status exited {code}"),
            Self::Signaled(signal) => format!("status signaled {signal}"),
            Self::Stopped(signal) => format!("status stopped {signal}"),
        }
    }

    fn from_event(event: &str) -> Option<Self> {
        let mut words = event.split(' ');
        let (Some("status"), Some(kind), Some(number)) = (words.next(), words.next(), words.next())
        else {
            return None;
        };
        let number = number.parse().ok()?;
        match kind {
            "exited" => Some(Self::Exited(number)),
            "signaled" => Some(Self::Signaled(number)),
            "stopped" => Some(Self::Stopped(number)),
            _ => None,
        }
    }
}

/// Whether the shell gives a new job the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The job gets the terminal.
    Foreground,
    /// The shell keeps the terminal.
    Background,
}

/// A job the shell started: shipyard, leading its own process group.
#[derive(Debug, Clone, Copy)]
pub struct Job {
    pid: libc::pid_t,
}

/// The shell's side: runs in the forked shell process.
#[derive(Debug)]
pub struct Shell {
    events: File,
    control: File,
    /// The shipyard binary that [`Shell::spawn`] starts.
    shipyard: &'static str,
}

// The job-control methods take `&mut self` on purpose: they're the shell's
// actions, and only the shell process may run them.
#[allow(clippy::unused_self, reason = "only the shell runs these")]
impl Shell {
    /// Starts `shipyard ARGS…` as a job in its own process group, like a
    /// shell with job control does. In the foreground, the job gets the
    /// terminal.
    pub fn spawn(&mut self, mode: Mode, args: &[&str]) -> Job {
        self.spawn_program(mode, self.shipyard, &crate::for_this_kernel(args))
    }

    /// Like [`Shell::spawn`], for any program.
    pub fn spawn_program(&mut self, mode: Mode, program: &str, args: &[&str]) -> Job {
        let foreground = mode == Mode::Foreground;
        let mut command = Command::new(program);
        command.args(args).env_clear().env("TERM", TERM);
        // SAFETY: only async-signal-safe calls, as `pre_exec` requires.
        unsafe {
            command.pre_exec(move || {
                // What bash does in a new job: its own group, the default
                // job-control signals (the shell ignores them), and the
                // terminal if it's a foreground job. Both the shell and the
                // job set the group and the terminal, so the order in which
                // they run doesn't matter.
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                for signal in [
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTSTP,
                    libc::SIGTTIN,
                    libc::SIGTTOU,
                    libc::SIGHUP,
                ] {
                    libc::signal(signal, libc::SIG_DFL);
                }
                if foreground {
                    give_terminal_to(libc::getpid());
                }
                Ok(())
            });
        }
        // Reaped by `wait`, with `waitpid(WUNTRACED)`: std's `Child::wait`
        // can't report stops.
        #[allow(clippy::zombie_processes, reason = "reaped by `Shell::wait`")]
        let child = command.spawn().expect("starting shipyard");
        let pid = libc::pid_t::try_from(child.id()).expect("pid fits");
        // SAFETY: `setpgid` only changes the job's process group.
        unsafe { libc::setpgid(pid, pid) };
        if foreground {
            give_terminal_to(pid);
        }
        self.report(&format!("job {pid}"));
        Job { pid }
    }

    /// Waits until the job exits, is killed, or stops, then takes the
    /// terminal back and reports what happened.
    pub fn wait(&mut self, job: Job) -> Status {
        let status = self.wait_for(job);
        // SAFETY: `getpgrp` has no preconditions.
        give_terminal_to(unsafe { libc::getpgrp() });
        self.report(&status.to_event());
        status
    }

    /// Like [`Shell::wait`], but leaves the terminal with the job's group,
    /// which stays the foreground group after its leader exits.
    pub fn wait_leaving_terminal(&mut self, job: Job) -> Status {
        let status = self.wait_for(job);
        self.report(&status.to_event());
        status
    }

    /// `waitpid(WUNTRACED)`, polled with WNOHANG so a hangup can't slip in
    /// between a check and the wait: on hangup it does what bash does and
    /// sends the job SIGHUP (and SIGCONT, in case it's stopped).
    fn wait_for(&mut self, job: Job) -> Status {
        loop {
            if HUNG_UP.swap(false, Ordering::AcqRel) {
                // SAFETY: `killpg` only sends signals to the job's group.
                unsafe {
                    libc::killpg(job.pid, libc::SIGHUP);
                    libc::killpg(job.pid, libc::SIGCONT);
                }
            }
            let mut raw = 0;
            // SAFETY: `raw` is valid storage; `job.pid` is our child.
            let rc =
                unsafe { libc::waitpid(job.pid, &raw mut raw, libc::WUNTRACED | libc::WNOHANG) };
            match rc {
                0 => std::thread::sleep(Duration::from_millis(5)),
                -1 => {
                    let err = std::io::Error::last_os_error();
                    assert_eq!(err.kind(), ErrorKind::Interrupted, "waitpid: {err}");
                }
                _ => return Status::from_wait_status(raw),
            }
        }
    }

    /// `fg`: gives the job the terminal and continues its whole group.
    pub fn fg(&mut self, job: Job) {
        give_terminal_to(job.pid);
        // SAFETY: `killpg` only sends a signal to the job's group.
        unsafe { libc::killpg(job.pid, libc::SIGCONT) };
    }

    /// `bg`: continues the job's group without giving it the terminal.
    pub fn bg(&mut self, job: Job) {
        // SAFETY: as above.
        unsafe { libc::killpg(job.pid, libc::SIGCONT) };
    }

    /// Like `fg`, but continues only shipyard, not the rest of its group.
    pub fn fg_shipyard_only(&mut self, job: Job) {
        give_terminal_to(job.pid);
        // SAFETY: `kill` only sends a signal to the job's leader.
        unsafe { libc::kill(job.pid, libc::SIGCONT) };
    }

    /// Blocks until the test calls [`Session::go`].
    pub fn await_go(&mut self) {
        let mut byte = [0u8; 1];
        self.control
            .read_exact(&mut byte)
            .expect("the test went away");
    }

    /// The terminal's settings now (what `stty -g` prints), to compare
    /// with [`report_settings`](Self::report_settings) after a job.
    #[must_use]
    pub fn terminal_settings(&self) -> libc::termios {
        // Zeroed: the C library's `termios` can have more control characters
        // than the kernel fills in (musl's), which would differ from one
        // call to the next.
        let mut settings = std::mem::MaybeUninit::<libc::termios>::zeroed();
        // SAFETY: `tcgetattr` fills in the `termios` it's given; it's read
        // only once the call succeeded.
        let rc = unsafe { libc::tcgetattr(0, settings.as_mut_ptr()) };
        assert_eq!(rc, 0, "tcgetattr: {}", std::io::Error::last_os_error());
        // SAFETY: filled in by the successful call.
        unsafe { settings.assume_init() }
    }

    /// Writes "settings unchanged" on the terminal if its settings are still
    /// `before`, "settings changed" if not.
    pub fn report_settings(&self, before: &libc::termios) {
        let now = self.terminal_settings();
        let same = (now.c_iflag, now.c_oflag, now.c_cflag, now.c_lflag, now.c_cc)
            == (
                before.c_iflag,
                before.c_oflag,
                before.c_cflag,
                before.c_lflag,
                before.c_cc,
            );
        say(if same {
            "settings unchanged\n"
        } else {
            "settings changed\n"
        });
    }

    /// Writes "terminal blocking" on the terminal if its open file (the
    /// shell's, which the jobs share) has no `O_NONBLOCK`, "terminal
    /// NON-BLOCKING" if it has.
    pub fn report_blocking(&self) {
        // SAFETY: `fcntl(F_GETFL)` only reads the open file's flags.
        let flags = unsafe { libc::fcntl(0, libc::F_GETFL) };
        say(if flags != -1 && flags & libc::O_NONBLOCK == 0 {
            "terminal blocking\n"
        } else {
            "terminal NON-BLOCKING\n"
        });
    }

    /// Tells the test something. If the test is gone, there's no one to
    /// tell; the shell must not panic over it.
    fn report(&mut self, event: &str) {
        let _ = writeln!(self.events, "{event}");
    }
}

/// Makes `pgid` the terminal's foreground group. SIGTTOU is blocked around
/// the call: a process outside the foreground group that changes it would
/// otherwise be stopped by it. Async-signal-safe.
///
/// Best effort: after a hangup there's no terminal left to hand over, and
/// that's fine.
fn give_terminal_to(pgid: libc::pid_t) {
    // SAFETY: plain signal-set and terminal calls on valid storage.
    unsafe {
        let mut ttou: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut ttou);
        libc::sigaddset(&raw mut ttou, libc::SIGTTOU);
        libc::sigprocmask(libc::SIG_BLOCK, &raw const ttou, &raw mut old);
        libc::tcsetpgrp(0, pgid);
        libc::sigprocmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut());
    }
}

/// The test's side: a running shell on a pseudo-terminal.
#[derive(Debug)]
pub struct Session {
    /// The terminal's master side. `None` after [`Session::hang_up`], or
    /// once nothing has the terminal open any more.
    master: Option<File>,
    /// `None` once the shell has closed it (it exited).
    events: Option<File>,
    control: File,
    /// The shell's PID; 0 once it has been reaped.
    shell: libc::pid_t,
    /// The jobs' process groups. [`Session::finish`] checks they're empty;
    /// if the test fails before that, `Drop` kills what's left.
    jobs: Vec<libc::pid_t>,
    output: Vec<u8>,
    /// Where the next [`Session::expect`] starts looking.
    cursor: usize,
    pending_events: Vec<u8>,
}

impl Session {
    /// Starts a shell on a new pseudo-terminal, running `script`; its
    /// [`Shell::spawn`] starts the shipyard binary at `shipyard`.
    ///
    /// # Safety
    ///
    /// The calling process must be single-threaded. The shell is forked and
    /// keeps running ordinary Rust code (it allocates, uses `Command`), which
    /// is only sound when no other thread could hold a lock at the fork.
    pub unsafe fn start(shipyard: &'static str, script: fn(&mut Shell)) -> Self {
        let size = Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = openpty(Some(&size), None).expect("opening a pseudo-terminal");
        // `openpty` doesn't set close-on-exec: without it, every process the
        // test or the shell starts would hold the terminal open.
        set_cloexec(&pty.master);
        set_cloexec(&pty.slave);
        let (events_read, events_write) = cloexec_pipe();
        let (control_read, control_write) = cloexec_pipe();
        // SAFETY: the test process is single-threaded (the caller's promise), so
        // the child may run any code the parent could.
        match unsafe { libc::fork() } {
            -1 => panic!("fork: {}", std::io::Error::last_os_error()),
            0 => {
                drop(pty.master);
                drop(events_read);
                drop(control_write);
                // Nothing may unwind out of here: it would land in the test
                // runner's code, and this copy would run the other tests.
                let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    run_shell(pty.slave, events_write, control_read, shipyard, script)
                }))
                .unwrap_or(1);
                // SAFETY: `_exit` ends the forked shell without running the
                // test process's exit path a second time.
                unsafe { libc::_exit(code) }
            }
            shell => {
                drop(pty.slave);
                drop(events_write);
                drop(control_read);
                ALARM_SHELL.store(shell, Ordering::Release);
                Self {
                    master: Some(File::from(pty.master)),
                    events: Some(File::from(events_read)),
                    control: File::from(control_write),
                    shell,
                    jobs: Vec::new(),
                    output: Vec::new(),
                    cursor: 0,
                    pending_events: Vec::new(),
                }
            }
        }
    }

    /// Types `keys` into the terminal: `b"\x03"` is Ctrl-C, `b"\x1c"`
    /// Ctrl-\, `b"\x1a"` Ctrl-Z.
    pub fn type_keys(&mut self, keys: &[u8]) {
        let mut master = self.master.as_ref().expect("the terminal is still open");
        master.write_all(keys).expect("typing into the terminal");
    }

    /// Changes the terminal's size, like resizing its window: the kernel
    /// then sends SIGWINCH to the terminal's foreground process group.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let master = self.master.as_ref().expect("the terminal is still open");
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `TIOCSWINSZ` reads one `winsize`, which `size` is.
        let rc = unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &raw const size) };
        assert_eq!(rc, 0, "resizing: {}", std::io::Error::last_os_error());
    }

    /// Waits until `text` appears in the output after the previous match.
    pub fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(at) = find(&self.output[self.cursor..], text.as_bytes()) {
                self.cursor += at + text.len();
                return;
            }
            assert!(
                Instant::now() < deadline,
                "never saw {text:?}\n{}",
                self.transcript()
            );
            self.pump(deadline);
        }
    }

    /// Waits for a line starting with `prefix`; returns the rest of it.
    pub fn expect_line_starting(&mut self, prefix: &str) -> String {
        self.expect(prefix);
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(end) = find(&self.output[self.cursor..], b"\n") {
                let rest = &self.output[self.cursor..self.cursor + end];
                let rest = String::from_utf8_lossy(rest).trim_end().to_owned();
                self.cursor += end + 1;
                return rest;
            }
            assert!(
                Instant::now() < deadline,
                "no end of line after {prefix:?}\n{}",
                self.transcript()
            );
            self.pump(deadline);
        }
    }

    /// Reads whatever arrives for `how_long`: for asserting what did *not*
    /// arrive (with [`count`](Self::count)).
    pub fn settle(&mut self, how_long: Duration) {
        let deadline = Instant::now() + how_long;
        while Instant::now() < deadline {
            self.pump(deadline);
        }
    }

    /// How often `text` appears in all the output so far.
    #[must_use]
    pub fn count(&self, text: &str) -> usize {
        String::from_utf8_lossy(&self.output).matches(text).count()
    }

    /// The PID of the next job the shell started.
    pub fn job(&mut self) -> libc::pid_t {
        let event = self.event();
        let pid = event
            .strip_prefix("job ")
            .and_then(|pid| pid.parse().ok())
            .unwrap_or_else(|| panic!("expected a job, got {event:?}\n{}", self.transcript()));
        self.jobs.push(pid);
        if let Some(slot) = ALARM_GROUPS
            .iter()
            .find(|slot| slot.load(Ordering::Acquire) == 0)
        {
            slot.store(pid, Ordering::Release);
        }
        pid
    }

    /// The next status the shell reported.
    pub fn status(&mut self) -> Status {
        let event = self.event();
        Status::from_event(&event)
            .unwrap_or_else(|| panic!("expected a status, got {event:?}\n{}", self.transcript()))
    }

    /// Lets the shell's script continue past [`Shell::await_go`].
    pub fn go(&mut self) {
        self.control.write_all(b"g").expect("signalling the shell");
    }

    /// Closes the terminal, like closing a terminal window.
    pub fn hang_up(&mut self) {
        self.master = None;
    }

    /// Waits for the shell to finish its script and checks that it did,
    /// then checks that the jobs left nothing running behind them.
    ///
    /// On macOS, with a terminal, shipyard can't clean up what the command
    /// leaves running (documented in docs/user/run.md), so test scripts must
    /// stop their own background processes.
    pub fn finish(mut self) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let mut raw = 0;
            // SAFETY: `raw` is valid storage; `self.shell` is our child.
            match unsafe { libc::waitpid(self.shell, &raw mut raw, libc::WNOHANG) } {
                0 => {
                    assert!(
                        Instant::now() < deadline,
                        "the shell didn't finish\n{}",
                        self.transcript()
                    );
                    self.pump(Instant::now() + Duration::from_millis(10));
                }
                pid if pid == self.shell => {
                    self.shell = 0;
                    assert!(
                        libc::WIFEXITED(raw) && libc::WEXITSTATUS(raw) == 0,
                        "the shell's script failed (wait status {raw:#x})\n{}",
                        self.transcript()
                    );
                    break;
                }
                _ => panic!("waitpid: {}", std::io::Error::last_os_error()),
            }
        }
        for group in std::mem::take(&mut self.jobs) {
            // Signal 0 only checks: ESRCH means the group is empty. Processes
            // still being cleaned up get a moment to go.
            // SAFETY: `killpg` with signal 0 sends nothing.
            while unsafe { libc::killpg(group, 0) } == 0 {
                if Instant::now() >= deadline {
                    // Back on the list, so `Drop` kills them.
                    self.jobs.push(group);
                    panic!(
                        "job group {group} left processes behind\n{}",
                        self.transcript()
                    );
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    /// Everything the terminal showed, for failure messages.
    #[must_use]
    pub fn transcript(&self) -> String {
        format!(
            "--- terminal output ---\n{}\n-----------------------",
            String::from_utf8_lossy(&self.output)
        )
    }

    /// The next line from the shell, reading terminal output meanwhile (a
    /// full terminal buffer would block the job's writes).
    fn event(&mut self) -> String {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(end) = find(&self.pending_events, b"\n") {
                let line: Vec<u8> = self.pending_events.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line).trim_end().to_owned();
                if let Some(problem) = line.strip_prefix("failed: ") {
                    panic!("the shell failed: {problem}\n{}", self.transcript());
                }
                return line;
            }
            assert!(
                self.events.is_some(),
                "the shell exited without reporting (see its output)\n{}",
                self.transcript()
            );
            assert!(
                Instant::now() < deadline,
                "no event from the shell\n{}",
                self.transcript()
            );
            self.pump(deadline);
        }
    }

    /// Waits (until `deadline` at most) for the terminal or the event pipe
    /// to have something, and reads it.
    fn pump(&mut self, deadline: Instant) {
        let left = deadline.saturating_duration_since(Instant::now());
        // Rounded up: rounding down would turn the last fraction of a
        // millisecond into busy polling.
        let millis = left.as_nanos().div_ceil(1_000_000);
        let timeout = PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX);
        let watched = [self.events.as_ref(), self.master.as_ref()];
        let mut fds: Vec<PollFd> = watched
            .iter()
            .flatten()
            .map(|file| PollFd::new(file.as_fd(), PollFlags::POLLIN))
            .collect();
        if fds.is_empty() {
            std::thread::sleep(left.min(Duration::from_millis(10)));
            return;
        }
        if poll(&mut fds, timeout).unwrap_or(0) == 0 {
            return;
        }
        let mut ready = fds
            .iter()
            .map(|fd| fd.revents().is_some_and(|r| !r.is_empty()));
        let events_ready = self.events.is_some() && ready.next() == Some(true);
        let master_ready = self.master.is_some() && ready.next() == Some(true);
        drop(fds);
        // At end of input, stop watching: the shell exited, or nothing has
        // the terminal open any more.
        if events_ready
            && let Some(events) = &self.events
            && !read_some(events, &mut self.pending_events)
        {
            self.events = None;
        }
        if master_ready
            && let Some(master) = &self.master
            && !read_some(master, &mut self.output)
        {
            self.master = None;
        }
    }
}

/// If a test fails half-way, nothing it started may outlive it. (After a
/// successful [`Session::finish`] there's nothing left to kill, and killing
/// an emptied group could hit an unrelated one that reused its number.)
impl Drop for Session {
    fn drop(&mut self) {
        ALARM_SHELL.store(0, Ordering::Release);
        for slot in &ALARM_GROUPS {
            slot.store(0, Ordering::Release);
        }
        for &group in &self.jobs {
            // SAFETY: these are process groups this session's shell created,
            // not yet seen empty.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
        if self.shell == 0 {
            return;
        }
        // SAFETY: `self.shell` is our unreaped child.
        unsafe { libc::kill(self.shell, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let mut raw = 0;
            // SAFETY: as above; WNOHANG never blocks.
            if unsafe { libc::waitpid(self.shell, &raw mut raw, libc::WNOHANG) } != 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// The shell process: sets itself up as an interactive shell on the
/// terminal, then runs `script`. Returns its exit code; failures are also
/// reported to the test, which shows them.
fn run_shell(
    slave: OwnedFd,
    events: OwnedFd,
    control: OwnedFd,
    shipyard: &'static str,
    script: fn(&mut Shell),
) -> i32 {
    let mut shell = Shell {
        events: File::from(events),
        control: File::from(control),
        shipyard,
    };
    let became = become_interactive_shell(&slave);
    // Now stdin, stdout and stderr; the original isn't needed.
    drop(slave);
    if let Err(err) = became {
        shell.report(&format!("failed: {err}"));
        return 1;
    }
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| script(&mut shell)));
    if ran.is_err() {
        shell.report("failed: the script panicked (its message is in the output)");
        return 1;
    }
    0
}

/// Becomes a session leader whose controlling terminal is `slave`, with it
/// as stdin, stdout and stderr, and the signal handling of an interactive
/// shell.
fn become_interactive_shell(slave: &OwnedFd) -> Result<(), String> {
    let fail = |what: &str| format!("{what}: {}", std::io::Error::last_os_error());
    // SAFETY: plain process and descriptor calls on descriptors we own.
    unsafe {
        if libc::setsid() == -1 {
            return Err(fail("setsid"));
        }
        // Explicitly, on both systems: becoming a session leader alone
        // doesn't attach a terminal. (The request's C type differs between
        // platforms, and libc declares macOS's constant narrower than its
        // `ioctl` takes.)
        #[allow(clippy::useless_conversion, reason = "a no-op on Linux")]
        if libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY.into(), 0) == -1 {
            return Err(fail("making the terminal the controlling terminal"));
        }
        for fd in 0..=2 {
            if libc::dup2(slave.as_raw_fd(), fd) == -1 {
                return Err(fail("dup2"));
            }
        }
        // An interactive shell ignores these, so typing Ctrl-C or Ctrl-Z
        // affects only the foreground job.
        for signal in [
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
        ] {
            libc::signal(signal, libc::SIG_IGN);
        }
        // `Shell::wait` polls the flag the handler sets.
        let handler: extern "C" fn(libc::c_int) = on_hangup;
        libc::signal(libc::SIGHUP, handler as libc::sighandler_t);
        // Last resort: never outlive a stuck test by much.
        libc::alarm(60);
    }
    Ok(())
}

extern "C" fn on_hangup(_signal: libc::c_int) {
    HUNG_UP.store(true, Ordering::Release);
}

/// The current state letters of a process, from `ps` (`T` means stopped).
#[must_use]
pub fn process_state(pid: libc::pid_t) -> String {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("running ps");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Waits until process `pid` is stopped.
pub fn wait_until_stopped(pid: libc::pid_t) {
    let deadline = Instant::now() + DEADLINE;
    while !process_state(pid).starts_with('T') {
        assert!(
            Instant::now() < deadline,
            "process {pid} never stopped (state {:?})",
            process_state(pid)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A pipe whose ends close on exec, so shipyard and the commands don't
/// inherit them (and keep them open after the shell is gone).
fn cloexec_pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` has room for two descriptors.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
    // SAFETY: `pipe` just returned these descriptors, and nothing else owns
    // them.
    let ends = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(&ends.0);
    set_cloexec(&ends.1);
    ends
}

/// Marks `fd` close-on-exec.
fn set_cloexec(fd: &OwnedFd) {
    // SAFETY: F_SETFD only changes the descriptor's flags.
    let rc = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    assert_ne!(rc, -1, "marking a descriptor close-on-exec");
}

/// Reads what's available into `into`. Returns `false` at the end of
/// input: end-of-file, or (Linux) EIO from a terminal nobody has open.
fn read_some(mut file: &File, into: &mut Vec<u8>) -> bool {
    let mut buffer = [0u8; 4096];
    match file.read(&mut buffer) {
        Ok(0) => false,
        Ok(n) => {
            into.extend_from_slice(&buffer[..n]);
            true
        }
        // An interrupted read, or a spurious wakeup: nothing yet.
        Err(err) => matches!(err.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock),
    }
}

/// Where `needle` first occurs in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Writes `text` on the shell's terminal, in one call (no buffering in this
/// forked process).
fn say(text: &str) {
    // SAFETY: `write` reads `text` for its length; fd 1 is the terminal.
    unsafe { libc::write(1, text.as_ptr().cast(), text.len()) };
}

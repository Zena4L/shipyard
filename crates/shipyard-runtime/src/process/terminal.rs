//! Terminals: window sizes, passing a pseudo-terminal's master between
//! processes, and the user's terminal in raw mode (stage 8, step 8.2b; TLPI
//! ch. 62 and 64).
//!
//! **Raw mode.** With `-it`, Shipyard's own terminal must pass every key on
//! as a byte (Ctrl-C included: the container's terminal makes it a signal
//! for the container's foreground job), so its line discipline is turned off
//! (`cfmakeraw`). That changes the user's terminal, which their shell uses
//! once Shipyard is done, so it's put back on every way out Shipyard sees:
//! the end of the run, a stop (Ctrl-Z can't reach Shipyard in raw mode, but
//! `kill -TSTP` can, and job control stops it to read or write the terminal
//! from the background), and a panic. Not `kill -9`: nothing can.
//!
//! **Restoring from a signal handler.** A stop leaves the terminal to the
//! shell, so it must be put back before the stop, and raw again once
//! resumed: the shell's way, which `vim` follows too. That's done in a
//! SIGTSTP handler, with `tcsetattr` (async-signal-safe), from settings kept
//! in a static. Restores use `TCSANOW`: `TCSADRAIN` would wait for output a
//! stalled terminal never sends.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use nix::errno::Errno;

/// A terminal's size: rows, columns.
pub(crate) type Size = (u16, u16);

/// The size a pseudo-terminal gets when there's no terminal to copy it
/// from (`-t` with output to a pipe): a new one would be 0x0 (probe T7).
pub(crate) const DEFAULT_SIZE: Size = (24, 80);

/// The size of the terminal on `fd`, if it's one.
pub(crate) fn window_size(fd: RawFd) -> Option<Size> {
    let mut size = MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: `TIOCGWINSZ` fills in the `winsize` it's given; it's read only
    // if the call succeeded.
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: filled in by the successful call.
    let size = unsafe { size.assume_init() };
    (size.ws_row > 0 && size.ws_col > 0).then_some((size.ws_row, size.ws_col))
}

/// Sets the size of the terminal on `fd` (a pseudo-terminal's master: the
/// kernel then sends SIGWINCH to its foreground job, probe T7).
pub(crate) fn set_window_size(fd: RawFd, (rows, cols): Size) -> Result<(), Errno> {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `TIOCSWINSZ` reads one `winsize`, which `size` is.
    Errno::result(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &raw const size) }).map(drop)
}

/// The size of Shipyard's own terminal (its output's, else its input's or
/// its error's), if it has one.
pub(crate) fn terminal_size() -> Option<Size> {
    [libc::STDOUT_FILENO, libc::STDIN_FILENO, libc::STDERR_FILENO]
        .into_iter()
        .find_map(window_size)
}

/// Whether Shipyard's process group is the foreground group of the
/// terminal on `fd`. One that can't say (hung up) counts as foreground:
/// reading or writing it then fails, which ends what was waiting for it.
pub(crate) fn in_foreground(fd: RawFd) -> bool {
    // SAFETY: `tcgetpgrp` only reads the terminal's state.
    let foreground = unsafe { libc::tcgetpgrp(fd) };
    foreground == -1 || foreground == nix::unistd::getpgrp().as_raw()
}

/// Whether Shipyard's own terminal has hung up (closed: a window, an SSH
/// connection). The kernel then sends SIGHUP to the terminal's session
/// leader only (the user's shell), which passes it on to its jobs, Shipyard
/// among them (TLPI §34.6): the signal's sender can't tell a hang-up from a
/// `kill -HUP`, but the terminal can. On Linux a hung-up terminal's open
/// files fail every request with EIO; on macOS it still answers, but `poll`
/// reports the hang-up. Only terminals count: a pipe whose writer is gone
/// reports POLLHUP too.
pub(crate) fn host_hung_up() -> bool {
    [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO]
        .into_iter()
        .any(|fd| {
            let mut t = MaybeUninit::<libc::termios>::uninit();
            // SAFETY: `tcgetattr` fills in the `termios` it's given (unread
            // here).
            if unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) } != 0 {
                return matches!(Errno::last(), Errno::EIO | Errno::ENXIO);
            }
            let mut poll_fd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid `pollfd`, not waiting.
            let ready = unsafe { libc::poll(&raw mut poll_fd, 1, 0) };
            ready > 0 && poll_fd.revents & libc::POLLHUP != 0
        })
}

/// What a pseudo-terminal's master is sent with: a name, as runc sends one
/// on a console socket (`SendRawFd(socket, pty.Name(), …)`: the path it
/// opened, `/dev/pts/ptmx` in runc 1.5), which a receiver may use as the
/// file's name and otherwise ignores.
#[cfg(any(target_os = "linux", test))]
pub(crate) const MASTER_NAME: &[u8] = b"/dev/ptmx";

/// The most a descriptor's name may take ([`receive_fd`] reads it; runc's
/// `MaxNameLen`).
const MAX_NAME_LEN: usize = 4096;

/// Sends `fd` over the UNIX socket `socket` (`SCM_RIGHTS`, TLPI §61.13.3),
/// with `name` as the message's bytes: the receiver gets a descriptor of its
/// own for the same open file, across namespaces (probe T6).
#[cfg(any(target_os = "linux", test))]
pub(crate) fn send_fd(
    socket: BorrowedFd<'_>,
    fd: BorrowedFd<'_>,
    name: &[u8],
) -> Result<(), Errno> {
    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
    let fds = [fd.as_raw_fd()];
    let iov = [std::io::IoSlice::new(name)];
    loop {
        match sendmsg::<()>(
            socket.as_raw_fd(),
            &iov,
            &[ControlMessage::ScmRights(&fds)],
            MsgFlags::empty(),
            None,
        ) {
            Ok(_) => return Ok(()),
            Err(Errno::EINTR) => {}
            Err(errno) => return Err(errno),
        }
    }
}

/// Receives a descriptor [`send_fd`] sent, without blocking. `Ok(None)`:
/// nothing yet. `Err(Errno::ENODATA)`: the other end closed without sending
/// (it failed). The descriptor is close-on-exec; any more than one, or a
/// truncated message, is refused (and what came is closed). Its name is
/// read with it, and not used.
pub(crate) fn receive_fd(socket: BorrowedFd<'_>) -> Result<Option<OwnedFd>, Errno> {
    use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
    let mut name = [0_u8; MAX_NAME_LEN];
    let mut iov = [std::io::IoSliceMut::new(&mut name)];
    let mut space = nix::cmsg_space!([RawFd; 2]);
    #[cfg(target_os = "linux")]
    let flags = MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_CMSG_CLOEXEC;
    // (Elsewhere, close-on-exec is set right after: Shipyard forks from one
    // thread only.)
    #[cfg(not(target_os = "linux"))]
    let flags = MsgFlags::MSG_DONTWAIT;
    let message = match recvmsg::<()>(socket.as_raw_fd(), &mut iov, Some(&mut space), flags) {
        Ok(message) => message,
        Err(Errno::EAGAIN | Errno::EINTR) => return Ok(None),
        Err(errno) => return Err(errno),
    };
    let mut received: Vec<OwnedFd> = Vec::new();
    for cmsg in message.cmsgs()? {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            // SAFETY: the kernel just installed these descriptors in this
            // process; nothing else owns them.
            received.extend(
                fds.into_iter()
                    .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
            );
        }
    }
    if message.bytes == 0 && received.is_empty() {
        return Err(Errno::ENODATA);
    }
    let truncated = message.flags.contains(MsgFlags::MSG_CTRUNC);
    if truncated || received.len() != 1 {
        // Dropped, so closed.
        return Err(Errno::EBADMSG);
    }
    #[cfg(not(target_os = "linux"))]
    for fd in &received {
        cloexec(fd)?;
    }
    Ok(received.pop())
}

/// A connected pair of UNIX stream sockets, both close-on-exec.
pub(crate) fn socket_pair() -> Result<(OwnedFd, OwnedFd), Errno> {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
    #[cfg(target_os = "linux")]
    let flags = SockFlag::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = SockFlag::empty();
    let (a, b) = socketpair(AddressFamily::Unix, SockType::Stream, None, flags)?;
    #[cfg(not(target_os = "linux"))]
    for fd in [&a, &b] {
        cloexec(fd)?;
    }
    Ok((a, b))
}

fn cloexec(fd: &OwnedFd) -> Result<(), Errno> {
    nix::fcntl::fcntl(
        fd,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )
    .map(drop)
}

/// A pseudo-terminal of Shipyard's own (no isolation: the command runs on
/// the host), `size` big: (master, slave), both close-on-exec.
pub(crate) fn host_pty(size: Size) -> Result<(OwnedFd, OwnedFd), Errno> {
    let winsize = nix::pty::Winsize {
        ws_row: size.0,
        ws_col: size.1,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&winsize), None)?;
    for fd in [&pty.master, &pty.slave] {
        cloexec(fd)?;
    }
    Ok((pty.master, pty.slave))
}

// ---- Raw mode ----

/// The settings to put back: the terminal's own before raw mode.
struct Saved(UnsafeCell<MaybeUninit<libc::termios>>);

// SAFETY: written only by the one `RawMode` (see `ACTIVE`) and by its
// SIGTSTP handler, which runs on Shipyard's single thread of control,
// interrupting it: never two at once. Read the same way.
unsafe impl Sync for Saved {}

static SAVED: Saved = Saved(UnsafeCell::new(MaybeUninit::zeroed()));
/// The terminal whose settings `SAVED` holds; -1 when there's none.
static TTY: AtomicI32 = AtomicI32::new(-1);
/// Whether a `RawMode` exists (one per process: the settings are global).
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Whether the terminal should be raw: the SIGTSTP handler sets it raw again
/// on resuming only while this holds.
static RAW: AtomicBool = AtomicBool::new(false);

/// The user's terminal in raw mode, put back when dropped (see the module's
/// documentation).
pub(crate) struct RawMode {
    /// SIGTSTP's disposition before ours, to put back; `None` if it was
    /// ignored (then no handler: Shipyard wasn't stoppable that way).
    previous_tstp: Option<libc::sigaction>,
}

impl std::fmt::Debug for RawMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawMode").finish_non_exhaustive()
    }
}

impl RawMode {
    /// Puts the terminal on `fd` in raw mode, if Shipyard is in its
    /// foreground. `Ok(None)` if it isn't: setting it from the background
    /// would change it under the user's shell (the caller stops for job
    /// control, or goes without).
    pub(crate) fn set(fd: RawFd) -> Result<Option<Self>, Errno> {
        if ACTIVE.swap(true, Ordering::AcqRel) {
            return Err(Errno::EBUSY);
        }
        if !in_foreground(fd) {
            ACTIVE.store(false, Ordering::Release);
            return Ok(None);
        }
        TTY.store(fd, Ordering::Release);
        if let Err(errno) = save_and_go_raw() {
            TTY.store(-1, Ordering::Release);
            ACTIVE.store(false, Ordering::Release);
            return Err(errno);
        }
        RAW.store(true, Ordering::Release);
        install_panic_hook();
        let previous_tstp =
            current_action(libc::SIGTSTP).filter(|action| action.sa_sigaction != libc::SIG_IGN);
        if previous_tstp.is_some() {
            set_tstp_handler();
        }
        Ok(Some(Self { previous_tstp }))
    }

    /// Stops Shipyard with `signal` (job control), the terminal put back
    /// meanwhile, and raw again once resumed.
    // (`&self`: only while raw.)
    #[allow(clippy::unused_self, reason = "callable only with raw mode on")]
    pub(crate) fn stop_with(&self, signal: libc::c_int) {
        // SIGTSTP held off throughout: its handler, in between, would set
        // the terminal raw again before this stop, or have this save raw
        // settings. One that comes is handled after (it stops again).
        let mut tstp = sigset(&[libc::SIGTSTP]);
        let mut old = sigset(&[]);
        // SAFETY: changing this thread's mask; both sets are valid.
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw mut tstp, &raw mut old) };
        restore_saved();
        stop_self_with(signal);
        go_raw_again();
        // SAFETY: the mask as it was.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut()) };
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // No stop in between: the handler could set raw again after the
        // restore.
        let mut tstp = sigset(&[libc::SIGTSTP]);
        let mut old = sigset(&[]);
        // SAFETY: changing this thread's mask; both sets are valid.
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &raw mut tstp, &raw mut old) };
        RAW.store(false, Ordering::Release);
        restore_saved();
        if let Some(previous) = &self.previous_tstp {
            // SAFETY: putting back the action read before ours.
            unsafe { libc::sigaction(libc::SIGTSTP, previous, std::ptr::null_mut()) };
        }
        TTY.store(-1, Ordering::Release);
        ACTIVE.store(false, Ordering::Release);
        // SAFETY: the mask as it was.
        unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut()) };
    }
}

/// Reads the terminal's settings into `SAVED`, then sets it raw.
/// Async-signal-safe: the SIGTSTP handler calls it on resuming.
fn save_and_go_raw() -> Result<(), Errno> {
    let fd = TTY.load(Ordering::Acquire);
    // SAFETY: `SAVED` is only touched on this thread of control (see its
    // `Sync`); `tcgetattr` fills it in.
    let saved = unsafe { &mut *SAVED.0.get() };
    // SAFETY: `tcgetattr` writes a whole `termios` there.
    if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
        return Err(Errno::last());
    }
    // SAFETY: filled in just above.
    let mut raw = unsafe { saved.assume_init() };
    make_raw(&mut raw);
    // SAFETY: `raw` is a complete `termios`.
    Errno::result(unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw const raw) }).map(drop)
}

/// Resumed after a stop: the terminal raw again, read afresh (the user may
/// have changed it meanwhile), if Shipyard is in its foreground (`fg`, not
/// `bg`: in the background the relay stops for the terminal again, and
/// this runs once it's back). Async-signal-safe.
fn go_raw_again() {
    let fd = TTY.load(Ordering::Acquire);
    if RAW.load(Ordering::Acquire) && fd >= 0 && in_foreground(fd) {
        let _ = save_and_go_raw();
    }
}

/// Puts the saved settings back, if there are. From the background too:
/// with SIGTTOU blocked, the kernel lets it (probe T5). Async-signal-safe.
fn restore_saved() {
    let fd = TTY.load(Ordering::Acquire);
    if fd < 0 {
        return;
    }
    let mut ttou = sigset(&[libc::SIGTTOU]);
    let mut old = sigset(&[]);
    // SAFETY: changing this thread's mask; both sets are valid. `SAVED` is
    // always initialised (zeroed at first), and holds the terminal's
    // settings whenever a restore can run: only `Drop`, `stop_with` and the
    // SIGTSTP handler restore, and each needs a `RawMode`, which exists only
    // after a successful save. (A panic between `TTY`'s store and the save
    // can't happen: nothing there panics.)
    unsafe {
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw mut ttou, &raw mut old);
        libc::tcsetattr(fd, libc::TCSANOW, (*SAVED.0.get()).as_ptr());
        libc::pthread_sigmask(libc::SIG_SETMASK, &raw const old, std::ptr::null_mut());
    }
}

/// `cfmakeraw`, spelled out: async-signal-safe, and the same on Linux and
/// macOS. No input processing, no output processing, no echo, no signals
/// from keys, 8-bit characters, a read returns each byte.
fn make_raw(t: &mut libc::termios) {
    t.c_iflag &= !(libc::IGNBRK
        | libc::BRKINT
        | libc::PARMRK
        | libc::ISTRIP
        | libc::INLCR
        | libc::IGNCR
        | libc::ICRNL
        | libc::IXON);
    t.c_oflag &= !libc::OPOST;
    t.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
    t.c_cflag &= !(libc::CSIZE | libc::PARENB);
    t.c_cflag |= libc::CS8;
    t.c_cc[libc::VMIN] = 1;
    t.c_cc[libc::VTIME] = 0;
}

/// A signal set of `signals`.
fn sigset(signals: &[libc::c_int]) -> libc::sigset_t {
    let mut set = MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `sigemptyset` initialises the set; `sigaddset` only adds.
    unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        for &signal in signals {
            libc::sigaddset(set.as_mut_ptr(), signal);
        }
        set.assume_init()
    }
}

/// Stops Shipyard with `signal` (it returns once Shipyard is resumed, or at
/// once if the signal doesn't stop it).
pub(crate) fn stop_self_with(signal: libc::c_int) {
    // SAFETY: `kill` has no memory-safety conditions; this stops Shipyard
    // until its shell resumes it.
    unsafe { libc::kill(libc::getpid(), signal) };
}

/// `signal`'s current action.
pub(crate) fn current_action(signal: libc::c_int) -> Option<libc::sigaction> {
    let mut action = MaybeUninit::<libc::sigaction>::uninit();
    // SAFETY: with no new action, `sigaction` only fills in the current one,
    // read only if the call succeeded.
    (unsafe { libc::sigaction(signal, std::ptr::null(), action.as_mut_ptr()) } == 0)
        // SAFETY: filled in by the successful call.
        .then(|| unsafe { action.assume_init() })
}

fn set_tstp_handler() {
    // SAFETY: a zeroed `sigaction` is valid; the handler is filled in.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = on_tstp as extern "C" fn(libc::c_int) as libc::sighandler_t;
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: installing a handler that only makes async-signal-safe calls.
    unsafe { libc::sigaction(libc::SIGTSTP, &raw const action, std::ptr::null_mut()) };
}

/// SIGTSTP while raw: the terminal is put back, Shipyard stops as it would
/// have, and once resumed the terminal is raw again (read afresh: the user
/// may have changed it meanwhile). In an orphaned group the kernel discards
/// the stop, and this just carries on.
extern "C" fn on_tstp(_: libc::c_int) {
    let errno = Errno::last_raw();
    restore_saved();
    // SAFETY: async-signal-safe calls only: back to the default action,
    // unblock (it's blocked while its handler runs), and send it again.
    unsafe {
        libc::signal(libc::SIGTSTP, libc::SIG_DFL);
        let tstp = sigset(&[libc::SIGTSTP]);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &raw const tstp, std::ptr::null_mut());
        libc::kill(libc::getpid(), libc::SIGTSTP);
    }
    // Resumed (or the stop was discarded). SIGTSTP is held off again until
    // the handler returns (which puts the mask back): another one now would
    // run this handler inside itself, and have this save raw settings.
    // SAFETY: changing this thread's mask; the set is valid.
    unsafe {
        let tstp = sigset(&[libc::SIGTSTP]);
        libc::pthread_sigmask(libc::SIG_BLOCK, &raw const tstp, std::ptr::null_mut());
    }
    set_tstp_handler();
    go_raw_again();
    Errno::set_raw(errno);
}

/// Puts the terminal back before a panic ends Shipyard (release builds
/// abort: no `Drop` runs). Chained before whatever hook there was; installed
/// once.
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if RAW.load(Ordering::Acquire) {
                restore_saved();
            }
            previous(info);
        }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_mode_turns_off_the_line_discipline() {
        // SAFETY: a zeroed `termios` is a valid value to change.
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        t.c_lflag = libc::ICANON | libc::ECHO | libc::ISIG;
        t.c_iflag = libc::ICRNL | libc::IXON;
        t.c_oflag = libc::OPOST;
        make_raw(&mut t);
        assert_eq!(t.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG), 0);
        assert_eq!(t.c_iflag & (libc::ICRNL | libc::IXON), 0);
        assert_eq!(t.c_oflag & libc::OPOST, 0);
        assert_eq!(t.c_cc[libc::VMIN], 1);
    }

    /// The one test that uses `RawMode` (one per process).
    #[test]
    fn raw_mode_is_put_back_when_dropped() {
        let (_master, slave) = host_pty(DEFAULT_SIZE).unwrap();
        let settings = || {
            // Zeroed: musl's `termios` has control characters past the
            // kernel's, which `tcgetattr` leaves as they were.
            let mut t = MaybeUninit::<libc::termios>::zeroed();
            // SAFETY: `tcgetattr` fills in the `termios`, read only if it
            // succeeded.
            let got = unsafe { libc::tcgetattr(slave.as_raw_fd(), t.as_mut_ptr()) };
            assert_eq!(got, 0);
            // SAFETY: filled in by the successful call.
            unsafe { t.assume_init() }
        };
        let before = settings();
        assert_ne!(before.c_lflag & libc::ICANON, 0);
        // Not this process's controlling terminal: it has no foreground
        // group, which counts as in the foreground.
        let raw = RawMode::set(slave.as_raw_fd()).unwrap().unwrap();
        assert_eq!(
            settings().c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG),
            0
        );
        assert_eq!(RawMode::set(slave.as_raw_fd()).unwrap_err(), Errno::EBUSY);
        drop(raw);
        let after = settings();
        // (`PENDIN` is the kernel's own: macOS sets it on going back to
        // canonical mode, to reprocess what's waiting.)
        let flags =
            |t: &libc::termios| (t.c_iflag, t.c_oflag, t.c_lflag & !libc::PENDIN, t.c_cflag);
        assert_eq!(flags(&after), flags(&before));
        assert_eq!(after.c_cc, before.c_cc);
        // And again: a `RawMode` can be had once the last one's gone.
        drop(RawMode::set(slave.as_raw_fd()).unwrap().unwrap());
    }

    #[test]
    fn a_pty_takes_the_size_it_is_given() {
        let (master, _slave) = host_pty((40, 100)).unwrap();
        assert_eq!(window_size(master.as_raw_fd()), Some((40, 100)));
        set_window_size(master.as_raw_fd(), (30, 90)).unwrap();
        assert_eq!(window_size(master.as_raw_fd()), Some((30, 90)));
    }

    #[test]
    fn a_master_crosses_a_socket() {
        let (a, b) = socket_pair().unwrap();
        assert!(receive_fd(std::os::fd::AsFd::as_fd(&b)).unwrap().is_none());
        let (master, _slave) = host_pty(DEFAULT_SIZE).unwrap();
        send_fd(
            std::os::fd::AsFd::as_fd(&a),
            std::os::fd::AsFd::as_fd(&master),
            MASTER_NAME,
        )
        .unwrap();
        let received = receive_fd(std::os::fd::AsFd::as_fd(&b)).unwrap().unwrap();
        assert_eq!(window_size(received.as_raw_fd()), Some(DEFAULT_SIZE));
        drop(a);
        // Another test forking meanwhile holds a copy of `a` until its child
        // execs: wait for the end-of-file rather than expect it at once.
        let mut fds = [nix::poll::PollFd::new(
            std::os::fd::AsFd::as_fd(&b),
            nix::poll::PollFlags::POLLIN,
        )];
        nix::poll::poll(&mut fds, nix::poll::PollTimeout::from(5000_u16)).unwrap();
        assert_eq!(
            receive_fd(std::os::fd::AsFd::as_fd(&b)).unwrap_err(),
            Errno::ENODATA
        );
    }
}

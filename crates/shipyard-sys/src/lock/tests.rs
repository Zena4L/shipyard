use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

/// A lock file of this test's own, removed when dropped.
struct LockPath(PathBuf);

impl LockPath {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "shipyard-sys-lock-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        File::create(&path).unwrap();
        Self(path)
    }

    /// A descriptor of an open file description of its own.
    fn open(&self) -> File {
        File::open(&self.0).unwrap()
    }

    /// Whether another description could take it exclusively now.
    fn free(&self) -> bool {
        self.open().try_lock().is_ok()
    }
}

impl Drop for LockPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

const POLL: Duration = Duration::from_millis(10);

/// Holds an exclusive lock on `path` (a description of its own) for `for_`,
/// on another thread.
fn held_for(path: &LockPath, for_: Duration) -> std::thread::JoinHandle<()> {
    let holder = Flock::lock(path.open(), FlockArg::LockExclusiveNonblock).unwrap();
    std::thread::spawn(move || {
        std::thread::sleep(for_);
        drop(holder);
    })
}

#[test]
fn a_lock_let_go_while_waiting_is_taken() {
    let path = LockPath::new();
    // Before the holder's clock starts: it can only let go 100 ms after this.
    let started = Instant::now();
    let holder = held_for(&path, Duration::from_millis(100));
    let mut waited = 0;
    let locked = lock_until_with(
        path.open(),
        Mode::Exclusive,
        Instant::now() + Duration::from_secs(5),
        || {
            waited += 1;
            std::thread::sleep(POLL);
        },
    );
    holder.join().unwrap();
    locked.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(waited > 0, "it waited between tries");
}

#[test]
fn a_lock_still_held_at_the_deadline_isnt_taken() {
    let path = LockPath::new();
    let _holder = Flock::lock(path.open(), FlockArg::LockExclusiveNonblock).unwrap();
    let wait = Duration::from_millis(200);
    let file = path.open();
    let (sender, receiver) = std::sync::mpsc::channel();
    let started = Instant::now();
    // On a thread: one that never gives up fails the test, not hangs it.
    std::thread::spawn(move || {
        let _ = sender.send(lock_until(file, Mode::Exclusive, Instant::now() + wait, POLL).err());
    });
    let error = receiver
        .recv_timeout(wait + Duration::from_secs(5))
        .expect("gave up at the deadline");
    assert_eq!(error, Some(Errno::EWOULDBLOCK));
    let took = started.elapsed();
    assert!(took >= wait, "{took:?}: not before the deadline");
}

#[test]
fn a_lock_free_is_taken_at_once_even_past_its_deadline() {
    let path = LockPath::new();
    let past = Instant::now();
    let locked = lock_until(path.open(), Mode::Exclusive, past, POLL).unwrap();
    assert!(!path.free());
    drop(locked);
    assert!(path.free(), "let go when it drops");
}

#[test]
fn shared_locks_are_held_together_and_keep_out_an_exclusive_one() {
    let path = LockPath::new();
    let deadline = || Instant::now() + Duration::from_millis(50);
    let first = lock_until(path.open(), Mode::Shared, deadline(), POLL).unwrap();
    let second = lock_until(path.open(), Mode::Shared, deadline(), POLL).unwrap();
    let error = lock_until(path.open(), Mode::Exclusive, deadline(), POLL).unwrap_err();
    assert_eq!(error, Errno::EWOULDBLOCK);
    drop((first, second));
    assert!(path.free());
}

#[test]
fn a_handed_over_lock_is_waited_for_and_given_up_on_at_the_deadline() {
    let path = LockPath::new();
    let holder = held_for(&path, Duration::from_millis(100));
    let file = path.open();
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(try_lock_until(&file, Mode::Exclusive, deadline, POLL).unwrap());
    holder.join().unwrap();
    let wait = Duration::from_millis(200);
    let started = Instant::now();
    let other = path.open();
    let taken = try_lock_until(&other, Mode::Exclusive, Instant::now() + wait, POLL).unwrap();
    assert!(!taken);
    assert!(started.elapsed() >= wait);
    // Shared with another shared, not with an exclusive. (Closed, not let go:
    // a child another test forks holds a copy until it execs, so waited for.)
    drop(file);
    let shared = path.open();
    let soon = || Instant::now() + Duration::from_secs(5);
    assert!(try_lock_until(&shared, Mode::Shared, soon(), POLL).unwrap());
    assert!(try_lock_until(&other, Mode::Shared, soon(), POLL).unwrap());
    let third = path.open();
    assert!(!try_lock_until(&third, Mode::Exclusive, Instant::now(), POLL).unwrap());
}

#[test]
fn a_held_file_lets_go_when_it_drops_though_a_copy_is_open() {
    let path = LockPath::new();
    let file = path.open();
    file.try_lock().unwrap();
    // A copy another thread's child would hold until it execs.
    let copy = file.try_clone().unwrap();
    let held = HeldFile::new(file);
    assert!(!path.free(), "the control: held");
    drop(held);
    assert!(path.free(), "let go, the copy open or not");
    drop(copy);
}

#[test]
fn a_held_file_given_up_lasts_while_a_copy_is_open() {
    let path = LockPath::new();
    let file = path.open();
    file.try_lock().unwrap();
    let mut held = HeldFile::new(file);
    // The monitor's copy.
    let copy = held.hold().try_clone_to_owned().unwrap();
    held.give_up();
    drop(held);
    assert!(!path.free(), "the monitor's copy keeps it");
    drop(copy);
    // Let go once every copy is closed: a child another test forks holds
    // copies until it execs (a moment), so waited for, briefly.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.free() {
        assert!(Instant::now() < deadline, "every copy closed: let go");
        std::thread::sleep(POLL);
    }
}

/// A range lock keeps another open file description's out until it's let
/// go (Linux: elsewhere there are none).
#[cfg(target_os = "linux")]
#[test]
fn a_range_lock_keeps_another_description_out_until_let_go() {
    let path = LockPath::new();
    // A write lock needs a descriptor open for writing (as a lease's
    // rewrite has); a read lock, one open for reading.
    let writer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path.0)
        .unwrap();
    let reader = path.open();
    // Tried without waiting, as `byte_range` itself would wait.
    let try_read = |file: &File| {
        // SAFETY: plain integers; all zeros is a valid value to fill in.
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::c_short::try_from(libc::F_RDLCK).unwrap();
        lock.l_whence = libc::c_short::try_from(libc::SEEK_SET).unwrap();
        nix::fcntl::fcntl(file, nix::fcntl::FcntlArg::F_OFD_SETLK(&lock)).map(drop)
    };
    // The control: free, a read lock is had (and let go).
    try_read(&reader).unwrap();
    byte_range(&reader, ByteRange::Off).unwrap();
    byte_range(&writer, ByteRange::Write).unwrap();
    assert_eq!(try_read(&reader).unwrap_err(), Errno::EAGAIN);
    byte_range(&writer, ByteRange::Off).unwrap();
    try_read(&reader).unwrap();
}

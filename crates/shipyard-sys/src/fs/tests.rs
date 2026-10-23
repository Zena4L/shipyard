use std::ffi::OsString;
use std::os::unix::fs::{MetadataExt as _, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use nix::errno::Errno;

use super::*;

/// A fresh directory of this test's own, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "shipyard-sys-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn open(&self) -> File {
        File::open(&self.0).unwrap()
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn synced() -> usize {
    hooks::DIRECTORIES_SYNCED.with(std::cell::Cell::get)
}

fn removed() -> usize {
    hooks::TEMPORARIES_REMOVED.with(std::cell::Cell::get)
}

fn force_temporary(name: &str) {
    hooks::NEXT_TEMPORARY.set(Some(OsString::from(name)));
}

#[test]
fn a_write_replaces_the_file_whole_and_leaves_nothing_else() {
    let scratch = Scratch::new();
    write_atomically(&scratch.path("state.json"), b"old", &Write::new(0o600)).unwrap();
    write_atomically(&scratch.path("state.json"), b"new", &Write::new(0o600)).unwrap();
    assert_eq!(std::fs::read(scratch.path("state.json")).unwrap(), b"new");
    assert_eq!(scratch.names(), ["state.json"]);
}

#[test]
fn a_planted_temporary_name_is_never_written_through_but_passed_over() {
    let scratch = Scratch::new();
    std::fs::write(scratch.path("state.json"), b"old").unwrap();
    std::fs::write(scratch.path(".planted"), b"someone else's").unwrap();
    force_temporary(".planted");
    write_atomically(&scratch.path("state.json"), b"new", &Write::new(0o600)).unwrap();
    assert_eq!(
        std::fs::read(scratch.path(".planted")).unwrap(),
        b"someone else's"
    );
    assert_eq!(std::fs::read(scratch.path("state.json")).unwrap(), b"new");
    assert_eq!(scratch.names(), [".planted", "state.json"]);
}

#[test]
fn a_symlink_planted_at_the_temporary_name_is_not_followed() {
    let scratch = Scratch::new();
    std::fs::write(scratch.path("outside"), b"untouched").unwrap();
    symlink(scratch.path("outside"), scratch.path(".planted")).unwrap();
    force_temporary(".planted");
    write_atomically(&scratch.path("state.json"), b"new", &Write::new(0o600)).unwrap();
    assert_eq!(
        std::fs::read(scratch.path("outside")).unwrap(),
        b"untouched"
    );
    assert!(
        std::fs::symlink_metadata(scratch.path(".planted"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(std::fs::read(scratch.path("state.json")).unwrap(), b"new");
}

#[test]
fn a_symlink_at_the_target_is_replaced_not_followed() {
    let scratch = Scratch::new();
    std::fs::write(scratch.path("outside"), b"untouched").unwrap();
    symlink(scratch.path("outside"), scratch.path("state.json")).unwrap();
    write_atomically(&scratch.path("state.json"), b"new", &Write::new(0o600)).unwrap();
    assert!(
        std::fs::symlink_metadata(scratch.path("state.json"))
            .unwrap()
            .is_file()
    );
    assert_eq!(std::fs::read(scratch.path("state.json")).unwrap(), b"new");
    assert_eq!(
        std::fs::read(scratch.path("outside")).unwrap(),
        b"untouched"
    );
}

#[test]
fn the_mode_is_given_and_a_preparer_can_set_it_exactly() {
    let scratch = Scratch::new();
    write_atomically(&scratch.path("private"), b"x", &Write::new(0o600)).unwrap();
    let mode = |name| std::fs::metadata(scratch.path(name)).unwrap().mode() & 0o777;
    assert_eq!(mode("private"), 0o600);

    // 0o666 less a usual umask isn't 0o640: only the preparer makes it so.
    let exactly = |file: &File| set_mode(file, 0o640);
    let how = Write::new(0o666).prepared_by(&exactly);
    write_atomically(&scratch.path("shared"), b"x", &how).unwrap();
    assert_eq!(mode("shared"), 0o640);
}

#[test]
fn a_removed_directory_is_not_made_again() {
    let scratch = Scratch::new();
    std::fs::create_dir(scratch.path("gone")).unwrap();
    let dir = File::open(scratch.path("gone")).unwrap();
    std::fs::remove_dir(scratch.path("gone")).unwrap();
    let error =
        write_atomically_at(dir.as_fd(), "state.json", b"x", &Write::new(0o600)).unwrap_err();
    assert_eq!(error.step, Step::Open);
    assert_eq!(error.source.raw_os_error(), Some(libc::ENOENT));
    assert!(!scratch.path("gone").exists());
}

#[test]
fn each_durability_syncs_what_it_says() {
    let scratch = Scratch::new();
    let dir = scratch.open();
    let files = || hooks::FILES_SYNCED.with(std::cell::Cell::get);
    for (written, (durability, file, directory)) in (0_u8..).zip([
        (Durability::Synced, 1, 1),
        (Durability::FileSynced, 1, 0),
        (Durability::Atomic, 0, 0),
    ]) {
        let before = (files(), synced());
        let how = Write::new(0o600).durability(durability);
        write_atomically_at(dir.as_fd(), "f", &[written], &how).unwrap();
        assert_eq!(
            (files() - before.0, synced() - before.1),
            (file, directory),
            "{durability:?}"
        );
        // Whole all the same: the new file is in place.
        assert_eq!(std::fs::read(scratch.path("f")).unwrap(), [written]);
    }
}

#[test]
fn a_directory_that_cant_be_synced_at_all_is_done_but_a_failed_sync_is_not() {
    let scratch = Scratch::new();
    let dir = scratch.open();
    hooks::FSYNC_FAILS.set(Some(Errno::EINVAL));
    let tolerated = write_atomically_at(dir.as_fd(), "a", b"x", &Write::new(0o600));
    hooks::FSYNC_FAILS.set(Some(Errno::EIO));
    let failed = write_atomically_at(dir.as_fd(), "b", b"x", &Write::new(0o600));
    hooks::FSYNC_FAILS.set(None);
    tolerated.unwrap();
    let error = failed.unwrap_err();
    assert_eq!(error.step, Step::SyncDirectory);
    assert_eq!(error.source.raw_os_error(), Some(libc::EIO));
    // The rename itself happened: only its durability is in doubt.
    assert_eq!(std::fs::read(scratch.path("b")).unwrap(), b"x");
    hooks::FSYNC_FAILS.set(Some(Errno::EINVAL));
    let by_path = sync_directory(&scratch.0);
    hooks::FSYNC_FAILS.set(None);
    by_path.unwrap();
}

#[test]
fn a_failed_write_removes_its_temporary_and_keeps_the_old_file() {
    let scratch = Scratch::new();
    std::fs::write(scratch.path("state.json"), b"old").unwrap();
    let before = removed();
    let refuse = |_: &File| Err(io::Error::from_raw_os_error(libc::ENOSPC));
    let how = Write::new(0o600).prepared_by(&refuse);
    let error = write_atomically(&scratch.path("state.json"), b"new", &how).unwrap_err();
    assert_eq!(error.step, Step::Write);
    assert_eq!(error.source.raw_os_error(), Some(libc::ENOSPC));
    let temporary = error.temporary.unwrap().into_string().unwrap();
    let parts: Vec<&str> = temporary.split('.').collect();
    assert_eq!(
        (parts[..3].to_vec(), parts[parts.len() - 1]),
        (vec!["", "state", "json"], "tmp"),
        "{temporary}"
    );
    assert_eq!(removed(), before + 1);
    assert_eq!(scratch.names(), ["state.json"]);
    assert_eq!(std::fs::read(scratch.path("state.json")).unwrap(), b"old");
}

#[test]
fn a_path_is_refused_where_a_name_is_meant() {
    let scratch = Scratch::new();
    let dir = scratch.open();
    for name in ["", "a/b"] {
        let error = write_atomically_at(dir.as_fd(), name, b"x", &Write::new(0o600)).unwrap_err();
        assert_eq!(error.source.kind(), io::ErrorKind::InvalidInput, "{name:?}");
    }
}

#[test]
fn a_bare_name_is_written_in_the_current_directory() {
    let (dir, name) = split(Path::new("state.json")).unwrap();
    assert_eq!(
        (dir.as_path(), name),
        (Path::new("."), OsStr::new("state.json"))
    );
}

#[test]
fn a_file_up_to_its_cap_is_read_and_one_byte_more_is_refused() {
    let scratch = Scratch::new();
    std::fs::write(scratch.path("at"), [7_u8; 16]).unwrap();
    std::fs::write(scratch.path("over"), [7_u8; 17]).unwrap();
    assert_eq!(read_bounded(&scratch.path("at"), 16).unwrap().len(), 16);
    let over = read_bounded(&scratch.path("over"), 16).unwrap_err();
    assert_eq!(over.kind(), io::ErrorKind::FileTooLarge);
    let dir = scratch.open();
    assert_eq!(read_bounded_at(dir.as_fd(), "at", 16).unwrap().len(), 16);
    let over = read_bounded_at(dir.as_fd(), "over", 16).unwrap_err();
    assert_eq!(over.kind(), io::ErrorKind::FileTooLarge);
}

#[test]
fn a_symlink_is_refused_not_followed() {
    let scratch = Scratch::new();
    std::fs::write(scratch.path("outside"), b"secret").unwrap();
    symlink(scratch.path("outside"), scratch.path("link")).unwrap();
    // The positive control: the target itself reads.
    assert_eq!(
        read_bounded(&scratch.path("outside"), 64).unwrap(),
        b"secret"
    );
    let error = read_bounded(&scratch.path("link"), 64).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
    let dir = scratch.open();
    let error = read_bounded_at(dir.as_fd(), "link", 64).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
}

#[test]
fn a_fifo_or_a_directory_is_refused_without_blocking() {
    let scratch = Scratch::new();
    nix::unistd::mkfifo(&scratch.path("fifo"), Mode::from_bits_truncate(0o600)).unwrap();
    std::fs::create_dir(scratch.path("dir")).unwrap();
    let (fifo, dir) = (scratch.path("fifo"), scratch.path("dir"));
    let (sender, receiver) = mpsc::channel();
    // A reader that blocked on the FIFO would never answer: the test fails
    // on the timeout rather than hanging.
    std::thread::spawn(move || {
        let _ = sender.send((read_bounded(&fifo, 64), read_bounded(&dir, 64)));
    });
    let (fifo, dir) = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(fifo.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    assert_eq!(dir.unwrap_err().kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn a_missing_file_is_not_found() {
    let scratch = Scratch::new();
    let error = read_bounded(&scratch.path("missing"), 64).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
}

#[test]
fn a_lock_file_is_made_private_and_a_symlink_there_is_refused() {
    let scratch = Scratch::new();
    open_lock_file(&scratch.path("lock")).unwrap();
    let mode = std::fs::metadata(scratch.path("lock")).unwrap().mode() & 0o777;
    assert_eq!(mode, 0o600);
    std::fs::write(scratch.path("outside"), b"").unwrap();
    symlink(scratch.path("outside"), scratch.path("link")).unwrap();
    let error = open_lock_file(&scratch.path("link")).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
    let dir = scratch.open();
    open_lock_file_at(dir.as_fd(), "lock-at").unwrap();
    assert!(scratch.path("lock-at").exists());
    assert_eq!(
        open_lock_file_at(dir.as_fd(), "link").unwrap_err(),
        Errno::ELOOP
    );
}

#[test]
fn an_open_file_is_read_as_it_is_with_the_same_rules() {
    let scratch = Scratch::new();
    std::fs::write(scratch.path("lease"), b"ctr").unwrap();
    let file = File::open(scratch.path("lease")).unwrap();
    // Replaced at its name meanwhile: the open file is still the one read.
    std::fs::remove_file(scratch.path("lease")).unwrap();
    std::fs::write(scratch.path("lease"), b"another").unwrap();
    assert_eq!(read_bounded_from(&file, 3).unwrap(), b"ctr");
    let file = File::open(scratch.path("lease")).unwrap();
    let error = read_bounded_from(&file, 3).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
}

#[test]
fn a_refusal_is_told_apart_from_the_systems_errors() {
    let scratch = Scratch::new();
    std::fs::create_dir(scratch.path("dir")).unwrap();
    std::fs::write(scratch.path("over"), [0_u8; 9]).unwrap();
    let not_a_file = read_bounded(&scratch.path("dir"), 64).unwrap_err();
    assert_eq!(refusal(&not_a_file), Some(Refusal::NotAFile));
    let too_large = read_bounded(&scratch.path("over"), 8).unwrap_err();
    assert_eq!(refusal(&too_large), Some(Refusal::TooLarge { cap: 8 }));
    // The system's own, of the same kinds, are not refusals.
    for errno in [libc::EINVAL, libc::EFBIG, libc::ENOENT] {
        assert_eq!(
            refusal(&io::Error::from_raw_os_error(errno)),
            None,
            "{errno}"
        );
    }
    let missing = read_bounded(&scratch.path("missing"), 8).unwrap_err();
    assert_eq!(refusal(&missing), None);
}

#[test]
fn a_file_already_too_large_is_refused_unread() {
    let scratch = Scratch::new();
    // Sparse: 1 GiB claimed, nothing read (a read would take a while).
    let file = File::create(scratch.path("huge")).unwrap();
    file.set_len(1 << 30).unwrap();
    let started = std::time::Instant::now();
    let error = read_bounded(&scratch.path("huge"), 16).unwrap_err();
    assert_eq!(refusal(&error), Some(Refusal::TooLarge { cap: 16 }));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_write_gives_up_once_every_name_it_tries_is_taken() {
    let scratch = Scratch::new();
    std::fs::write(scratch.path(".planted"), b"someone else's").unwrap();
    hooks::EVERY_TEMPORARY.set(Some(OsString::from(".planted")));
    let written = write_atomically(&scratch.path("state.json"), b"new", &Write::new(0o600));
    hooks::EVERY_TEMPORARY.set(None);
    let error = written.unwrap_err();
    assert_eq!(error.step, Step::Open);
    assert_eq!(error.source.raw_os_error(), Some(libc::EEXIST));
    assert_eq!(error.temporary.as_deref(), Some(OsStr::new(".planted")));
    assert_eq!(
        std::fs::read(scratch.path(".planted")).unwrap(),
        b"someone else's"
    );
    assert_eq!(scratch.names(), [".planted"]);
}

#[test]
fn text_that_isnt_utf8_fails_as_reading_text_does() {
    let scratch = Scratch::new();
    std::fs::write(scratch.path("bytes"), [0xff]).unwrap();
    let std_said = std::fs::read_to_string(scratch.path("bytes")).unwrap_err();
    assert_eq!(text(b"ok".to_vec()).unwrap(), "ok");
    let error = text(vec![0xff]).unwrap_err();
    assert_eq!(
        (error.kind(), error.to_string()),
        (std_said.kind(), std_said.to_string())
    );
}

/// A directory this user may write in and search but not list: written in
/// as a path would be (on Linux; root can always list it).
#[cfg(target_os = "linux")]
#[test]
fn a_drop_box_directory_is_written_in() {
    use std::os::unix::fs::PermissionsExt as _;
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let scratch = Scratch::new();
    std::fs::create_dir(scratch.path("drop")).unwrap();
    std::fs::set_permissions(scratch.path("drop"), std::fs::Permissions::from_mode(0o300)).unwrap();
    // The control: it really can't be listed.
    assert!(std::fs::read_dir(scratch.path("drop")).is_err());
    let before = synced();
    let written = write_atomically(&scratch.path("drop/c.pid"), b"42", &Write::new(0o644));
    std::fs::set_permissions(scratch.path("drop"), std::fs::Permissions::from_mode(0o700)).unwrap();
    written.unwrap();
    assert_eq!(std::fs::read(scratch.path("drop/c.pid")).unwrap(), b"42");
    assert_eq!(synced(), before, "not synced through O_PATH");
}

/// A file that says it's small and reads longer (a `/proc` file says 0
/// bytes; a file growing meanwhile does the same) is still cut at its
/// bound and refused.
#[cfg(target_os = "linux")]
#[test]
fn a_file_longer_than_it_says_is_refused_at_its_bound() {
    let path = Path::new("/proc/self/status");
    assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
    // The control: read whole, it's longer than the bound.
    assert!(std::fs::read(path).unwrap().len() > 16);
    let error = read_bounded(path, 16).unwrap_err();
    assert_eq!(refusal(&error), Some(Refusal::TooLarge { cap: 16 }));
}

/// A directory made private is the caller's alone; one others can write to,
/// or a symbolic link to one, is said to be unsafe.
#[test]
fn private_directories_are_the_caller_s_alone() {
    use std::os::unix::fs::PermissionsExt as _;
    let base = std::env::temp_dir().join(format!("shipyard-sys-private-{}", std::process::id()));
    let dir = base.join("a").join("b");
    assert_eq!(super::private_dir(&dir).unwrap(), None);
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        super::private_dir(&dir).unwrap(),
        Some("others can write to it")
    );
    let link = base.join("link");
    std::os::unix::fs::symlink(base.join("a"), &link).unwrap();
    assert!(super::unsafe_dir(&link).unwrap().is_some());
    std::fs::remove_dir_all(&base).unwrap();
}

/// What's missing below a directory is made, each component a directory;
/// a symlink on the way is refused (`ELOOP` on Linux, `ENOTDIR` on macOS,
/// for `O_NOFOLLOW | O_DIRECTORY`), never followed out of it,
/// and its target is left alone; a path that isn't plain is refused. The
/// control: the same path without the symlink is made and opened.
#[test]
fn below_a_directory_no_symlink_is_followed() {
    let scratch = Scratch::new();
    let dir = scratch.open();
    let made = make_below(dir.as_fd(), Path::new("a/b/c"), 0o755).unwrap();
    assert!(scratch.path("a/b/c").is_dir());
    drop(made);
    open_below(dir.as_fd(), Path::new("a/b/c")).unwrap();

    let outside = Scratch::new();
    std::fs::remove_dir_all(scratch.path("a/b")).unwrap();
    symlink(&outside.0, scratch.path("a/b")).unwrap();
    for walked in [
        make_below(dir.as_fd(), Path::new("a/b/c"), 0o755),
        open_below(dir.as_fd(), Path::new("a/b")),
    ] {
        let refused = walked.unwrap_err();
        assert!(
            matches!(refused.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)),
            "{refused}"
        );
    }
    assert!(outside.names().is_empty(), "nothing made through the link");
    std::fs::write(scratch.path("file"), "x").unwrap();
    let refused = open_below(dir.as_fd(), Path::new("file")).unwrap_err();
    assert_eq!(refused.raw_os_error(), Some(libc::ENOTDIR), "{refused}");
    for unplain in ["../a", "/a", "a/../b"] {
        let refused = make_below(dir.as_fd(), Path::new(unplain), 0o755).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "{unplain}");
    }
}

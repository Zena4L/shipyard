//! The attach server against real sockets: sizes, holders, a client
//! too far behind, descriptors passed, stops.

use super::*;
use crate::attach::{Decoder, HOLD_VERSION, HoldRefusal, VERSION};
use std::os::fd::BorrowedFd;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::net::UnixStream;

/// An attach socket of a test's own, in a scratch directory, with what
/// the monitor holds (taking more) or not.
fn listening(
    name: &str,
    holds: Option<super::super::holds::Holds>,
) -> (Attachments, std::path::PathBuf) {
    listening_with(name, holds, false)
}

/// [`listening`], for a container with a terminal or without.
fn listening_with(
    name: &str,
    holds: Option<super::super::holds::Holds>,
    terminal: bool,
) -> (Attachments, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("shipyard-attach-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fd = OwnedFd::from(std::fs::File::open(&dir).unwrap());
    let mut attachments = Attachments::listen(&fd, terminal, false, None).unwrap();
    if let Some(holds) = holds {
        attachments.serve_holds(holds);
    }
    (attachments, dir)
}

/// A client of `attachments`, connected.
fn connect(dir: &std::path::Path) -> UnixStream {
    let stream = UnixStream::connect(dir.join(SOCKET)).unwrap();
    stream.set_nonblocking(true).unwrap();
    stream
}

fn send(stream: &UnixStream, frame: &Frame, fd: Option<BorrowedFd<'_>>) {
    send_fds(stream, frame, fd.as_slice());
}

/// Sends `frame` with `fds` passed alongside, in one message.
fn send_fds(stream: &UnixStream, frame: &Frame, fds: &[BorrowedFd<'_>]) {
    use std::os::fd::AsFd as _;
    let mut bytes = Vec::new();
    frame.encode(&mut bytes);
    let flags = nix::sys::socket::MsgFlags::empty();
    let sent = shipyard_sys::fdpass::send(stream.as_fd(), fds, &bytes, flags).unwrap();
    assert_eq!(sent, bytes.len());
}

/// The next frame for `stream`, `attachments` served meanwhile; `None`
/// if it's let go of (or nothing comes within a few seconds).
fn next(
    attachments: &mut Attachments,
    stream: &UnixStream,
    decoder: &mut Decoder,
) -> Option<Frame> {
    use std::io::Read as _;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buffer = [0_u8; 1024];
    while Instant::now() < deadline {
        if let Ok(Some(frame)) = decoder.next_frame() {
            return Some(frame);
        }
        attachments.service();
        match (&*stream).read(&mut buffer) {
            Ok(0) => return decoder.next_frame().ok().flatten(),
            Ok(read) => decoder.push(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
    None
}

/// A holder let in, with `version`.
fn holder(
    attachments: &mut Attachments,
    dir: &std::path::Path,
    version: u32,
) -> (UnixStream, Decoder, Option<Frame>) {
    let stream = connect(dir);
    let mut decoder = Decoder::new();
    send(&stream, &Frame::HelloHolder { version }, None);
    let welcome = next(attachments, &stream, &mut decoder);
    (stream, decoder, welcome)
}

/// A regular file to pass (this test's own program: there, and not a
/// device, which isn't held).
fn file() -> std::fs::File {
    std::fs::File::open(std::env::current_exe().unwrap()).unwrap()
}

/// A size asked for and typing after it, in one read, reach the
/// terminal in that order (F68): the size is set before the typing is
/// written, so the container, reading it, has the new size; the
/// control, the size was the old one before.
#[test]
fn a_size_is_set_before_the_typing_after_it() {
    use std::io::{Read as _, Write as _};
    // A terminal's, taking stdin (`listening_with` takes none).
    let dir =
        std::env::temp_dir().join(format!("shipyard-attach-size-first-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let fd = OwnedFd::from(std::fs::File::open(&dir).unwrap());
    let mut attachments = Attachments::listen(&fd, true, true, None).unwrap();
    let (master, slave) =
        crate::process::terminal::host_pty(crate::process::terminal::DEFAULT_SIZE).unwrap();
    attachments.set_master(&master);
    let stream = connect(&dir);
    let mut decoder = Decoder::new();
    send(
        &stream,
        &Frame::Hello {
            version: VERSION,
            detach_keys: Vec::new(),
        },
        None,
    );
    assert!(matches!(
        next(&mut attachments, &stream, &mut decoder),
        Some(Frame::Welcome { .. })
    ));
    assert_eq!(
        crate::process::terminal::window_size(master.as_raw_fd()),
        Some(crate::process::terminal::Size::new(24, 80)),
        "the control"
    );
    let sizes_before = attachments.sizes_set;
    let mut both = Vec::new();
    Frame::Resize { rows: 33, cols: 99 }.encode(&mut both);
    // A line: the terminal's line discipline passes it on whole.
    Frame::Stdin(b"x\n".to_vec()).encode(&mut both);
    (&stream).write_all(&both).unwrap();
    let mut slave = std::fs::File::from(slave);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut typed = [0_u8; 2];
    loop {
        attachments.service();
        // The size, read the moment the typing can be.
        let mut fds = [nix::poll::PollFd::new(
            slave.as_fd(),
            nix::poll::PollFlags::POLLIN,
        )];
        if nix::poll::poll(&mut fds, nix::poll::PollTimeout::ZERO).unwrap() > 0 {
            let size = crate::process::terminal::window_size(slave.as_raw_fd());
            slave.read_exact(&mut typed).unwrap();
            assert_eq!(
                size,
                Some(crate::process::terminal::Size::new(33, 99)),
                "the size before the typing"
            );
            break;
        }
        assert!(Instant::now() < deadline, "the typing never came");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(&typed, b"x\n");
    // And within the monitor's one read of both: the size was set
    // before the typing was written, not after (what the terminal,
    // read once the read is done, can't tell).
    assert_eq!(
        attachments.typed_after_sizes,
        sizes_before + 1,
        "the typing written after the size"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A flood of sizes in one read sets the terminal's size once, to the
/// last asked for: one SIGWINCH for the container, not one each (F40).
#[test]
fn a_flood_of_sizes_is_one_change() {
    use std::io::Write as _;
    let (mut attachments, dir) = listening_with("sizes", None, true);
    let (master, _slave) =
        crate::process::terminal::host_pty(crate::process::terminal::DEFAULT_SIZE).unwrap();
    attachments.set_master(&master);
    let stream = connect(&dir);
    let mut decoder = Decoder::new();
    send(
        &stream,
        &Frame::Hello {
            version: VERSION,
            detach_keys: Vec::new(),
        },
        None,
    );
    assert!(
        matches!(
            next(&mut attachments, &stream, &mut decoder),
            Some(Frame::Welcome { .. })
        ),
        "welcomed"
    );
    let before = attachments.sizes_set;
    let mut flood = Vec::new();
    for n in 0..200_u16 {
        Frame::Resize {
            rows: 10 + n,
            cols: 40 + n,
        }
        .encode(&mut flood);
    }
    (&stream).write_all(&flood).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while attachments.sizes_set == before && Instant::now() < deadline {
        attachments.service();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(attachments.sizes_set - before, 1, "one change");
    assert_eq!(
        crate::process::terminal::window_size(master.as_raw_fd()),
        Some(crate::process::terminal::Size::new(209, 239))
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A holder's hold: the descriptor passed with it is held by its label
/// (the same file), answered `Done`; a release lets go of it.
#[test]
fn a_holder_holds_and_releases() {
    let (mut attachments, dir) =
        listening("hold", Some(super::super::holds::Holds::new(Vec::new())));
    let (stream, mut decoder, welcome) = holder(&mut attachments, &dir, VERSION);
    assert!(
        matches!(
            welcome,
            Some(Frame::Welcome {
                version: VERSION,
                ..
            })
        ),
        "{welcome:?}"
    );
    let given = std::fs::File::open(&dir).unwrap();
    send(
        &stream,
        &Frame::Hold {
            label: "lease:app".to_owned(),
        },
        Some(given.as_fd()),
    );
    assert_eq!(
        next(&mut attachments, &stream, &mut decoder),
        Some(Frame::Done {
            ok: true,
            why: String::new()
        })
    );
    let held = attachments.take_holds();
    assert_eq!(held.labels(), "lease:app");
    let same =
        |fd: &OwnedFd| nix::sys::stat::fstat(fd).unwrap().st_ino == given.metadata().unwrap().ino();
    assert!(held.fds().all(same));
    attachments.serve_holds(held);
    let (stream, mut decoder, _) = holder(&mut attachments, &dir, VERSION);
    send(
        &stream,
        &Frame::Release {
            label: "lease:app".to_owned(),
        },
        None,
    );
    assert_eq!(
        next(&mut attachments, &stream, &mut decoder),
        Some(Frame::Done {
            ok: true,
            why: String::new()
        })
    );
    assert_eq!(attachments.take_holds().len(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Refused: a hold with no descriptor, one before the monitor's holds
/// are here or once it ends, a release of nothing; a holder of an older
/// version isn't let in.
#[test]
fn what_a_holder_cant_do() {
    let (mut attachments, dir) = listening("refused", None);
    let refused = |attachments: &mut Attachments, frame: &Frame, fd: Option<BorrowedFd<'_>>| {
        let (stream, mut decoder, _) = holder(attachments, &dir, VERSION);
        send(&stream, frame, fd);
        next(attachments, &stream, &mut decoder)
    };
    let hold = Frame::Hold {
        label: "lease:app".to_owned(),
    };
    let dev_null = file();
    // Not handed its holds yet: none taken.
    assert!(matches!(
        refused(&mut attachments, &hold, Some(dev_null.as_fd())),
        Some(Frame::NotDone {
            reason: HoldRefusal::Ending,
            ..
        })
    ));
    attachments.serve_holds(super::super::holds::Holds::new(Vec::new()));
    assert!(matches!(
        refused(&mut attachments, &hold, None),
        Some(Frame::NotDone { reason: HoldRefusal::NoDescriptor, why })
            if why.contains("no descriptor")
    ));
    let release = Frame::Release {
        label: "lease:x".to_owned(),
    };
    assert!(matches!(
        refused(&mut attachments, &release, None),
        Some(Frame::NotDone { reason: HoldRefusal::NotHeld, why })
            if why.contains("nothing is held as lease:x")
    ));
    // A version 3 holder (an older CLI) is refused in words only.
    let (stream, mut decoder, _) = holder(&mut attachments, &dir, HOLD_VERSION);
    send(&stream, &release, None);
    assert_eq!(
        next(&mut attachments, &stream, &mut decoder),
        Some(Frame::Done {
            ok: false,
            why: "nothing is held as lease:x".to_owned()
        })
    );
    attachments.stop_taking_holds();
    assert!(matches!(
        refused(&mut attachments, &hold, Some(dev_null.as_fd())),
        Some(Frame::NotDone { reason: HoldRefusal::Ending, why })
            if why.contains("starting or ending")
    ));
    assert_eq!(attachments.take_holds().len(), 0);
    let (_stream, _decoder, welcome) = holder(&mut attachments, &dir, 2);
    assert!(matches!(welcome, Some(Frame::Refused(_))), "{welcome:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// An attached client sent more than [`CLIENT_BUFFER`] at once is let
/// go with `Dropped`, after what came before it (F83: deterministic
/// here, as the client reads, where a stalled one's socket may be full).
#[test]
fn a_client_too_far_behind_is_dropped_with_a_frame() {
    let (mut attachments, dir) = listening("behind", None);
    let stream = connect(&dir);
    let mut decoder = Decoder::new();
    send(
        &stream,
        &Frame::Hello {
            version: VERSION,
            detach_keys: Vec::new(),
        },
        None,
    );
    assert!(matches!(
        next(&mut attachments, &stream, &mut decoder),
        Some(Frame::Welcome { .. })
    ));
    attachments.output(Stream::Stdout, &vec![b'x'; CLIENT_BUFFER + 1]);
    let mut last = None;
    while let Some(frame) = next(&mut attachments, &stream, &mut decoder) {
        let dropped = matches!(frame, Frame::Dropped(_));
        last = Some(frame);
        if dropped {
            break;
        }
    }
    assert!(
        matches!(&last, Some(Frame::Dropped(reason)) if reason.contains("256 KiB")),
        "{last:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Whether `stream`'s connection is let go of (end-of-file) within two
/// seconds, `attachments` served meanwhile (what's sent to it read and
/// dropped).
fn let_go(attachments: &mut Attachments, stream: &UnixStream) -> bool {
    use std::io::Read as _;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buffer = [0_u8; 1024];
    while Instant::now() < deadline {
        attachments.service();
        match (&*stream).read(&mut buffer) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return true,
        }
    }
    false
}

/// A descriptor from anyone but a holder (an attached client typing
/// here, which would otherwise stay), or two at once, is closed, and
/// its client let go of.
#[test]
fn a_descriptor_from_anyone_else_is_refused() {
    let (mut attachments, dir) =
        listening("waiter", Some(super::super::holds::Holds::new(Vec::new())));
    let dev_null = file();
    // Typing, plain: it stays.
    let stream = connect(&dir);
    let mut decoder = Decoder::new();
    send(
        &stream,
        &Frame::Hello {
            version: VERSION,
            detach_keys: Vec::new(),
        },
        None,
    );
    assert!(matches!(
        next(&mut attachments, &stream, &mut decoder),
        Some(Frame::Welcome { .. })
    ));
    send(&stream, &Frame::Stdin(b"x".to_vec()), None);
    assert!(!let_go(&mut attachments, &stream), "a plain client stays");
    // Typing with a descriptor: let go of.
    let stream = connect(&dir);
    let mut decoder = Decoder::new();
    send(
        &stream,
        &Frame::Hello {
            version: VERSION,
            detach_keys: Vec::new(),
        },
        None,
    );
    assert!(matches!(
        next(&mut attachments, &stream, &mut decoder),
        Some(Frame::Welcome { .. })
    ));
    send(
        &stream,
        &Frame::Stdin(b"x".to_vec()),
        Some(dev_null.as_fd()),
    );
    assert!(
        let_go(&mut attachments, &stream),
        "a descriptor from an attached client"
    );
    // A holder passing two in one message, or three (more than one
    // message's room once was: none may be left open here, the
    // monitor's process): let go of, nothing held.
    // (Files of this test's own: whether one is still open anywhere in
    // this process is told by its name, whatever other tests open.)
    let passed: Vec<std::path::PathBuf> =
        (0..5).map(|at| dir.join(format!("passed-{at}"))).collect();
    for (count, paths) in [(2, &passed[..2]), (3, &passed[2..])] {
        let given: Vec<std::fs::File> = paths
            .iter()
            .map(|path| std::fs::File::create(path).unwrap())
            .collect();
        let fds: Vec<BorrowedFd<'_>> = given.iter().map(AsFd::as_fd).collect();
        let (stream, _decoder, _) = holder(&mut attachments, &dir, VERSION);
        send_fds(
            &stream,
            &Frame::Hold {
                label: "a".to_owned(),
            },
            &fds,
        );
        assert!(
            let_go(&mut attachments, &stream),
            "{count} descriptors at once"
        );
    }
    assert_eq!(attachments.take_holds().len(), 0);
    let still_open: Vec<std::path::PathBuf> = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter(|target| passed.contains(target))
        .collect();
    assert!(still_open.is_empty(), "left open: {still_open:?}");
    // Only a file, a directory or a socket is held: a pipe's end isn't.
    attachments.serve_holds(super::super::holds::Holds::new(Vec::new()));
    let (reading, _writing) = nix::unistd::pipe().unwrap();
    let (stream, mut decoder, _) = holder(&mut attachments, &dir, VERSION);
    send(
        &stream,
        &Frame::Hold {
            label: "a".to_owned(),
        },
        Some(reading.as_fd()),
    );
    assert!(matches!(
        next(&mut attachments, &stream, &mut decoder),
        Some(Frame::NotDone { reason: HoldRefusal::NotHoldable, why })
            if why.contains("only a file")
    ));
    // A release that brings a descriptor: refused.
    let (stream, mut decoder, _) = holder(&mut attachments, &dir, VERSION);
    send(
        &stream,
        &Frame::Release {
            label: "a".to_owned(),
        },
        Some(dev_null.as_fd()),
    );
    assert!(matches!(
        next(&mut attachments, &stream, &mut decoder),
        Some(Frame::NotDone { reason: HoldRefusal::DescriptorWithRelease, why })
            if why.contains("came with the release")
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stop's signal must end the init's command: a real signal (to the
/// last real-time one), not one that stops or continues it, nor one the
/// init can't pass on; its timeout is in seconds.
#[test]
fn a_stop_is_only_a_signal_that_ends() {
    assert_eq!(
        StopRequest::of(15, 10),
        Some(StopRequest {
            signal: libc::SIGTERM,
            timeout: Duration::from_secs(10),
        })
    );
    let number = |signal: i32| u8::try_from(signal).unwrap();
    assert!(StopRequest::of(number(libc::SIGKILL), 0).is_some());
    assert!(
        StopRequest::of(number(libc::SIGRTMAX()), 0).is_some(),
        "the last real-time one"
    );
    for refused in [
        0,
        libc::SIGSTOP,
        libc::SIGTSTP,
        libc::SIGTTIN,
        libc::SIGTTOU,
        libc::SIGCONT,
        libc::SIGCHLD,
        32,
        33,
        libc::SIGRTMAX() + 1,
        255,
    ] {
        assert_eq!(StopRequest::of(number(refused), 10), None, "{refused}");
    }
}

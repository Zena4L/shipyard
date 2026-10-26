use shipyard_runtime::attach::Frame;
use shipyard_runtime::attach::forward::{Connection, Request};

use super::command::tries;

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

/// A refusal by its code (F99) is known whatever its words: a stopping
/// host's command is sent again, a not-ready guest's asked again.
#[test]
fn a_refusal_is_known_by_its_code() {
    let declined = |reason| {
        vec![Frame::Declined {
            reason,
            why: "words of another version".to_owned(),
        }]
    };
    let welcome = Frame::Welcome {
        version: shipyard_runtime::attach::VERSION,
        terminal: false,
        stdin: false,
    };
    let request = Request::new(vec!["ps".to_owned()], "/".to_owned(), Vec::new());
    let paths = scratch("declined");
    for reason in [
        shipyard_runtime::attach::Decline::Stopping,
        shipyard_runtime::attach::Decline::NotReady,
    ] {
        let host = fake_host(
            &paths,
            vec![declined(reason), vec![welcome.clone(), Frame::Exited(7)]],
        );
        let tried = tries(&paths, &request, &mut None);
        assert!(
            matches!(tried, Ok(Outcome::Exited(7))),
            "{reason:?}: {tried:?}"
        );
        host.join().unwrap();
        let _ = std::fs::remove_file(paths.socket());
    }
    let _ = std::fs::remove_dir_all(&paths.dir);
}

/// A guest not ready yet (its data disk being checked) refuses each try:
/// asked again until it's ready, then the command runs (never cut short
/// by a bound: D10.4-4).
#[test]
fn a_command_waits_for_a_guest_that_isnt_ready() {
    let not_ready = || vec![Frame::Refused(forward::NOT_READY.to_owned())];
    let welcome = Frame::Welcome {
        version: shipyard_runtime::attach::VERSION,
        terminal: false,
        stdin: false,
    };
    let request = Request::new(vec!["ps".to_owned()], "/".to_owned(), Vec::new());
    let paths = scratch("not-ready");
    let host = fake_host(
        &paths,
        vec![
            not_ready(),
            not_ready(),
            not_ready(),
            vec![welcome, Frame::Exited(3)],
        ],
    );
    let tried = tries(&paths, &request, &mut None);
    assert!(matches!(tried, Ok(Outcome::Exited(3))), "{tried:?}");
    host.join().unwrap();
    let _ = std::fs::remove_dir_all(&paths.dir);
}

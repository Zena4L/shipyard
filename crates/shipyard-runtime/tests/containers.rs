//! Containers through the runtime's API, end to end: joining a running one
//! (`Supervisor::supervise_joined`), and isolation without a PID namespace.
//! Linux; root only, except for the "needs root" check.
//!
//! Not on libtest (`harness = false`): this binary is also its own container
//! runtime. It plays the part the CLI will (stage 8+):
//!
//! - `containers container [--rootfs DIR] CMD…` supervises CMD in a new
//!   container (hostname `box`), with DIR as its root filesystem if given.
//!   The container is set up by this binary, re-exec'd, so `main` calls
//!   `init_entry_point` first;
//! - `containers container-nopid [--churn] CMD…` does the same without a PID
//!   namespace (so without an init), optionally while another thread keeps
//!   starting and ending threads;
//! - `containers join PID CMD…` supervises CMD inside PID's namespaces.
//!
//! Each exits with the command's exit code (128 + N for a signal), or 125 and
//! the error on stderr. The tests drive them as subprocesses, one supervisor
//! per process as the runtime requires, and run one at a time.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use shipyard_runtime::isolation::{Isolation, Join, Namespace, Root};
use shipyard_runtime::process::{self, ProcessSpec, StopPolicy, Supervisor};
use shipyard_test_support::rootfs::{MARKER, RootFs};
use shipyard_test_support::{
    Running, Shipyard, child_of, gone, is_root, run, start_command, under_rosetta,
};

fn main() -> ExitCode {
    process::init_entry_point();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = serve_if_asked(&args) {
        return code;
    }
    let tests = tests();
    if args.iter().any(|arg| arg == "--list") {
        for (name, _) in tests {
            println!("{name}: test");
        }
        return ExitCode::SUCCESS;
    }
    println!("\nrunning {} tests", tests.len());
    let mut failed = Vec::new();
    for (name, test) in tests {
        print!("test {name} ... ");
        let _ = std::io::stdout().flush();
        // Last resort against a hang: SIGALRM ends this binary.
        // SAFETY: `alarm` has no preconditions.
        unsafe { libc::alarm(90) };
        let passed = std::panic::catch_unwind(test).is_ok();
        println!("{}", if passed { "ok" } else { "FAILED" });
        if !passed {
            failed.push(*name);
        }
    }
    // SAFETY: as above; 0 cancels it.
    unsafe { libc::alarm(0) };
    println!(
        "\ntest result: {}. {} passed; {} failed",
        if failed.is_empty() { "ok" } else { "FAILED" },
        tests.len() - failed.len(),
        failed.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// The tests this system can run: the root ones as root.
fn tests() -> &'static [(&'static str, fn())] {
    if !cfg!(target_os = "linux") {
        &[("joining_needs_linux", joining_needs_linux)]
    } else if is_root() {
        &[
            (
                "the_joined_command_sees_the_containers_world",
                sees_the_containers_world,
            ),
            ("results_come_back", results_come_back),
            (
                "a_stop_request_reaches_the_joined_command",
                a_stop_request_reaches_it,
            ),
            ("signals_are_forwarded", signals_are_forwarded),
            (
                "the_joined_command_ends_with_the_container",
                ends_with_the_container,
            ),
            ("the_joined_command_dies_with_shipyard", dies_with_shipyard),
            (
                "its_orphans_belong_to_the_containers_init",
                orphans_belong_to_the_init,
            ),
            ("no_descriptors_of_ours_reach_it", no_descriptors_reach_it),
            (
                "joining_a_container_that_has_exited_fails",
                joining_an_exited_container,
            ),
            (
                "other_threads_keep_starting_processes_on_the_host",
                other_threads_unaffected,
            ),
            (
                "joining_a_container_that_is_exiting_fails",
                joining_an_exiting_container,
            ),
            (
                "a_stopped_joiner_holds_up_the_containers_end",
                a_stopped_joiner_holds_up_the_end,
            ),
            (
                "with_another_namespaces_proc_joining_is_refused",
                another_namespaces_proc,
            ),
            (
                "the_joined_command_sees_the_containers_root_filesystem",
                sees_the_containers_root_filesystem,
            ),
            (
                "without_a_pid_namespace_the_command_is_shipyards_child",
                without_a_pid_namespace_the_command_is_shipyards_child,
            ),
            (
                "without_a_pid_namespace_under_thread_churn",
                without_a_pid_namespace_under_thread_churn,
            ),
            (
                "an_init_that_cannot_start_is_reported_without_a_pid_namespace",
                an_init_that_cannot_start_is_reported_without_a_pid_namespace,
            ),
        ]
    } else {
        &[(
            "without_root_joining_is_refused",
            without_root_joining_is_refused,
        )]
    }
}

/// Runs one of the serving modes when the arguments ask for one.
fn serve_if_asked(args: &[String]) -> Option<ExitCode> {
    let rest = args.get(1..).unwrap_or_default();
    Some(match args.first().map(String::as_str)? {
        "container" => match rest {
            [flag, dir, command @ ..] if flag == "--rootfs" => serve(
                Mode::Container {
                    root: Some(dir.into()),
                },
                command,
            ),
            _ => serve(Mode::Container { root: None }, rest),
        },
        "container-nopid" => {
            let churn = rest.first().is_some_and(|arg| arg == "--churn");
            let command = if churn {
                rest.get(1..).unwrap_or_default()
            } else {
                rest
            };
            serve(Mode::NoPidContainer { churn }, command)
        }
        "join" => {
            let Some(target) = rest.first().and_then(|pid| pid.parse().ok()) else {
                eprintln!("join: the target must be a PID");
                return Some(ExitCode::from(2));
            };
            serve(Mode::Join(target), rest.get(1..).unwrap_or_default())
        }
        _ => return None,
    })
}

// --- The modes --------------------------------------------------------------

/// What this binary does when it isn't running the tests.
enum Mode {
    /// Supervise a command in a new container, with a root filesystem of its
    /// own if `root` is given.
    Container { root: Option<std::path::PathBuf> },
    /// The same without a PID namespace; with `churn`, while another thread
    /// keeps starting and ending threads.
    NoPidContainer { churn: bool },
    /// Supervise a command inside this process's namespaces.
    Join(u32),
}

/// The standard isolation, without a seccomp filter under Rosetta, where the
/// kernel refuses every one (stage 4's probe, S5; as the test support does
/// for the CLI).
fn standard_isolation() -> Isolation {
    let mut isolation = Isolation::standard();
    if under_rosetta() {
        isolation.security.seccomp = shipyard_runtime::security::Seccomp::Unconfined;
    }
    isolation
}

/// Supervises `command` as `mode` says. In join mode it also runs a thread
/// that keeps starting processes meanwhile, which must all stay on the host,
/// and checks afterwards that this thread's children are born on the host
/// again.
fn serve(mode: Mode, command: &[String]) -> ExitCode {
    let mut spec = ProcessSpec::new(command.iter().map(Into::into).collect());
    spec.env.fill_default_path();
    let stop = StopPolicy {
        timeout: Duration::from_secs(5),
        ..StopPolicy::default()
    };
    let mut supervisor = match Supervisor::install() {
        Ok(supervisor) => supervisor,
        Err(err) => return fail(&err),
    };
    let result = match mode {
        Mode::Container { root } => {
            let mut isolation = standard_isolation();
            isolation.hostname = Some("box".into());
            isolation.root = root.map(Root::new);
            supervisor.supervise(&spec, stop, Some(&isolation))
        }
        Mode::NoPidContainer { churn } => {
            let mut isolation = standard_isolation();
            isolation.namespaces = vec![Namespace::Uts];
            isolation.hostname = Some("box".into());
            let churner = churn.then(ThreadChurn::start);
            let result = supervisor.supervise(&spec, stop, Some(&isolation));
            if let Some(churner) = churner {
                churner.stop();
            }
            result
        }
        Mode::Join(target) => {
            #[cfg(target_os = "linux")]
            let watcher = HostWatcher::start();
            let result = supervisor.supervise_joined(&spec, stop, &Join::standard(target));
            #[cfg(target_os = "linux")]
            if let Some(failed) = watcher.check() {
                return failed;
            }
            result
        }
    };
    match result {
        Ok(status) => ExitCode::from(status.code()),
        Err(err) => fail(&err),
    }
}

fn fail(err: &shipyard_runtime::Error) -> ExitCode {
    eprintln!("error: {err}");
    ExitCode::from(err.exit_code())
}

/// A thread that keeps starting processes and counts any that weren't born
/// in this process's own PID namespace.
///
/// Its children only pause until it has read their namespace from outside,
/// then it kills them: the supervisor reaps every child of this process
/// (it's a subreaper), so they can't run a program and report back.
#[cfg(target_os = "linux")]
struct HostWatcher {
    done: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<Vec<String>>,
}

#[cfg(target_os = "linux")]
impl HostWatcher {
    fn start() -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let own = read_link("/proc/self/ns/pid");
        // A child's PID (from `fork`) only names that child in a `/proc` of
        // our own PID namespace. With another one's (one test runs us that
        // way), the same number is some unrelated process: no evidence.
        let proc_is_ours = read_link("/proc/self") == std::process::id().to_string();
        let thread = std::thread::spawn({
            let done = Arc::clone(&done);
            move || {
                let mut strays = Vec::new();
                if !proc_is_ours {
                    return strays;
                }
                while !done.load(Ordering::Relaxed) {
                    let parent = std::process::id();
                    // SAFETY: the child only makes async-signal-safe calls
                    // until it's killed.
                    let pid = match unsafe { nix::unistd::fork() }.unwrap() {
                        nix::unistd::ForkResult::Child => {
                            // If this process is killed first (a test does
                            // that), take the child with it: a leftover copy
                            // would hold the test's output pipes open.
                            // SAFETY: as above.
                            unsafe {
                                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                                if libc::getppid().cast_unsigned() != parent {
                                    libc::_exit(0);
                                }
                                loop {
                                    libc::pause();
                                }
                            }
                        }
                        nix::unistd::ForkResult::Parent { child } => child,
                    };
                    let born_in = read_link(&format!("/proc/{pid}/ns/pid"));
                    // Empty if it's already gone: no evidence either way.
                    if !born_in.is_empty() && born_in != own {
                        strays.push(born_in);
                    }
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                    // The supervisor may reap it first: then there's nothing to wait for.
                    let _ = nix::sys::wait::waitpid(pid, None);
                }
                strays
            }
        });
        Self { done, thread }
    }

    /// Stops it, and checks that no child of the other thread was born
    /// elsewhere and that this thread's children are born on the host again.
    /// Returns the exit code to fail with, if not.
    fn check(self) -> Option<ExitCode> {
        self.done.store(true, Ordering::Relaxed);
        let strays = self.thread.join().expect("the watcher thread panicked");
        if !strays.is_empty() {
            eprintln!("join: processes another thread started were born elsewhere: {strays:?}");
            return Some(ExitCode::from(98));
        }
        if read_link("/proc/thread-self/ns/pid_for_children") != read_link("/proc/self/ns/pid") {
            eprintln!("join: this thread's children are still born in the container");
            return Some(ExitCode::from(99));
        }
        None
    }
}

/// A thread that keeps starting and ending threads, as busy programs do.
/// The cloned child of a multi-threaded process inherits whatever glibc
/// locks another thread held at that moment (stage 2's setxid deadlock).
struct ThreadChurn {
    done: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl ThreadChurn {
    fn start() -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let done = Arc::clone(&done);
            move || {
                while !done.load(Ordering::Relaxed) {
                    std::thread::spawn(|| {}).join().unwrap();
                }
            }
        });
        Self { done, thread }
    }

    fn stop(self) {
        self.done.store(true, Ordering::Relaxed);
        self.thread.join().unwrap();
    }
}

// --- Helpers ---------------------------------------------------------------

/// This binary, as the "shipyard" the helpers run.
fn me() -> Shipyard {
    static ME: std::sync::LazyLock<Shipyard> = std::sync::LazyLock::new(|| {
        let path = std::env::current_exe().expect("the test binary's path");
        // `Shipyard` holds a `&'static str`: leaked once, for the process.
        Shipyard::at(path.to_str().expect("a UTF-8 path").to_owned().leak())
    });
    *ME
}

/// A running container whose command prints `ready` first.
struct Container {
    running: Running,
    /// Its init, by its PID on the host.
    init: u32,
}

impl Container {
    /// A container running `sh -c SCRIPT`; SCRIPT must print `ready` first.
    fn start(script: &str) -> Self {
        Self::start_with(&[], script)
    }

    /// The same, with the `container` mode's FLAGS (`--rootfs DIR`).
    fn start_with(flags: &[&str], script: &str) -> Self {
        let args = [&["container"], flags, &["sh", "-c", script]].concat();
        let running = start_command(me().command(&args));
        assert_eq!(running.next_line(), "ready");
        let init = u32::try_from(child_of(running.pid())).unwrap();
        Self { running, init }
    }

    /// A container that sleeps until stopped.
    fn sleeping() -> Self {
        Self::start("echo ready; exec sleep 300")
    }

    /// Its init's PID, as the `join` mode's argument.
    fn target(&self) -> String {
        self.init.to_string()
    }
}

/// `join join PID sh -c SCRIPT`.
fn join(container: &Container, script: &str) -> std::process::Command {
    me().command(&["join", &container.target(), "sh", "-c", script])
}

fn read_link(path: &str) -> String {
    std::fs::read_link(path)
        .map(|target| target.to_string_lossy().into_owned())
        .unwrap_or_default()
}

// --- The tests -------------------------------------------------------------

fn sees_the_containers_world() {
    let container = Container::sleeping();
    let joined = run(join(
        &container,
        "hostname; cat /proc/1/comm; echo $$; grep PPid /proc/$$/status; \
         for n in pid mnt uts ipc; do readlink /proc/self/ns/$n; done",
    ));
    assert_eq!(joined.code, Some(0), "{}", joined.stderr);
    let lines: Vec<&str> = joined.stdout.lines().collect();
    assert_eq!(lines.first(), Some(&"box"), "the container's hostname");
    assert_eq!(
        lines.get(1),
        Some(&"shipyard-init"),
        "its /proc: PID 1 is the init"
    );
    let pid: u32 = lines.get(2).and_then(|pid| pid.parse().ok()).unwrap();
    assert!(pid > 2, "an ordinary process inside, not PID {pid}");
    // Its parent (shipyard) is outside the PID namespace.
    assert_eq!(lines.get(3), Some(&"PPid:\t0"));
    let joined_namespaces = lines.get(4..).unwrap_or_default();
    let containers: Vec<String> = ["pid", "mnt", "uts", "ipc"]
        .iter()
        .map(|kind| read_link(&format!("/proc/{}/ns/{kind}", container.init)))
        .collect();
    assert_eq!(joined_namespaces, containers, "the container's namespaces");
}

/// Joining the mount namespace moves the joined command into the
/// container's root filesystem (`setns` changes its root and working
/// directory to the namespace's).
fn sees_the_containers_root_filesystem() {
    let root = RootFs::minimal();
    let container = Container::start_with(&["--rootfs", root.arg()], "echo ready; exec sleep 300");
    let joined = run(join(
        &container,
        "cat /marker; pwd; test -e /etc/passwd || echo no-host",
    ));
    assert_eq!(joined.code, Some(0), "{}", joined.stderr);
    assert_eq!(joined.stdout, format!("{MARKER}/\nno-host\n"));
}

fn results_come_back() {
    let container = Container::sleeping();
    assert_eq!(run(join(&container, "exit 7")).code, Some(7));
    assert_eq!(
        run(join(&container, "kill -USR1 $$")).code,
        Some(128 + libc::SIGUSR1)
    );
    let missing = run(join(&container, "exec /no/such/program"));
    assert_eq!(missing.code, Some(127), "{}", missing.stderr);
    // The container doesn't notice.
    assert!(!gone_soon(container.init), "the container ended");
}

fn a_stop_request_reaches_it() {
    let container = Container::sleeping();
    let joined = start_command(join(&container, "echo ready; exec sleep 100"));
    assert_eq!(joined.next_line(), "ready");
    let started = Instant::now();
    joined.signal(libc::SIGTERM);
    assert_eq!(joined.finish().0, 128 + libc::SIGTERM);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "not at the stop timeout"
    );
}

fn signals_are_forwarded() {
    let container = Container::sleeping();
    let joined = start_command(join(
        &container,
        "trap 'echo got USR1; exit 3' USR1; echo ready; sleep 100 & wait",
    ));
    assert_eq!(joined.next_line(), "ready");
    joined.signal(libc::SIGUSR1);
    let (code, rest) = joined.finish();
    assert_eq!(code, 3);
    assert_eq!(rest, ["got USR1"]);
}

fn ends_with_the_container() {
    let container = Container::sleeping();
    let joined = start_command(join(&container, "echo ready; exec sleep 100"));
    assert_eq!(joined.next_line(), "ready");
    // The container's command exits, its init with it, and the kernel kills
    // everything left in the namespace.
    container.running.signal(libc::SIGTERM);
    assert_eq!(joined.finish().0, 128 + libc::SIGKILL);
}

fn dies_with_shipyard() {
    let container = Container::sleeping();
    let joined = start_command(join(&container, "echo ready; exec sleep 100"));
    assert_eq!(joined.next_line(), "ready");
    let command = child_of(joined.pid());
    joined.signal(libc::SIGKILL);
    assert!(gone(command), "the joined command outlived shipyard");
    assert!(!gone_soon(container.init), "the container ended");
}

fn orphans_belong_to_the_init() {
    let container = Container::sleeping();
    // As with `run`, whatever is left in the command's process group ends
    // with it. A process that left the group (`setsid`) is orphaned instead,
    // to the container's init: the parent its PID namespace gives it.
    let first = run(join(
        &container,
        // `setsid -f` forks, and the child leaves the group before it says
        // its PID; `head` waits for that line. Then the orphan lets go of the
        // output pipe, or the run would wait for it to end.
        "sleep 30 & echo $!; \
         setsid -f sh -c 'echo $$; exec sleep 3 >/dev/null 2>&1' | head -1",
    ));
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    let pids: Vec<&str> = first.stdout.lines().collect();
    let [in_group, orphan] = pids.as_slice() else {
        panic!("expected two PIDs: {}", first.stdout);
    };
    let ended = run(join(&container, &format!("test ! -e /proc/{in_group}")));
    assert_eq!(ended.code, Some(0), "its group's leftover kept running");
    let parent = run(join(
        &container,
        &format!("grep PPid /proc/{orphan}/status"),
    ));
    assert_eq!(
        parent.stdout.trim(),
        "PPid:\t1",
        "re-parented to the container's init"
    );
    // Once it exits, the init reaps it: no zombie (a zombie keeps its /proc
    // entry).
    std::thread::sleep(Duration::from_millis(3500));
    let after = run(join(&container, &format!("test ! -e /proc/{orphan}")));
    assert_eq!(after.code, Some(0), "the orphan was left a zombie");
}

fn no_descriptors_reach_it() {
    let container = Container::sleeping();
    let joined = run(join(&container, "ls -l /proc/$$/fd"));
    assert_eq!(joined.code, Some(0), "{}", joined.stderr);
    for leak in ["pid:[", "mnt:[", "uts:[", "ipc:[", "/proc/"] {
        assert!(
            !joined.stdout.contains(leak),
            "{leak} leaked:\n{}",
            joined.stdout
        );
    }
}

fn joining_an_exited_container() {
    let container = Container::sleeping();
    let pid = container.target();
    container.running.signal(libc::SIGTERM);
    let init = container.init;
    drop(container);
    assert!(gone(libc::pid_t::try_from(init).unwrap()));
    let joined = run(me().command(&["join", &pid, "true"]));
    assert_eq!(joined.code, Some(125));
    assert!(
        joined.stderr.contains(&format!("no process {pid} to join")),
        "{}",
        joined.stderr
    );
}

fn other_threads_unaffected() {
    let container = Container::sleeping();
    // The join mode watches for both (exit codes 98 and 99).
    for _ in 0..20 {
        let joined = run(join(&container, "true"));
        assert_eq!(joined.code, Some(0), "{}", joined.stderr);
    }
}

fn joining_an_exiting_container() {
    let container = Container::sleeping();
    // Its supervisor can't reap the init while stopped, so the init stays
    // a zombie: exited, but its PID namespace not yet gone.
    container.running.signal(libc::SIGSTOP);
    // SAFETY: `kill` only sends a signal; `init` is the container's init.
    unsafe {
        libc::kill(
            libc::pid_t::try_from(container.init).unwrap(),
            libc::SIGKILL,
        )
    };
    std::thread::sleep(Duration::from_millis(300));
    let joined = run(me().command(&["join", &container.target(), "true"]));
    assert_eq!(joined.code, Some(125));
    assert!(joined.stderr.contains("it is exiting"), "{}", joined.stderr);
    container.running.signal(libc::SIGCONT);
    // The init's own status: killed.
    assert_eq!(container.running.finish().0, 128 + libc::SIGKILL);
}

/// The kernel doesn't let a PID namespace's init finish exiting while a
/// process that was started into it from outside is still an unreaped
/// zombie of its outside parent (checked: `scripts/probes/join.c`). So a
/// joiner that can't reap (stopped here) holds the container's end up
/// until it can; then both finish normally.
fn a_stopped_joiner_holds_up_the_end() {
    let container = Container::sleeping();
    let joined = start_command(join(&container, "echo ready; exec sleep 100"));
    assert_eq!(joined.next_line(), "ready");
    joined.signal(libc::SIGSTOP);
    container.running.signal(libc::SIGTERM);
    assert!(
        !gone_soon(container.running.pid().cast_unsigned()),
        "the container ended while the joined process was unreaped"
    );
    joined.signal(libc::SIGCONT);
    assert_eq!(joined.finish().0, 128 + libc::SIGKILL);
    assert_eq!(container.running.finish().0, 128 + libc::SIGTERM);
}

/// In a PID namespace of its own but with the host's /proc, a PID number
/// would name another process: refused rather than guessed.
fn another_namespaces_proc() {
    let container = Container::sleeping();
    let mut command = std::process::Command::new("unshare");
    command.args([
        "--pid",
        "--fork",
        me().path(),
        "join",
        &container.target(),
        "true",
    ]);
    let joined = run(command);
    assert_eq!(joined.code, Some(125), "{}", joined.stderr);
    assert!(
        joined.stderr.contains("another PID namespace"),
        "{}",
        joined.stderr
    );
}

/// Without a PID namespace there's no init: the clean image sets the
/// container up and becomes the command, so the process shipyard reaps is
/// the command itself, and its status comes straight from it.
fn without_a_pid_namespace_the_command_is_shipyards_child() {
    let running = start_command(me().command(&[
        "container-nopid",
        "sh",
        "-c",
        "hostname; echo $PPID; exit 7",
    ]));
    assert_eq!(running.next_line(), "box");
    let parent: u32 = running.next_line().parse().unwrap();
    assert_eq!(
        parent,
        running.pid().cast_unsigned(),
        "the command's parent is shipyard"
    );
    assert_eq!(running.finish().0, 7);
}

/// Many isolated starts while another thread keeps starting and ending
/// threads: the raw clone must never leave the child stuck before its
/// re-exec. Each run has its own deadline (`run`'s timeout).
fn without_a_pid_namespace_under_thread_churn() {
    // Rosetta (x86-64 emulation on Apple silicon) deadlocks inside its own
    // handling of `clone` while threads are being created: the emulator,
    // not shipyard (native runs never hang). Skip there.
    if under_rosetta() {
        eprintln!("under Rosetta emulation, which deadlocks on this stress; skipping");
        return;
    }
    for _ in 0..20 {
        let ran = run(me().command(&["container-nopid", "--churn", "sh", "-c", "exit 7"]));
        assert_eq!(ran.code, Some(7), "{}", ran.stderr);
    }
}

/// Through the dynamic loader, `/proc/self/exe` is the loader, which can't
/// start as the clean image: the start mark never comes, and that's
/// reported, without a PID namespace too.
fn an_init_that_cannot_start_is_reported_without_a_pid_namespace() {
    let Some(loader) = ["/lib/ld-linux-aarch64.so.1", "/lib64/ld-linux-x86-64.so.2"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
    else {
        return;
    };
    let mut command = std::process::Command::new(loader);
    command.args([me().path(), "container-nopid", "true"]);
    let ran = run(command);
    assert_eq!(ran.code, Some(125), "{}", ran.stderr);
    assert!(
        ran.stderr.contains("init failed to start"),
        "{}",
        ran.stderr
    );
}

fn without_root_joining_is_refused() {
    let joined = run(me().command(&["join", "1", "true"]));
    assert_eq!(joined.code, Some(125));
    assert!(joined.stderr.contains("needs root"), "{}", joined.stderr);
}

fn joining_needs_linux() {
    let joined = run(me().command(&["join", "1", "true"]));
    assert_eq!(joined.code, Some(125));
    assert!(
        joined.stderr.contains("requires a Linux kernel"),
        "{}",
        joined.stderr
    );
}

/// Whether `pid` disappears within a second (it shouldn't).
fn gone_soon(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

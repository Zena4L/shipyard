//! Joining a running container's namespaces (`Supervisor::supervise_joined`),
//! end to end, against real Shipyard containers. Linux; root only, except for
//! the "needs root" check.
//!
//! Not on libtest (`harness = false`): this binary is also its own container
//! runtime. It plays the part the CLI will (stage 8+):
//!
//! - `join container CMD…` supervises CMD in a new container (hostname
//!   `box`); the container's init re-execs this binary, so `main` calls
//!   `init_entry_point` first;
//! - `join join PID CMD…` supervises CMD inside PID's namespaces.
//!
//! Both exit with the command's exit code (128 + N for a signal), or 125 and
//! the error on stderr. The tests drive them as subprocesses, one supervisor
//! per process as the runtime requires, and run one at a time.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "in a test crate, a panic is a test failure with a clear message"
)]

use std::io::Write as _;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use shipyard_runtime::isolation::{Isolation, Join};
use shipyard_runtime::process::{self, Env, ProcessSpec, StopPolicy, Supervisor, User};
use shipyard_test_support::{Running, Shipyard, child_of, gone, is_root, run, start_command};

fn main() -> ExitCode {
    process::init_entry_point();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("container") => return serve(Mode::Container, args.get(1..).unwrap_or_default()),
        Some("join") => {
            let Some(target) = args.get(1).and_then(|pid| pid.parse().ok()) else {
                eprintln!("join: the target must be a PID");
                return ExitCode::from(2);
            };
            return serve(Mode::Join(target), args.get(2..).unwrap_or_default());
        }
        _ => {}
    }
    let tests: &[(&str, fn())] = if !cfg!(target_os = "linux") {
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
        ]
    } else {
        &[(
            "without_root_joining_is_refused",
            without_root_joining_is_refused,
        )]
    };
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

// --- The two modes ---------------------------------------------------------

/// What this binary does when it isn't running the tests.
#[derive(Clone, Copy)]
enum Mode {
    /// Supervise a command in a new container.
    Container,
    /// Supervise a command inside this process's namespaces.
    Join(u32),
}

/// Supervises `command` as `mode` says. In join mode it also runs a thread
/// that keeps starting processes meanwhile, which must all stay on the host,
/// and checks afterwards that this thread's children are born on the host
/// again.
fn serve(mode: Mode, command: &[String]) -> ExitCode {
    let mut env = Env::new();
    let user = User::current();
    env.fill_defaults(user.uid);
    let spec = ProcessSpec {
        args: command.iter().map(Into::into).collect(),
        env,
        cwd: "/".into(),
        user,
        rlimits: Vec::new(),
    };
    let stop = StopPolicy {
        timeout: Duration::from_secs(5),
        ..StopPolicy::default()
    };
    let mut supervisor = match Supervisor::install() {
        Ok(supervisor) => supervisor,
        Err(err) => return fail(&err),
    };
    let result = match mode {
        Mode::Container => {
            let mut isolation = Isolation::standard();
            isolation.hostname = Some("box".into());
            supervisor.supervise(&spec, stop, Some(&isolation))
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
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<Vec<String>>,
}

#[cfg(target_os = "linux")]
impl HostWatcher {
    fn start() -> Self {
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let own = read_link("/proc/self/ns/pid");
        // A child's PID (from `fork`) only names that child in a `/proc` of
        // our own PID namespace. With another one's (one test runs us that
        // way), the same number is some unrelated process: no evidence.
        let proc_is_ours = read_link("/proc/self") == std::process::id().to_string();
        let thread = std::thread::spawn({
            let done = std::sync::Arc::clone(&done);
            move || {
                let mut strays = Vec::new();
                if !proc_is_ours {
                    return strays;
                }
                while !done.load(std::sync::atomic::Ordering::Relaxed) {
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
        self.done.store(true, std::sync::atomic::Ordering::Relaxed);
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
        let running = start_command(me().command(&["container", "sh", "-c", script]));
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

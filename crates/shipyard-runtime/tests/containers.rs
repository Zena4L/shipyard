//! Containers through the runtime's API, end to end: joining a running one
//! (`Supervisor::supervise_joined`), and isolation without a PID namespace.
//! Linux; root only, except for the check that joining root's needs root.
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
    // A planned container's monitor is this binary again.
    shipyard_runtime::container::monitor_entry_point();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = serve_if_asked(&args) {
        return code;
    }
    let tests = tests();
    if args.iter().any(|arg| arg == "--list") {
        for (name, _) in &tests {
            println!("{name}: test");
        }
        return ExitCode::SUCCESS;
    }
    println!("\nrunning {} tests", tests.len());
    let mut failed = Vec::new();
    for &(name, test) in &tests {
        print!("test {name} ... ");
        let _ = std::io::stdout().flush();
        // Last resort against a hang: SIGALRM ends this binary.
        // SAFETY: `alarm` has no preconditions.
        unsafe { libc::alarm(90) };
        let passed = std::panic::catch_unwind(test).is_ok();
        println!("{}", if passed { "ok" } else { "FAILED" });
        if !passed {
            failed.push(name);
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

/// The tests this system can run: the root ones as root, and the planned
/// containers' either way.
fn tests() -> Vec<(&'static str, fn())> {
    let mut tests = joining_tests().to_vec();
    if cfg!(target_os = "linux") {
        tests.extend_from_slice(PLANNED);
        #[cfg(target_os = "linux")]
        tests.push((
            "a_new_container_joins_another_s_namespaces",
            a_new_container_joins_another_s_namespaces,
        ));
    }
    tests
}

/// A planned container's tests, as root and without.
const PLANNED: &[(&str, fn())] = &[
    (
        "a_planned_container_runs_its_plan",
        a_planned_container_runs_its_plan,
    ),
    (
        "a_made_bind_source_is_bound_by_descriptor",
        a_made_bind_source_is_bound_by_descriptor,
    ),
    (
        "a_planned_overlay_root_writes_to_its_upper",
        a_planned_overlay_root_writes_to_its_upper,
    ),
    (
        "a_plan_is_checked_as_a_bundle_is",
        a_plan_is_checked_as_a_bundle_is,
    ),
    (
        "exec_runs_as_a_planned_user_by_name",
        exec_runs_as_a_planned_user_by_name,
    ),
    (
        "a_created_containers_init_can_be_followed",
        a_created_containers_init_can_be_followed,
    ),
];

/// The joining tests this system can run: the root ones as root.
fn joining_tests() -> &'static [(&'static str, fn())] {
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
            "without_root_joining_roots_is_refused",
            without_root_joining_roots_is_refused,
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
        #[cfg(target_os = "linux")]
        "planned" => planned(rest),
        _ => return None,
    })
}

/// `planned STATE-ROOT ID ROOTFS [--user NAME] COMMAND…`: creates container
/// ID from a plan (`create_planned`) and starts it, then leaves it to its
/// monitor: for looking at a planned container by hand, with
/// `shipyard-runtime --root STATE-ROOT` (`state`, `exec`, `kill`, `delete`).
#[cfg(target_os = "linux")]
fn planned(args: &[String]) -> ExitCode {
    use shipyard_runtime::container;
    let [state_root, id, rootfs, rest @ ..] = args else {
        eprintln!("usage: planned STATE-ROOT ID ROOTFS [--user NAME] COMMAND…");
        return ExitCode::from(2);
    };
    let (user, command) = match rest {
        [flag, name, command @ ..] if flag == "--user" => (Some(name.clone()), command),
        _ => (None, rest),
    };
    let mut isolation = standard_isolation();
    isolation.root = Some(Root::new(rootfs));
    if !is_root() {
        isolation.namespaces.push(Namespace::User);
        isolation.id_maps = Some(process::rootless_id_maps().maps);
    }
    let mut spec = ProcessSpec::new(command.iter().map(Into::into).collect());
    spec.user.names.user = user;
    let plan = container::Plan::new(spec, isolation);
    let state_root = std::path::Path::new(state_root);
    let created = match container::create_planned(
        state_root,
        id,
        &plan,
        &container::CreateOptions::default(),
    ) {
        Ok(created) => created,
        Err(err) => {
            eprintln!("planned: {err}");
            return ExitCode::FAILURE;
        }
    };
    for warning in &created.warnings {
        eprintln!("planned: warning: {warning}");
    }
    if let Err(err) = container::start(state_root, id) {
        eprintln!("planned: starting {id}: {err}");
        return ExitCode::FAILURE;
    }
    println!(
        "{id}: created from a plan and started (monitor {})",
        created.pid
    );
    ExitCode::SUCCESS
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
    // (`; true`: the shell stays, and `ls` lists its descriptors. Exec'd,
    // `ls` would list its own, with the one it reads `/proc/N/fd` through.)
    let joined = run(join(&container, "ls -l /proc/$$/fd; true"));
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
    // A static test binary (built for musl) names no loader: run through
    // one anyway, it isn't the case this checks.
    let names_a_loader = std::fs::read(me().path()).is_ok_and(|binary| {
        binary
            .windows(8)
            .any(|bytes| bytes == b"/lib/ld-" || bytes == b"lib64/ld")
    });
    if !names_a_loader {
        eprintln!("skipping: a static test binary has no dynamic loader to run it through");
        return;
    }
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

/// Without root, the host's init (root's) can't be joined: its namespaces
/// aren't this user's (a rootless container's would be).
fn without_root_joining_roots_is_refused() {
    let joined = run(me().command(&["join", "1", "true"]));
    assert_eq!(joined.code, Some(125));
    assert!(
        joined.stderr.contains("aren't this user's to join"),
        "{}",
        joined.stderr
    );
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

// --- Planned containers (`create_planned`) ---------------------------------

/// A state root and a directory the container writes its results to, for
/// one test; removed at the end.
struct PlanTest {
    dir: std::path::PathBuf,
}

impl PlanTest {
    fn new(name: &str) -> Self {
        use std::os::unix::fs::DirBuilderExt as _;
        let dir =
            std::env::temp_dir().join(format!("shipyard-planned-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir.join("state"))
            .unwrap();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        // Rootless, the container's root is one of the caller's subordinate
        // IDs: it may write here only if anyone may.
        open_to_all(&dir.join("out"));
        Self { dir }
    }

    fn state_root(&self) -> std::path::PathBuf {
        self.dir.join("state")
    }

    /// What the container wrote to `/out/NAME`.
    fn out(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join("out").join(name)).unwrap_or_default()
    }

    /// `script` in `root`, with this test's `/out`, as a plan.
    fn plan(&self, mut root: Root, script: &str) -> shipyard_runtime::container::Plan {
        root.mounts.push(shipyard_runtime::isolation::Mount::bind(
            self.dir.join("out"),
            "/out",
        ));
        let mut isolation = standard_isolation();
        isolation.root = Some(root);
        // A plan says everything: rootless, its own user namespace too.
        #[cfg(target_os = "linux")]
        if !is_root() {
            isolation
                .namespaces
                .push(shipyard_runtime::isolation::Namespace::User);
            isolation.id_maps = Some(shipyard_runtime::process::rootless_id_maps().maps);
        }
        let spec = ProcessSpec::new(vec!["sh".into(), "-c".into(), script.into()]);
        shipyard_runtime::container::Plan::new(spec, isolation)
    }

    /// Creates `id` from `plan`, starts it, and waits for its end.
    fn run(&self, id: &str, plan: &shipyard_runtime::container::Plan) {
        self.run_and(id, plan, None::<fn(&std::path::Path)>);
    }

    /// [`run`](Self::run), calling `running` once the command has written
    /// `/out/started` (which it must), while it runs.
    fn run_watching(
        &self,
        id: &str,
        plan: &shipyard_runtime::container::Plan,
        running: impl FnOnce(&std::path::Path),
    ) {
        self.run_and(id, plan, Some(running));
    }

    fn run_and(
        &self,
        id: &str,
        plan: &shipyard_runtime::container::Plan,
        running: Option<impl FnOnce(&std::path::Path)>,
    ) {
        use shipyard_runtime::container;
        let root = self.state_root();
        let options = container::CreateOptions::default();
        container::create_planned(&root, id, plan, &options).unwrap();
        let state = container::state(&root, id).unwrap();
        assert_eq!(state.status, container::Status::Created);
        assert_eq!(
            state.bundle,
            root.join(id).canonicalize().unwrap(),
            "its own directory"
        );
        container::start(&root, id).unwrap();
        let started = self.dir.join("out/started");
        let deadline = Instant::now() + Duration::from_secs(5);
        while !started.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Some(running) = running {
            assert!(started.exists(), "{id} never wrote /out/started");
            running(&root);
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while container::exit_record(&root, id).unwrap().is_none() {
            assert!(Instant::now() < deadline, "{id} never ended");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Makes `dir` writable by anyone (a rootless container's mapped IDs).
fn open_to_all(dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777)).unwrap();
}

impl Drop for PlanTest {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A bind of a source Shipyard made (step 11.7) is reached from its
/// directory without following a symlink, whatever the engine checked
/// before: bound while it's a directory (the control: what's written there
/// is on the host), and refused once a symlink took its place, its target
/// untouched. This is the runtime's half alone (the engine's check of each
/// start is `a_made_bind_source_is_never_reached_through_a_symlink`).
fn a_made_bind_source_is_bound_by_descriptor() {
    use shipyard_runtime::container;
    let test = PlanTest::new("made");
    let rootfs = RootFs::minimal();
    let shared = test.dir.join("shared");
    std::fs::create_dir_all(shared.join("made")).unwrap();
    open_to_all(&shared.join("made"));
    let with_made = |root: &mut Root| {
        root.mounts
            .push(shipyard_runtime::isolation::Mount::bind_made(
                &shared,
                std::path::Path::new("made"),
                "/m",
            ));
    };
    let mut root = Root::new(rootfs.path());
    with_made(&mut root);
    let plan = test.plan(root, "echo here > /m/note; touch /out/started");
    test.run("made", &plan);
    assert_eq!(
        std::fs::read_to_string(shared.join("made/note")).unwrap(),
        "here\n"
    );

    let outside = test.dir.join("outside");
    std::fs::create_dir(&outside).unwrap();
    open_to_all(&outside);
    std::fs::remove_dir_all(shared.join("made")).unwrap();
    std::os::unix::fs::symlink(&outside, shared.join("made")).unwrap();
    let mut root = Root::new(rootfs.path());
    with_made(&mut root);
    let plan = test.plan(root, "echo through > /m/note; touch /out/started");
    let state = test.state_root();
    let made = container::create_planned(
        &state,
        "through",
        &plan,
        &container::CreateOptions::default(),
    )
    .and_then(|_| container::start(&state, "through"));
    // A failure of its setup (the monitor's: no finer kind reaches the
    // caller, F135), not of anything before it.
    assert!(
        matches!(made, Err(container::ContainerError::Setup(_))),
        "a symlink in a made source's place was bound: {made:?}"
    );
    assert!(!outside.join("note").exists(), "written through the link");
}

/// A container from a plan runs what the plan says, in its root, and ends
/// with its code; its plan is beside its record. `exec` joins it, confined
/// as the plan's process (the monitor's `exec.json` from the plan).
fn a_planned_container_runs_its_plan() {
    use shipyard_runtime::container::{self, ExecOptions, ExecOutcome};
    let test = PlanTest::new("plain");
    let rootfs = RootFs::minimal();
    let plan = test.plan(
        Root::new(rootfs.path()),
        "cat /marker > /out/seen; touch /out/started; sleep 2; exit 3",
    );
    test.run_watching("plain", &plan, |root| {
        let state = container::state(root, "plain").unwrap();
        assert_eq!(state.status, container::Status::Running);
        let options = ExecOptions::new(vec![
            "sh".into(),
            "-c".into(),
            "cat /marker > /out/exec".into(),
        ]);
        let outcome = container::exec(root, "plain", &options, None).unwrap();
        assert!(
            matches!(outcome, ExecOutcome::Exited(process::ExitStatus::Exited(0))),
            "{outcome:?}"
        );
    });
    assert_eq!(test.out("seen"), MARKER);
    assert_eq!(test.out("exec"), MARKER, "exec ran in the plan's root");
    let root = test.state_root();
    let ended = shipyard_runtime::container::exit_record(&root, "plain")
        .unwrap()
        .unwrap();
    assert_eq!(ended.code, Some(3), "{ended:?}");
    assert!(root.join("plain/plan.json").is_file());
    shipyard_runtime::container::delete(&root, "plain", false).unwrap();
    assert!(!root.join("plain").exists());
}

/// This process's children (from `/proc`: any, living or a zombie).
fn my_children() -> Vec<i32> {
    let me = i32::try_from(std::process::id()).unwrap();
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            (shipyard_sys::proc::stat(pid)?.ppid == me).then_some(pid)
        })
        .collect()
}

/// A created container's init, for a caller to follow (pasta, detached):
/// its PID and start time, and a pidfd that's readable once it has ended;
/// refused once the container has started. `create` leaves no child of
/// this process behind (the monitor's double fork).
fn a_created_containers_init_can_be_followed() {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use shipyard_runtime::container;
    let test = PlanTest::new("follow");
    let rootfs = RootFs::minimal();
    let plan = test.plan(Root::new(rootfs.path()), "sleep 30");
    let root = test.state_root();
    let options = container::CreateOptions::default();
    // Not a subreaper meanwhile (the joining tests before made this one):
    // a subreaper adopts the orphaned monitor, as init would.
    #[cfg(target_os = "linux")]
    let subreaper = nix::sys::prctl::get_child_subreaper().unwrap();
    #[cfg(target_os = "linux")]
    nix::sys::prctl::set_child_subreaper(false).unwrap();
    let before = my_children();
    container::create_planned(&root, "init-followed", &plan, &options).unwrap();
    let after = my_children();
    #[cfg(target_os = "linux")]
    nix::sys::prctl::set_child_subreaper(subreaper).unwrap();
    assert_eq!(after, before, "create left a child");
    let init = container::init_process(&root, "init-followed").unwrap();
    assert_eq!(
        Some(init.pid),
        container::state(&root, "init-followed").unwrap().pid
    );
    assert_ne!(init.start_time, 0);
    let pidfd = init.pidfd.expect("a pidfd (Linux 5.3)");
    let readable = |wait: u16| {
        let mut fds = [PollFd::new(
            std::os::fd::AsFd::as_fd(&pidfd),
            PollFlags::POLLIN,
        )];
        poll(&mut fds, PollTimeout::from(wait)).unwrap() == 1
    };
    assert!(!readable(0), "readable while the init runs");
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(init.pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    assert!(readable(5000), "not readable once the init has ended");
    container::delete(&root, "init-followed", true).unwrap();

    // Started: no longer the caller's to follow.
    container::create_planned(&root, "init-started", &plan, &options).unwrap();
    container::start(&root, "init-started").unwrap();
    assert!(container::init_process(&root, "init-started").is_err());
    container::delete(&root, "init-started", true).unwrap();
}

/// A plan's user by name (an image's `USER`): the container's process runs
/// as it, from the container's own `/etc/passwd` and `/etc/group`, and so
/// does `exec`'s command, with its groups and `HOME`.
fn exec_runs_as_a_planned_user_by_name() {
    use shipyard_runtime::container::{self, ExecOptions, ExecOutcome};
    let test = PlanTest::new("named");
    let rootfs = RootFs::minimal();
    let etc = rootfs.path().join("etc");
    std::fs::create_dir_all(&etc).unwrap();
    std::fs::write(etc.join("passwd"), "app:x:4321:4322::/home/app:/bin/sh\n").unwrap();
    std::fs::write(etc.join("group"), "app:x:4322:\nextra:x:4323:app\n").unwrap();
    let mut plan = test.plan(
        Root::new(rootfs.path()),
        "cat /proc/self/status > /out/own; touch /out/started; sleep 2",
    );
    plan.spec.user.names.user = Some("app".to_owned());
    plan.spec.user.names.memberships = true;
    test.run_watching("named", &plan, |root| {
        let options = ExecOptions::new(vec![
            "sh".into(),
            "-c".into(),
            "cat /proc/self/status > /out/exec; echo \"$HOME\" > /out/home".into(),
        ]);
        let outcome = container::exec(root, "named", &options, None).unwrap();
        assert!(
            matches!(outcome, ExecOutcome::Exited(process::ExitStatus::Exited(0))),
            "{outcome:?}"
        );
    });
    let ids = |status: &str, key: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .map(|ids| ids.split_whitespace().next().unwrap_or_default().to_owned())
            .unwrap_or_default()
    };
    for file in ["own", "exec"] {
        let status = test.out(file);
        assert_eq!(ids(&status, "Uid:"), "4321", "{file}: {status}");
        assert_eq!(ids(&status, "Gid:"), "4322", "{file}: {status}");
        let groups = status
            .lines()
            .find_map(|line| line.strip_prefix("Groups:"))
            .unwrap_or_default();
        assert!(
            groups.split_whitespace().any(|gid| gid == "4323"),
            "{file}: {status}"
        );
    }
    assert_eq!(test.out("home").trim(), "/home/app");
    container::delete(&test.state_root(), "named", false).unwrap();
}

/// An image's root as a plan has it: an overlay of a lower directory, with
/// the container's writes in its upper one, the lower left as it was.
fn a_planned_overlay_root_writes_to_its_upper() {
    let test = PlanTest::new("overlay");
    let lower = RootFs::minimal();
    let area = test.dir.join("area");
    for dir in ["upper", "work", "merged"] {
        std::fs::create_dir_all(area.join(dir)).unwrap();
        open_to_all(&area.join(dir));
    }
    let mut root = Root::new(area.join("merged"));
    root.overlay = Some(shipyard_runtime::isolation::Overlay::new(
        vec![lower.path().to_owned()],
        area.join("upper"),
        area.join("work"),
    ));
    let plan = test.plan(root, "cat /marker > /out/seen; echo new > /made");
    test.run("overlay", &plan);
    assert_eq!(test.out("seen"), MARKER, "the lower's files");
    assert_eq!(
        std::fs::read_to_string(area.join("upper/made")).unwrap(),
        "new\n",
        "the write is in the upper"
    );
    assert!(
        !lower.path().join("made").exists(),
        "the lower is untouched"
    );
    shipyard_runtime::container::delete(&test.state_root(), "overlay", false).unwrap();
}

/// A plan gets a bundle's checks before anything is made: a terminal with
/// no console socket, or without a mount namespace; a hostname without a
/// UTS namespace; a cgroup file a bundle couldn't name (`unified`); a cgroup
/// mount without a cgroup namespace; a seccomp profile that can't be used;
/// no root filesystem; and rootless, no user namespace of its own. The
/// same plan with none of these mistakes is created (the control).
fn a_plan_is_checked_as_a_bundle_is() {
    use shipyard_runtime::container::{self, ContainerError, PlanError};
    use shipyard_runtime::isolation::{IsolationError, Namespace};
    let test = PlanTest::new("checked");
    let rootfs = RootFs::minimal();
    let create = |plan: &container::Plan| {
        container::create_planned(
            &test.state_root(),
            "checked",
            plan,
            &container::CreateOptions::default(),
        )
    };
    let refused = |plan: &container::Plan, kind: fn(&ContainerError) -> bool| {
        let err = create(plan).unwrap_err();
        assert!(!test.state_root().join("checked").exists(), "{err}");
        assert!(kind(&err), "{err}");
    };
    let good = || test.plan(Root::new(rootfs.path()), "true");
    let plan_without = |namespace: Namespace| {
        let mut plan = good();
        plan.isolation.namespaces.retain(|kept| *kept != namespace);
        plan
    };
    // The control: created, then deleted unstarted.
    create(&good()).unwrap();
    container::delete(&test.state_root(), "checked", true).unwrap();
    // Whether this host gives a container a cgroup (CI's and the test
    // container's do; a user outside a delegated cgroup doesn't).
    #[cfg(target_os = "linux")]
    let gets_a_cgroup = shipyard_runtime::cgroup::Tree::find().is_ok();
    #[cfg(not(target_os = "linux"))]
    let gets_a_cgroup = false;
    let mut plan = good();
    plan.terminal = Some(shipyard_runtime::oci::BundleTerminal::default());
    refused(&plan, |err| {
        matches!(err, ContainerError::TerminalWithoutConsoleSocket)
    });
    let mut plan = plan_without(Namespace::Mount);
    plan.isolation.root = good().isolation.root;
    plan.terminal = Some(shipyard_runtime::oci::BundleTerminal::default());
    refused(&plan, |err| {
        matches!(
            err,
            ContainerError::InvalidPlan(PlanError::TerminalWithoutMount)
        )
    });
    let mut plan = plan_without(Namespace::Uts);
    plan.isolation.hostname = Some("box".to_owned());
    refused(&plan, |err| {
        matches!(
            err,
            ContainerError::InvalidPlan(PlanError::Isolation(IsolationError::HostnameNeedsUts))
        )
    });
    let mut plan = good();
    plan.isolation
        .resources
        .unified
        .insert("../../../etc/x".to_owned(), "1".to_owned());
    refused(&plan, |err| {
        matches!(err, ContainerError::InvalidPlan(PlanError::Resources(_)))
    });
    // Where the container gets a cgroup, its mount is kept, and needs the
    // namespace; where it can't, the mount is left out (as a bundle's), and
    // the plan runs.
    if gets_a_cgroup {
        refused(&plan_without(Namespace::Cgroup), |err| {
            matches!(
                err,
                ContainerError::InvalidPlan(PlanError::Isolation(
                    IsolationError::CgroupMountNeedsCgroup
                ))
            )
        });
    } else {
        create(&plan_without(Namespace::Cgroup)).unwrap();
        container::delete(&test.state_root(), "checked", true).unwrap();
    }
    let mut plan = good();
    plan.isolation.security.seccomp = shipyard_runtime::security::Seccomp::Profile(Box::new(
        serde_json::from_str(r#"{"defaultAction":"SCMP_ACT_ALLOW","listenerPath":"/x"}"#).unwrap(),
    ));
    refused(&plan, |err| {
        matches!(err, ContainerError::InvalidPlan(PlanError::Seccomp(_)))
    });
    let mut plan = good();
    plan.isolation.root = None;
    refused(&plan, |err| {
        matches!(err, ContainerError::InvalidPlan(PlanError::NoRoot))
    });
    if !is_root() {
        let mut plan = plan_without(Namespace::User);
        plan.isolation.id_maps = None;
        refused(&plan, |err| {
            matches!(
                err,
                ContainerError::InvalidPlan(PlanError::NeedsUserNamespace)
            )
        });
    }
}

#[cfg(target_os = "linux")]
/// The namespace of `kind` of process `pid`, as `readlink` names it
/// (`net:[INODE]`).
fn namespace_of(pid: impl std::fmt::Display, kind: &str) -> String {
    std::fs::read_link(format!("/proc/{pid}/ns/{kind}"))
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[cfg(target_os = "linux")]
/// A new container joins a running one's network and IPC namespaces (by
/// its init's PID and start time) and its UTS namespace (by path), and
/// makes the rest its own; its monitor goes back to its own namespaces
/// after the clone. A start time that isn't the init's is refused (a
/// reused PID), as is a container that has ended. Rootless, the other's
/// namespaces are its user namespace's: that's joined too, with its maps,
/// by a process of its own (the monitor stays in its own).
fn a_new_container_joins_another_s_namespaces() {
    use shipyard_runtime::container;
    use shipyard_runtime::isolation::JoinSource;
    let test = PlanTest::new("joins");
    let rootfs = RootFs::minimal();
    let root = test.state_root();
    let options = container::CreateOptions::default();
    let mut first = test.plan(
        Root::new(rootfs.path()),
        "echo from-a > /dev/shm/marker; sleep 60",
    );
    first.isolation.namespaces.push(Namespace::Network);
    container::create_planned(&root, "first", &first, &options).unwrap();
    container::start(&root, "first").unwrap();
    let (pid, start_time) = container::running_init(&root, "first").unwrap();
    let process = JoinSource::Process { pid, start_time };
    // Rootless, the first's namespaces are its user namespace's: that's
    // joined too (with its maps, the plan's), and no path beside it.
    let rootless = !is_root();
    let joined: &[&str] = if rootless {
        &["net", "ipc", "user"]
    } else {
        &["net", "ipc", "uts"]
    };
    let second = |from: JoinSource| joining_plan(&test, &rootfs, from);
    test.run_and("second", &second(process), None::<fn(&std::path::Path)>);
    if rootless {
        assert_eq!(
            test.out("uid_map"),
            std::fs::read_to_string(format!("/proc/{pid}/uid_map")).unwrap(),
            "the first's maps"
        );
    }
    for &kind in joined {
        assert_eq!(
            test.out(kind).trim(),
            namespace_of(pid, kind),
            "{kind}: the first's"
        );
    }
    // Its `/dev/shm` is the first's, both ways.
    assert_eq!(test.out("shm"), "from-a\n", "the first's shared memory");
    assert_eq!(
        std::fs::read_to_string(format!("/proc/{pid}/root/dev/shm/back")).unwrap(),
        "from-b\n"
    );
    // The controls: the first's network isn't the host's, and the second
    // made its own mount namespace.
    assert_ne!(namespace_of(pid, "net"), namespace_of("self", "net"));
    assert_ne!(test.out("mnt").trim(), namespace_of(pid, "mnt"));
    assert!(test.out("mnt").starts_with("mnt:["), "{}", test.out("mnt"));
    container::delete(&root, "second", true).unwrap();

    // As root, a container with a user namespace of its own joins the
    // first's network, but not its IPC namespace: that one's shared memory
    // is cloned in the first's mount namespace, where its own user namespace
    // has no privileges.
    if !rootless {
        let mut own_user = second(JoinSource::Process { pid, start_time });
        own_user.isolation.namespaces.push(Namespace::User);
        own_user.isolation.id_maps = Some(shipyard_runtime::isolation::IdMaps::single((0, 0)));
        own_user
            .isolation
            .joins
            .retain(|join| join.kind != Namespace::Uts);
        let refused =
            container::create_planned(&root, "own-user", &own_user, &options).unwrap_err();
        assert!(
            matches!(
                refused,
                container::ContainerError::InvalidPlan(container::PlanError::Isolation(
                    shipyard_runtime::isolation::IsolationError::SharedMemoryOfAnotherUserNamespace
                ))
            ),
            "{refused}"
        );
        own_user
            .isolation
            .joins
            .retain(|join| join.kind != Namespace::Ipc);
        std::fs::remove_file(test.dir.join("out/net")).unwrap();
        test.run_and("own-user", &own_user, None::<fn(&std::path::Path)>);
        assert_eq!(
            test.out("net").trim(),
            namespace_of(pid, "net"),
            "the first's"
        );
        assert_ne!(
            test.out("user").trim(),
            namespace_of(pid, "user"),
            "its own"
        );
        container::delete(&root, "own-user", true).unwrap();
    }

    // Its monitor (still there, until the container's deleted) went back
    // to its own namespaces after the clone.
    let mut held = second(JoinSource::Process { pid, start_time });
    held.spec = ProcessSpec::new(vec!["sleep".into(), "60".into()]);
    container::create_planned(&root, "held", &held, &options).unwrap();
    let (monitor, _) = container::peek_monitor(&root, "held").unwrap().unwrap();
    for &kind in joined {
        assert_eq!(
            namespace_of(monitor, kind),
            namespace_of("self", kind),
            "the monitor's {kind}"
        );
    }
    container::delete(&root, "held", true).unwrap();

    a_gone_process_is_never_joined(&test, &rootfs, (pid, start_time));
}

#[cfg(target_os = "linux")]
/// A helper of [`a_new_container_joins_another_s_namespaces`], not a test
/// of its own: its refusals. Maps that aren't the joined user namespace's
/// (rootless), a path that isn't a namespace of its kind (root), a start
/// time that isn't the running `first`'s init's (`pid`, `start_time`), then
/// the first ended.
fn a_gone_process_is_never_joined(test: &PlanTest, rootfs: &RootFs, (pid, start_time): (u32, u64)) {
    use shipyard_runtime::container;
    use shipyard_runtime::isolation::JoinSource;
    let root = test.state_root();
    let options = container::CreateOptions::default();
    let second = |from| joining_plan(test, rootfs, from);
    // Rootless, maps that aren't the joined user namespace's: refused (the
    // setup would go by the wrong ones).
    let mut other_maps = second(JoinSource::Process { pid, start_time });
    let uid = nix::unistd::getuid().as_raw();
    let single = Some(shipyard_runtime::isolation::IdMaps::single((uid, uid)));
    // (Where the first's own maps are that one ID, there's no other to give;
    // the test container's tester has subordinate IDs.)
    if std::env::var("SHIPYARD_TEST_ROOTLESS").as_deref() == Ok("required") {
        assert_ne!(
            other_maps.isolation.id_maps, single,
            "no other maps to give"
        );
    }
    if !is_root() && other_maps.isolation.id_maps != single {
        other_maps.isolation.id_maps = single;
        let err = container::create_planned(&root, "maps", &other_maps, &options).unwrap_err();
        assert!(
            err.to_string()
                .contains("user namespace has other ID maps than the container was given"),
            "{err}"
        );
        assert!(!root.join("maps").exists());
    }
    // As root, an IPC namespace by a path that's another kind's, or no
    // namespace at all: refused, nothing left.
    if is_root() {
        for (path, said) in [
            (format!("/proc/{pid}/ns/net"), "it isn't an IPC namespace"),
            ("/dev/null".to_owned(), "it isn't a namespace"),
        ] {
            let mut plan = second(JoinSource::Process { pid, start_time });
            plan.isolation
                .joins
                .retain(|join| join.kind != Namespace::Ipc);
            plan.isolation
                .joins
                .push(shipyard_runtime::isolation::NamespaceJoin::new(
                    Namespace::Ipc,
                    JoinSource::Path(path.into()),
                ));
            let err = container::create_planned(&root, "kind", &plan, &options).unwrap_err();
            assert!(err.to_string().contains(said), "{err}");
            assert!(!root.join("kind").exists());
        }
    }
    // A PID that's no longer the init's: refused, nothing left.
    let reused = second(JoinSource::Process {
        pid,
        start_time: start_time + 1,
    });
    let err = container::create_planned(&root, "reused", &reused, &options).unwrap_err();
    assert!(
        err.to_string()
            .contains(&format!("no process {pid} to join")),
        "{err}"
    );
    assert!(!root.join("reused").exists());

    // Ended: not running, so nothing to name.
    container::kill(&root, "first", process::Signal::KILL).unwrap();
    shipyard_test_support::eventually(Duration::from_secs(10), "the first's end", || {
        container::exit_record(&root, "first").unwrap().is_some()
    });
    assert!(matches!(
        container::running_init(&root, "first"),
        Err(container::ContainerError::WrongStatus { .. })
    ));
    let err = container::create_planned(
        &root,
        "late",
        &second(JoinSource::Process { pid, start_time }),
        &options,
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains(&format!("no process {pid} to join")),
        "{err}"
    );
    container::delete(&root, "first", true).unwrap();
}

#[cfg(target_os = "linux")]
/// A plan that joins `from`'s network and IPC namespaces, and (rootless)
/// its user namespace, or (root) its UTS namespace by path, and says which
/// namespaces it's in.
fn joining_plan(
    test: &PlanTest,
    rootfs: &RootFs,
    from: shipyard_runtime::isolation::JoinSource,
) -> shipyard_runtime::container::Plan {
    use shipyard_runtime::isolation::{JoinSource, NamespaceJoin};
    let mut plan = test.plan(
        Root::new(rootfs.path()),
        "for kind in net ipc uts mnt user; do readlink /proc/self/ns/$kind > /out/$kind; done; \
         cat /proc/self/uid_map > /out/uid_map; cat /dev/shm/marker > /out/shm; \
         echo from-b > /dev/shm/back; touch /out/started",
    );
    plan.isolation.namespaces.push(Namespace::Network);
    plan.isolation.joins = vec![
        NamespaceJoin::new(Namespace::Network, from.clone()),
        NamespaceJoin::new(Namespace::Ipc, from.clone()),
    ];
    if !is_root() {
        plan.isolation
            .joins
            .push(NamespaceJoin::new(Namespace::User, from));
    } else if let JoinSource::Process { pid, .. } = from {
        plan.isolation.joins.push(NamespaceJoin::new(
            Namespace::Uts,
            JoinSource::Path(format!("/proc/{pid}/ns/uts").into()),
        ));
    }
    plan
}

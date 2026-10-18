//! PID 1 of the macOS guest (step 10.3a). It runs twice:
//!
//! 1. As the kernel's `/init`, from the initramfs: it copies the root into a
//!    tmpfs, frees the initramfs's copy, moves the tmpfs onto `/`
//!    (`MS_MOVE`), `chroot`s into it and re-executes itself. The
//!    initramfs's root is the kernel's first mount, which `pivot_root`
//!    refuses to move (`EINVAL`), and every container's start pivots.
//! 2. With `moved`: it mounts what the engine needs, loads the kernel's
//!    modules, prepares the data disk and the shared home, settles the
//!    containers the last boot left, starts the agent, then reaps and waits
//!    for its stop (SIGTERM, SIGINT or SIGPWR), when it stops the
//!    containers and powers off.
//!
//! It never returns: every failure is said on the console and powers the
//! VM off (a kernel panic would reboot it in place instead).

use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::mount::{MsFlags, mount};
use nix::sys::signal::{SigSet, SigmaskHow, Signal, sigprocmask};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use shipyard_runtime::attach::forward;

use super::disk::{self, Found, Step};

/// The second run's argument: the root is a tmpfs by now.
const MOVED: &str = "moved";

/// Where the tmpfs is made, under the initramfs's root, before it's moved.
const NEW_ROOT: &str = "/.shipyard-root";

/// This binary, in the guest.
const SHIPYARD: &str = "/usr/bin/shipyard";

/// The data disk (the VM's first virtio block device) and where it goes.
const DATA_DISK: &str = "/dev/vda";
pub(super) const DATA: &str = "/var/lib/shipyard";

/// Modules the engine can't work without, in their load order (the
/// kernel loads what each needs first, and the rest on demand: `nf_tables`'
/// expressions, conntrack).
const REQUIRED_MODULES: &[&str] = &[
    "virtio_blk",
    "ext4",
    "virtio_net",
    "virtiofs",
    "vsock",
    "vmw_vsock_virtio_transport",
    "overlay",
    "nf_tables",
    "bridge",
    "veth",
];

/// Modules that help but aren't needed: their failure is said, not fatal.
const OPTIONAL_MODULES: &[&str] = &["virtio_rng", "virtio_balloon"];

/// How long the containers are given to stop, and the recovery to run.
const SETTLE: Duration = Duration::from_secs(60);

/// PID 1's run: never returns.
pub(super) fn run() -> ! {
    if std::env::args().nth(2).as_deref() != Some(MOVED) {
        power_off(&move_root());
    }
    match boot() {
        Ok(agent) => serve(agent.id()),
        Err(error) => power_off(&error),
    }
}

/// Says `line` on the console (PID 1's stderr is the kernel's console).
fn say(line: &str) {
    let _ = writeln!(std::io::stderr(), "shipyard-guest: {line}");
}

/// Says why, then powers the VM off: the VM host sees it stop.
fn power_off(why: &str) -> ! {
    say(&format!("{why}; powering off"));
    sync_data();
    let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_POWER_OFF);
    // (Reboot only returns on failure; PID 1 may not exit.)
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// Stage 1: the root into a tmpfs, then this binary again, there. Returns
/// only on failure: why.
fn move_root() -> String {
    match moved_root() {
        Ok(error) | Err(error) => error,
    }
}

/// [`move_root`]'s steps: `Ok` with the exec's failure, `Err` with a step's.
fn moved_root() -> Result<String, String> {
    let new_root = Path::new(NEW_ROOT);
    std::fs::create_dir_all(new_root).map_err(|e| format!("making {NEW_ROOT}: {e}"))?;
    mount(
        Some("tmpfs"),
        new_root,
        Some("tmpfs"),
        MsFlags::empty(),
        Some("mode=0755"),
    )
    .map_err(|e| format!("mounting a tmpfs for the root: {e}"))?;
    copy_tree(Path::new("/"), new_root)?;
    // The initramfs's copy freed (it's memory): everything but the new
    // root. What can't go is left, said.
    for entry in std::fs::read_dir("/").map_err(|e| format!("reading /: {e}"))? {
        let path = entry.map_err(|e| format!("reading /: {e}"))?.path();
        if path != new_root {
            let _ = remove(&path);
        }
    }
    std::env::set_current_dir(new_root).map_err(|e| format!("entering the new root: {e}"))?;
    mount(Some("."), "/", None::<&str>, MsFlags::MS_MOVE, None::<&str>)
        .map_err(|e| format!("moving the new root onto /: {e}"))?;
    nix::unistd::chroot(".").map_err(|e| format!("chroot into the new root: {e}"))?;
    std::env::set_current_dir("/").map_err(|e| format!("entering /: {e}"))?;
    let error = Command::new(SHIPYARD).args([super::INIT_ARG, MOVED]).exec();
    Ok(format!("running {SHIPYARD} again: {error}"))
}

/// Copies the tree at `from` into `to` (directories, files and links, with
/// their modes), leaving out `to` itself and the kernel's file systems.
fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let failed = |what: &Path, e: std::io::Error| format!("copying {}: {e}", what.display());
    for entry in std::fs::read_dir(from).map_err(|e| failed(from, e))? {
        let entry = entry.map_err(|e| failed(from, e))?;
        let source = entry.path();
        if source == Path::new(NEW_ROOT)
            || source == Path::new("/proc")
            || source == Path::new("/sys")
        {
            continue;
        }
        let target = to.join(entry.file_name());
        let kind = entry.file_type().map_err(|e| failed(&source, e))?;
        if kind.is_dir() {
            let mode = entry
                .metadata()
                .map_err(|e| failed(&source, e))?
                .permissions()
                .mode();
            std::fs::create_dir(&target).map_err(|e| failed(&target, e))?;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))
                .map_err(|e| failed(&target, e))?;
            copy_tree(&source, &target)?;
        } else if kind.is_symlink() {
            let link = std::fs::read_link(&source).map_err(|e| failed(&source, e))?;
            symlink(link, &target).map_err(|e| failed(&target, e))?;
        } else if kind.is_file() {
            std::fs::copy(&source, &target).map_err(|e| failed(&source, e))?;
        }
        // (Device nodes: /dev gets a devtmpfs of its own in stage 2.)
    }
    Ok(())
}

/// Removes `path`, a file or a tree (not across mounts: the new root is
/// skipped by the caller).
fn remove(path: &Path) -> std::io::Result<()> {
    if path.symlink_metadata()?.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Stage 2: the system set up, the agent started (refusing requests until
/// the guest is ready), the data disk and the home, the guest made ready;
/// then the containers the last boot left settled in the background.
fn boot() -> Result<Child, String> {
    // Blocked first: a child's end (the agent's, early) or a stop that
    // comes while booting waits for `serve` instead of being lost (PID 1
    // has no handler: the kernel drops a signal it doesn't block).
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&waited_for()), None)
        .map_err(|e| format!("blocking signals: {e}"))?;
    mount_kernel_file_systems()?;
    let _ = nix::unistd::sethostname("shipyard");
    load_modules()?;
    network();
    // The agent first: until the guest is ready it refuses every request
    // at once, saying why, so a long check or repair of the data disk
    // never runs against the host's boot bound (D10.4-3).
    let agent = start_agent()?;
    data_disk()?;
    home();
    std::fs::write(super::READY, b"").map_err(|e| format!("making {}: {e}", super::READY))?;
    // Containers the last boot left (killed with the VM, or stopped), as a
    // Linux host's boot unit settles them: after the agent, so a slow
    // recovery doesn't count against the VM's boot (`serve` reaps it).
    if let Err(error) = Command::new(SHIPYARD)
        .args(["system", "recover"])
        .stdin(Stdio::null())
        .spawn_unblocked()
    {
        say(&format!("shipyard system recover: {error}"));
    }
    Ok(agent)
}

/// The file systems the engine and its containers expect.
fn mount_kernel_file_systems() -> Result<(), String> {
    let mounts: &[(&str, &str, &str, MsFlags, Option<&str>)] = &[
        ("proc", "/proc", "proc", MsFlags::empty(), None),
        ("sysfs", "/sys", "sysfs", MsFlags::empty(), None),
        (
            "devtmpfs",
            "/dev",
            "devtmpfs",
            MsFlags::empty(),
            Some("mode=0755"),
        ),
        (
            "devpts",
            "/dev/pts",
            "devpts",
            MsFlags::empty(),
            Some("newinstance,ptmxmode=0666,mode=0620"),
        ),
        (
            "tmpfs",
            "/dev/shm",
            "tmpfs",
            MsFlags::empty(),
            Some("mode=1777"),
        ),
        (
            "tmpfs",
            "/run",
            "tmpfs",
            MsFlags::empty(),
            Some("mode=0755"),
        ),
        (
            "tmpfs",
            "/tmp",
            "tmpfs",
            MsFlags::empty(),
            Some("mode=1777"),
        ),
        (
            "cgroup2",
            "/sys/fs/cgroup",
            "cgroup2",
            MsFlags::empty(),
            None,
        ),
    ];
    for (source, target, kind, flags, options) in mounts {
        std::fs::create_dir_all(target).map_err(|e| format!("making {target}: {e}"))?;
        mount(Some(*source), *target, Some(*kind), *flags, *options)
            .map_err(|e| format!("mounting {kind} on {target}: {e}"))?;
    }
    Ok(())
}

/// The modules, by the image's `modprobe` (the same the kernel runs for
/// what it loads on demand). A required one that fails powers off.
fn load_modules() -> Result<(), String> {
    for (module, required) in REQUIRED_MODULES
        .iter()
        .map(|m| (m, true))
        .chain(OPTIONAL_MODULES.iter().map(|m| (m, false)))
    {
        let loaded = Command::new("/sbin/modprobe")
            .arg(module)
            .status()
            .is_ok_and(|status| status.success());
        match (loaded, required) {
            (true, _) => {}
            (false, true) => return Err(format!("the kernel module {module} didn't load")),
            (false, false) => say(&format!(
                "the kernel module {module} didn't load (optional)"
            )),
        }
    }
    Ok(())
}

/// The loopback, and the VM's NAT link by DHCP (busybox's `udhcpc`, which
/// keeps renewing the lease in the background).
fn network() {
    let ran = |program: &str, args: &[&str]| {
        Command::new(program)
            .args(args)
            .status()
            .is_ok_and(|status| status.success())
    };
    if !ran("/sbin/ip", &["link", "set", "lo", "up"]) {
        say("the loopback didn't come up");
    }
    // `udhcpc` doesn't bring the link up itself: without it, it loops on
    // "Network is down". Five tries two seconds apart, then the background
    // (it keeps the lease renewed there either way).
    if !ran("/sbin/ip", &["link", "set", "eth0", "up"])
        || !ran("/sbin/udhcpc", &["-i", "eth0", "-b", "-t", "5", "-T", "2"])
    {
        say("no address for eth0: the guest has no network");
    }
}

/// The data disk, formatted the first time, checked every time (and
/// repaired if the check leaves errors: D10.4-2), mounted; a repair said
/// in its marker for [`disk::BOOTS_SAID`] boots.
fn data_disk() -> Result<(), String> {
    let found = examine(DATA_DISK)?;
    let mut repaired = None;
    match disk::step(found) {
        Step::Refuse => {
            return Err(format!(
                "{DATA_DISK} holds data that isn't Shipyard's data disk (no `{}` file system): \
                 refused, not formatted",
                disk::LABEL
            ));
        }
        Step::Format => {
            say(&format!("formatting {DATA_DISK} (its first boot)"));
            run_tool(
                "/sbin/mkfs.ext4",
                &["-q", "-F", "-L", disk::LABEL, DATA_DISK],
            )?;
        }
        Step::CheckAndMount => repaired = check_data_disk()?,
    }
    std::fs::create_dir_all(DATA).map_err(|e| format!("making {DATA}: {e}"))?;
    mount(
        Some(DATA_DISK),
        DATA,
        Some("ext4"),
        MsFlags::empty(),
        None::<&str>,
    )
    .map_err(|e| format!("mounting {DATA_DISK} on {DATA}: {e}"))?;
    if let Err(error) = disk::mark(Path::new(DATA), repaired.as_deref()) {
        say(&format!("the data disk's repair marker: {error}"));
    }
    Ok(())
}

/// `e2fsck` on the data disk ([`disk::check`]).
fn check_data_disk() -> Result<Option<String>, String> {
    disk::check(
        DATA_DISK,
        |mode| {
            Command::new("/sbin/e2fsck")
                .args([mode, DATA_DISK])
                .status()
                .map_err(|e| format!("running e2fsck: {e}"))
                .map(|status| status.code().unwrap_or(-1))
        },
        say,
    )
}

/// What the data disk holds, before it's mounted (its first MiB read).
fn examine(device: &str) -> Result<Found, String> {
    use std::io::Read as _;
    let mut start = vec![0; 1024 * 1024];
    std::fs::File::open(device)
        .and_then(|mut file| file.read_exact(&mut start))
        .map_err(|e| format!("reading {device}: {e}"))?;
    Ok(disk::found(&start))
}

/// Runs a tool that must succeed.
fn run_tool(program: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("running {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed ({status})"))
    }
}

/// How the Mac's home is mounted (F108): its set-user-ID bits and device
/// files mean nothing in the guest, nor in a container a bind of it gives
/// them to (a bind keeps both flags): a file made on the Mac is never a
/// way to root in the VM.
const HOME_FLAGS: MsFlags = MsFlags::MS_NOSUID.union(MsFlags::MS_NODEV);

/// The Mac's home, shared at its own path (`shipyard.home=` on the kernel's
/// command line). Without it, commands run, but no path of the Mac's does.
fn home() {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let Some(home) = home_of(&cmdline) else {
        say("no home on the kernel's command line: the Mac's files aren't shared");
        return;
    };
    let shared = std::fs::create_dir_all(home)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            mount(
                Some(forward::HOME_TAG),
                home,
                Some("virtiofs"),
                HOME_FLAGS,
                None::<&str>,
            )
            .map_err(|e| e.to_string())
        });
    if let Err(error) = shared {
        say(&format!("the Mac's home isn't shared at {home}: {error}"));
        return;
    }
    for place in hidden_in(home, &forward::hidden_of(&cmdline)) {
        hide(&place);
    }
}

/// The places to hide (`hidden`, from the command line) that are in the
/// shared `home` and nothing that steps out of it.
pub(super) fn hidden_in(home: &str, hidden: &[String]) -> Vec<String> {
    hidden
        .iter()
        .filter(|place| {
            std::path::Path::new(place).starts_with(home)
                && *place != home
                && !place.split('/').any(|part| part == "..")
        })
        .cloned()
        .collect()
}

/// `place` (the VM's own files, in the shared home) covered with an empty,
/// read-only tmpfs: a container given the home (`--volume ~:/h`, binds are
/// recursive) sees nothing there, so it can't rewrite the VM's data disk
/// or its boot files (the guest needs neither through the share).
fn hide(place: &str) {
    if !std::path::Path::new(place).is_dir() {
        return;
    }
    let flags = MsFlags::MS_RDONLY | MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC;
    if let Err(error) = mount(
        Some("tmpfs"),
        place,
        Some("tmpfs"),
        flags,
        Some("size=4k,mode=0555"),
    ) {
        say(&format!("{place} isn't hidden from containers: {error}"));
    }
}

/// The home the kernel's command line names: an absolute path, nothing
/// that steps out of it.
pub(super) fn home_of(cmdline: &str) -> Option<&str> {
    let home = cmdline
        .split_ascii_whitespace()
        .find_map(|word| word.strip_prefix(forward::HOME_KEY))?;
    (home.starts_with('/') && !home.split('/').any(|part| part == "..")).then_some(home)
}

/// Runs `shipyard ARGS`, waited for up to `limit` (killed past it).
fn shipyard(args: &[&str], limit: Duration) {
    let child = Command::new(SHIPYARD)
        .args(args)
        .stdin(Stdio::null())
        .spawn_unblocked();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return say(&format!("shipyard {}: {error}", args.join(" "))),
    };
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                return say(&format!("shipyard {}: {status}", args.join(" ")));
            }
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return say(&format!(
                    "shipyard {}: still running after {limit:?}, killed",
                    args.join(" ")
                ));
            }
        }
    }
}

/// Starts the agent, in a process group of its own: a group it signals
/// by mistake (0: its own) is never PID 1's, which would take a SIGTERM or
/// SIGINT for the VM's stop.
fn start_agent() -> Result<Child, String> {
    Command::new(SHIPYARD)
        .arg(super::AGENT_ARG)
        .stdin(Stdio::null())
        .process_group(0)
        .spawn_unblocked()
        .map_err(|e| format!("starting the agent: {e}"))
}

/// The signals PID 1 waits for, blocked so they're only taken by `wait`:
/// a child's end, and the stop.
fn waited_for() -> SigSet {
    let mut set = SigSet::empty();
    for signal in [
        Signal::SIGCHLD,
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGPWR,
    ] {
        set.add(signal);
    }
    set
}

/// A `Command` whose child gets the signal mask PID 1 blocks unblocked.
trait SpawnUnblocked {
    fn spawn_unblocked(&mut self) -> std::io::Result<Child>;
}

impl SpawnUnblocked for Command {
    fn spawn_unblocked(&mut self) -> std::io::Result<Child> {
        // SAFETY: between fork and exec, `sigprocmask` only: it's
        // async-signal-safe, allocates nothing and takes no lock.
        unsafe {
            self.pre_exec(|| {
                sigprocmask(SigmaskHow::SIG_UNBLOCK, Some(&waited_for()), None)
                    .map_err(std::io::Error::from)
            });
        }
        self.spawn()
    }
}

/// How often PID 1 looks again without a signal: to start an agent whose
/// last start failed.
const LOOK_AGAIN: Duration = Duration::from_secs(5);

/// PID 1's loop: reaps every child (the whole guest's orphans), starts the
/// agent again when it ends or its start failed, and on a stop, stops the
/// containers and powers off.
fn serve(agent: u32) -> ! {
    let signals = waited_for();
    let mut agent = Some(agent);
    let mut backoff = Backoff::new(Instant::now());
    let mut next_start = Instant::now();
    loop {
        let wait = if agent.is_some() {
            LOOK_AGAIN
        } else {
            next_start
                .saturating_duration_since(Instant::now())
                .min(LOOK_AGAIN)
        };
        match wait_for(&signals, wait) {
            Some(Signal::SIGCHLD) | None => {}
            Some(_) => stop(),
        }
        if let Some(ended) = reap(agent)
            && agent == Some(ended)
        {
            next_start = backoff.ended(Instant::now());
            say("the agent ended; starting it again");
            agent = None;
        }
        if agent.is_none() && Instant::now() >= next_start {
            backoff.started(Instant::now());
            match start_agent() {
                Ok(child) => agent = Some(child.id()),
                Err(error) => {
                    next_start = backoff.ended(Instant::now());
                    say(&format!("{error}; trying again"));
                }
            }
        }
    }
}

/// When the agent may start again (F109): 0.1 s after it ended, then
/// after a wait that doubles, to 5 s, while it keeps ending soon after its
/// start (a crash loop: no spin, no flood of the console log); from 0.1 s
/// again once it has run [`Backoff::STEADY`].
#[derive(Debug)]
struct Backoff {
    next: Duration,
    started: Instant,
}

impl Backoff {
    /// The first wait.
    const FIRST: Duration = Duration::from_millis(100);
    /// The longest.
    const MOST: Duration = Duration::from_secs(5);
    /// How long a start must run to count as one that worked.
    const STEADY: Duration = Duration::from_secs(30);

    fn new(started: Instant) -> Self {
        Self {
            next: Self::FIRST,
            started,
        }
    }

    /// The agent is started at `now`.
    fn started(&mut self, now: Instant) {
        self.started = now;
    }

    /// The agent ended (or couldn't be started) at `now`: when it may
    /// start again.
    fn ended(&mut self, now: Instant) -> Instant {
        if now.saturating_duration_since(self.started) >= Self::STEADY {
            self.next = Self::FIRST;
        }
        let wait = self.next;
        self.next = self.next.saturating_mul(2).min(Self::MOST);
        now + wait
    }
}

/// The next of `signals` (blocked), or `None` once `limit` has passed.
fn wait_for(signals: &SigSet, limit: Duration) -> Option<Signal> {
    // (nix's `TimeSpec`: `time_t` itself differs between libcs.)
    let timeout = nix::sys::time::TimeSpec::from_duration(limit);
    // SAFETY: `sigtimedwait` reads the set and the timeout; with a null
    // `siginfo`, it writes nothing back.
    let signal =
        unsafe { libc::sigtimedwait(signals.as_ref(), std::ptr::null_mut(), timeout.as_ref()) };
    Signal::try_from(signal).ok()
}

/// Reaps every child that has ended; returns the agent's PID if it was one
/// of them.
fn reap(agent: Option<u32>) -> Option<u32> {
    let mut agent_ended = None;
    loop {
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(_) => return agent_ended,
            Ok(status) => {
                let pid = status
                    .pid()
                    .and_then(|pid| u32::try_from(pid.as_raw()).ok());
                if pid.is_some() && pid == agent {
                    agent_ended = pid;
                }
            }
        }
    }
}

/// The data disk's writes on the disk (`syncfs` on it alone, F109): a
/// global `sync` would wait on every file system, the home's virtiofs too,
/// which a wedged Mac side never lets end. Not mounted (yet): nothing to.
fn sync_data() {
    if let Ok(data) = std::fs::File::open(DATA) {
        let _ = nix::unistd::syncfs(&data);
    }
}

/// Reaps every process (`reap_one`: `waitpid(-1, WNOHANG)`) until none is
/// left or `deadline` passes (F109): never blocking, so a process stuck in
/// the kernel (an uninterruptible wait on a wedged virtiofs call, which
/// even SIGKILL doesn't end) can't hold PID 1 past it.
fn reap_all(deadline: Instant, mut reap_one: impl FnMut() -> nix::Result<WaitStatus>) {
    while Instant::now() < deadline {
        match reap_one() {
            Ok(WaitStatus::StillAlive) => std::thread::sleep(Duration::from_millis(20)),
            // One reaped, or interrupted: look again.
            Ok(_) | Err(nix::errno::Errno::EINTR) => {}
            // None left (`ECHILD`), or a failure that would only repeat.
            Err(_) => return,
        }
    }
}

/// The VM's stop: the containers stopped (as `system stop` does on a Linux
/// host's shutdown), every process ended, the disk synced and let go.
fn stop() -> ! {
    say("stopping");
    // What's written so far, on the disk first: a Mac that shuts down may
    // end the VM before its containers have all stopped.
    sync_data();
    shipyard(&["system", "stop"], SETTLE);
    let _ = nix::sys::signal::kill(Pid::from_raw(-1), Signal::SIGTERM);
    std::thread::sleep(Duration::from_secs(2));
    let _ = nix::sys::signal::kill(Pid::from_raw(-1), Signal::SIGKILL);
    // Every process reaped (they hold the disk's files open), then the disk
    // let go cleanly: its journal needs no replay at the next boot.
    reap_all(Instant::now() + Duration::from_secs(5), || {
        waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG))
    });
    sync_data();
    if let Err(error) = nix::mount::umount(DATA) {
        say(&format!(
            "unmounting {DATA}: {error}; made read-only instead"
        ));
        let _ = mount(
            None::<&str>,
            DATA,
            None::<&str>,
            MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
            None::<&str>,
        );
    }
    power_off("stopped")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The home is mounted without set-user-ID bits or device files.
    #[test]
    fn the_home_is_mounted_nosuid_and_nodev() {
        assert!(HOME_FLAGS.contains(MsFlags::MS_NOSUID));
        assert!(HOME_FLAGS.contains(MsFlags::MS_NODEV));
    }

    /// The stop's reaping ends once nothing is left, or at its deadline
    /// though a process never ends (F109): it never blocks.
    #[test]
    fn reaping_ends_by_its_deadline() {
        let deadline = Instant::now() + Duration::from_millis(200);
        reap_all(deadline, || Ok(WaitStatus::StillAlive));
        let late = Instant::now().saturating_duration_since(deadline);
        assert!(late < Duration::from_secs(1), "{late:?} past it");
        let mut looked = 0;
        reap_all(Instant::now() + Duration::from_secs(60), || {
            looked += 1;
            Err(nix::errno::Errno::ECHILD)
        });
        assert_eq!(looked, 1, "none left: at once");
        // One reaped, then none: looked at twice, no wait.
        let mut answers = [
            Ok(WaitStatus::Exited(Pid::from_raw(7), 0)),
            Err(nix::errno::Errno::ECHILD),
        ]
        .into_iter();
        let mut looked = 0;
        reap_all(Instant::now() + Duration::from_secs(60), || {
            looked += 1;
            answers.next().unwrap()
        });
        assert_eq!(looked, 2);
    }

    /// The agent's restarts wait longer each time it ends soon after its
    /// start, up to 5 s, and from 0.1 s again after a start that ran.
    #[test]
    fn a_crash_looping_agent_is_restarted_ever_later() {
        let start = Instant::now();
        let mut backoff = Backoff::new(start);
        let waits: Vec<Duration> = (0..8)
            .map(|_| {
                backoff.started(start);
                backoff.ended(start) - start
            })
            .collect();
        let ms = Duration::from_millis;
        assert_eq!(
            waits,
            [
                ms(100),
                ms(200),
                ms(400),
                ms(800),
                ms(1600),
                ms(3200),
                ms(5000),
                ms(5000)
            ]
        );
        backoff.started(start);
        let later = start + Backoff::STEADY;
        assert_eq!(
            backoff.ended(later) - later,
            ms(100),
            "after a start that ran"
        );
    }

    /// Only places inside the home are hidden: not the home itself, not
    /// one elsewhere, nothing that steps out of it.
    #[test]
    fn only_places_in_the_home_are_hidden() {
        let named = [
            "/Users/me/Library/Application Support/Shipyard/vm",
            "/Users/me",
            "/tmp/vm",
            "/Users/me/../other",
            "/Users/meagain/vm",
        ]
        .map(str::to_owned);
        assert_eq!(
            hidden_in("/Users/me", &named),
            ["/Users/me/Library/Application Support/Shipyard/vm"]
        );
    }

    /// The home is the kernel's command line's `shipyard.home=`, absolute,
    /// with no `..`; anything else shares nothing.
    #[test]
    fn the_home_is_an_absolute_path_from_the_command_line() {
        assert_eq!(
            home_of("console=hvc0 shipyard.home=/Users/me panic=-1"),
            Some("/Users/me")
        );
        assert_eq!(home_of("console=hvc0"), None);
        assert_eq!(home_of("shipyard.home=Users/me"), None);
        assert_eq!(home_of("shipyard.home=/Users/../etc"), None);
    }
}

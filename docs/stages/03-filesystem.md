# Stage 3 notes: filesystem isolation and OCI bundles

The engineering record of stage 3: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [Running commands](../user/run.md#root-filesystem),
[OCI bundles](../user/bundles.md), [The OCI runtime](../user/runtime.md) and
[Exit codes](../user/exit-codes.md).

## Outcome

Stage 3 asked for a container with a root filesystem of its own, OCI bundles, the OCI
runtime commands, user and group names, the domain name and namespaced sysctls
([ROADMAP](../ROADMAP.md), [FEATURES §3](../FEATURES.md#3-filesystem-isolation-and-oci-bundles--stage-3)).
On Linux, `shipyard run --rootfs DIR` and `shipyard run --bundle DIR` now run a command in
its own root filesystem, and `shipyard-runtime` runs a bundle step by step, as runc does.

| Requirement | Where | Tested by |
|---|---|---|
| The init re-execs first: all container setup in a clean, single-threaded image | `process/init.rs`, `setup.rs` (sealed memfd), `child.rs`, `spawn.rs` (`CleanImageFiles`) | every isolation test; `a_large_environment_reaches_the_command`, the non-UTF-8 tests, `the_init_entry_point_by_hand_does_nothing`, `containers.rs`'s no-PID-namespace tests |
| A root filesystem of its own: bind, mounts, `pivot_root`, the old root detached | `process/rootfs.rs` (`enter`, `bind_root`, `mount_standard`, `pivot`) | `the_command_sees_the_root_filesystem`, `no_host_mount_is_left_inside`, `the_root_filesystems_own_mounts_come_along`, … (17 root tests) |
| `/proc`, read-only `/sys`, a minimal `/dev` with devpts, shm, mqueue | `rootfs.rs` (`mount_standard`, `fill_dev`, `DEV_ENTRIES`) | `proc_shows_the_containers_processes`, `sys_is_read_only`, `dev_has_the_standard_devices` |
| Masked and read-only paths, `--read-only` | `rootfs.rs` (`mask`, `make_read_only`, `add_restrictions`, `remount`) | `masked_and_read_only_paths`, `read_only_keeps_the_mounts_other_options`, `read_only_paths_cover_the_mounts_below` |
| Mount targets never followed out of the root | `rootfs.rs` (`open_directory`, `O_NOFOLLOW`), `process/resolve.rs` | `a_symlinked_mount_point_is_refused`, `a_destination_through_a_symlink_stays_inside`, `a_destination_led_into_proc_is_refused`, the resolver's unit tests (both paths) |
| OCI bundles (`run --bundle`): refuse what confines less, warn on what doesn't | `oci.rs` (`load_bundle`, `Reader`), `isolation.rs` (`Mount`, `Root`) | `oci.rs` unit tests; `bundle.rs` (19 tests) |
| Bind and tmpfs mounts at any destination, with their flags | `rootfs.rs` (`add_mount`, `bind`, `add_restrictions`, `set_attributes`) | `a_read_only_bind_from_the_host`, `a_read_only_rbind_covers_the_mounts_below`, `restricting_a_bind_keeps_the_hosts_read_only_mounts`, `a_read_only_file_bind`, `a_tmpfs_with_options` |
| User and group names, memberships, `HOME` | `process/identity.rs` | `names.rs` (12 tests), the identity unit tests |
| The domain name and namespaced sysctls | `isolation.rs` (`validate_sysctl`), `process/sysctl.rs`, `init.rs` (`set_domainname`) | `the_domain_name_is_the_containers_own`, `an_ipc_sysctl_is_the_containers_own`, `sysctls_that_would_change_the_host_are_refused` |
| The OCI runtime: `create / start / state / kill / delete` | `container/` (`lifecycle.rs`, `mod.rs`), `src/bin/shipyard-runtime.rs` | `runtime.rs` (24 tests), `lifecycle.rs` unit tests |
| Terminals with a root filesystem and with the runtime | (no new code: descriptors, not names) | four `ROOT_TESTS` in `terminal.rs` |

**Moved out of stage 3**, each to the stage that gives it meaning:

| Item | Now in | Why |
|---|---|---|
| A terminal inside (`process.terminal`, `--console-socket`) | stage 8 | needs the monitor-owned pty |
| Names for commands run in an existing container | stage 8 (`exec`) | needs the safe resolver in a read-only mode, through the container's root |
| Hooks, `oomScoreAdj`, `linux.resources`, the cgroup mount, `cgroupsPath` | stage 5 | cgroups |
| Bundles without a `pid` namespace in `shipyard-runtime`, `kill STOP`, `pause`/`resume` | stage 5 | only cgroups can find, kill or freeze every process |
| Capabilities, `noNewPrivileges`, seccomp, user namespaces, id mappings | stage 4 | security |
| `/sys` without the host's network devices, `net.*` sysctls, `netDevices` | stage 7 | networking |
| Images as root filesystems | stage 6 | images |

Paths below are relative to `crates/shipyard-runtime/src/` and `crates/shipyard-cli/`.

## Architecture

```
shipyard run [--rootfs DIR | --bundle DIR]  (run.rs; main() calls init_entry_point() first)
  └─ Supervisor::supervise(spec, stop, isolation)          supervisor.rs: check_isolation,
       │                                                     resolve_names (--no-isolation)
       └─ spawn()                                           spawn.rs
            ├─ CleanImageFiles: status pipe, the binary (/proc/self/exe), the setup as
            │  JSON in a sealed memfd, the report pipe, [the start gate]
            └─ clone3(NEWPID|NEWNS|…) → child (after-fork rules):
                 reset dispositions → wait for "go" → parent-death signal → close all but
                 the four → execveat(exe, "__shipyard-init …") (→ execve /proc/self/fd/N)

the clean image (init.rs, ordinary single-threaded Rust)
  non-dumpable → close-on-exec again → read + check the setup → start mark
  → setsid → rslave / → with a root: rootfs::enter (below); without: /proc, sysctls
  → hostname, domain name → identity::resolve (the container's /etc)
  → the command's plan (PATH, credentials, limits)
  → [gate: close the report pipe ("created"); wait_at_gate; a new report pipe]
  → with a PID namespace: fork the command (PID 2) → [explain_failure] → become_init
  → without: exec in place

rootfs::enter(root)
  bind_root (rbind DIR onto itself) → mount_standard: proc, sys (ro), dev (tmpfs) via
  O_NOFOLLOW descriptors, host mounts on them detached first; /dev filled relative to
  its descriptor → sysctls through the new /proc → the bundle's mounts (resolve_in_root,
  bind or tmpfs, add_restrictions) → pivot_root(".", ".") + MNT_DETACH → masks →
  read-only paths → read-only /

shipyard-runtime create ID  (src/bin/shipyard-runtime.rs → container::create)
  load_bundle (pid namespace required) → mkdir ROOT/ID → open it (a descriptor)
  → under the lock: mkfifo exec.fifo, spawn the monitor (/proc/self/exe
    __shipyard-monitor BUNDLE DIRFD ANSWERFD), record it in state.json
  → read the monitor's answer: {"created":PID} or {"failed":"…"}

the monitor (one per container; its own session)
  close inherited fds → spawn the container with the gate → wait for "set up"
  → record_init (under the lock, through its directory descriptor) → answer
  → stdio to /dev/null → reap the init → write `exit` (through the descriptor)

start → open the FIFO, read the init's byte, remove the FIFO
state / kill / delete → lock ROOT/ID (checked to still be the same directory)
  → status from the directory and /proc/PID/stat start times → pidfd signals
```

## Decisions and why

**Made with the user:**

- **Root filesystems come from a directory you supply** (`--rootfs DIR`, or a bundle's
  `root.path`). No images until stage 6, which will produce such directories. Tests
  build a tiny root filesystem from host binaries, with no network.
- **A runc-compatible binary, `shipyard-runtime`,** for the OCI lifecycle: `create
  --bundle DIR ID`, `start`, `state`, `kill`, `delete`, state under `/run/shipyard`
  (`--root`). The conformance suite (stage 14) and later the engine call it exactly like
  runc. It's a second `[[bin]]` in `crates/shipyard-cli`, over a library module
  (`shipyard_runtime::container`) for the engine. `shipyard run` stays the all-in-one
  command.
- **The init re-execs first** (runc-style). With a root filesystem of its own,
  re-executing the shipyard binary *after* `pivot_root` would need the host's loader and
  libc, which a minimal root filesystem lacks. So the cloned child re-execs at once,
  while it still sees the host's files, and the fresh image does the whole setup in
  ordinary Rust. The after-fork rules shrink to the window between `clone` and that
  `exec`.
- **Refuse what would confine less.** A bundle field whose absence would leave the
  container less confined than asked is refused, every such field in one error (exit
  125): capabilities, `noNewPrivileges`, seccomp, AppArmor, SELinux, user/network/
  cgroup/time namespaces and joining one, id mappings, resources, devices, a terminal,
  shared propagation, mount types other than bind and tmpfs, and (step 7) scheduler, I/O
  priority, CPU affinity, Intel RDT, memory policy, personality, net devices and time
  offsets. A `runc spec` config is therefore refused until those arrive (it asks for a
  terminal, stage 8; capabilities and `noNewPrivileges`, stage 4; a device rule under
  `linux.resources`, stage 5; and a network namespace, stage 7). Fields that don't
  confine are ignored with a warning; fields the spec doesn't define are ignored
  silently, as it requires.
- **Bind and tmpfs mounts at any destination**, through the safe resolver, now.
- **`HOME` from `/etc/passwd`** when the environment has none (otherwise `/root` for
  root, `/` for others), for CLI runs and bundles alike, as runc and Docker do.
- **`--user alice` adds alice's groups** from the container's `/etc/group`, as Docker
  does. Bundles stay exact (`additionalGids` only).

**Engineering:**

- **`serde` and `serde_json`** (`[workspace.dependencies]`) read `config.json` and carry
  the init's setup description.
- **The API mirrors OCI:** `Isolation.root: Option<Root>`; a `Root` holds the path,
  `read_only`, `masked_paths`, `readonly_paths` and `mounts: Vec<Mount>`, and
  `Root::new` fills in Docker's default masked and read-only paths. A `Mount` has a
  destination, a kind (`Bind { source, recursive }` or `Tmpfs`), flags (`ReadOnly`,
  `NoSuid`, `NoDev`, `NoExec`) and data. Names are `User.names: Names` (`user`, `group`,
  `additional_groups`, `memberships`). `oci::load_bundle` reads `config.json` with
  `serde` structs for the parts of the spec Shipyard knows, and returns a `Bundle`: a
  `ProcessSpec`, an `Isolation`, the warnings and the annotations (which `state`
  reports).
- **Every isolated run goes through the clean image,** with or without a PID namespace,
  so all container setup lives in one place. `init_entry_point()` is required for any
  isolation. Joining (`setns`) has no setup and is unchanged.
- **The setup travels in a sealed memfd** (`memfd_create(2)`, `F_ADD_SEALS`). A pipe
  written before the clone would block past 64 KiB with no reader yet, and an
  environment can be bigger. Arguments, environment and paths travel as bytes, so
  non-UTF-8 survives; the clean image validates them again (`Setup::spec`). The hidden
  entry point acts only on a memfd with every seal (`F_GET_SEALS`).
- **The report pipe keeps its meaning** ("end of file = the command exec'd"). Its write
  end is passed deliberately, made close-on-exec again at once, and closed by the init
  after forking the command, so the re-exec doesn't count as "started". The start mark
  covers both modes, so `InitDidNotStart` is reported without a PID namespace too.
- **Setup failures in the clean image are numbered steps** (`ChildStep` 20–37:
  `Reexec`, `ReadSetup`, `InitPanicked`, `BindRoot` … `ReadOnlyRoot`, `MountTarget`,
  `Mount`, `MountRestrict`, `UserLookup`, `Domainname`, `Sysctl`) with a `detail`
  index, not messages: one protocol, and the parent names the path, mount, name or
  sysctl. A panic hook reports `InitPanicked` and exits 125.
- **The command resets its own signal dispositions** (`command_main`): the clean image's
  Rust startup ignores SIGPIPE.
- **Without a PID namespace, the command checks `getppid()` against shipyard's PID**
  before `exec` (the parent-death signal set before the re-exec is cleared by the
  credential change, and the go pipe is closed by the re-exec).
- **Bind the root filesystem onto itself, recursively,** then `pivot_root(".", ".")`
  and a lazy detach of the old root (probes 1, 3). No `oldroot` directory needed.
- **Mounts before the pivot, as runc does.** Under a user namespace (stage 4), `proc` and
  `sysfs` may only be mounted while a fully visible one is in the mount namespace, which
  stops being true once the old root is gone. Masks and read-only paths come after it.
- **`/proc` is mounted `nosuid,nodev,noexec`.**
- **Mount targets are never resolved by path** (probe 11, CVE-2021-30465). `proc`, `sys`
  and `dev` are opened `O_NOFOLLOW|O_DIRECTORY` (made if missing; EEXIST from a racing
  container is fine), and mounted through `fchdir` + `"."`. A symlinked one is refused
  rather than followed (runc follows it). Not `/proc/self/fd/N`: detaching a `proc`
  target that is the process's own `/proc` would remove the path it goes through.
- **A bad root filesystem is caught up front** (`check_isolation`): a missing DIR, or one
  that isn't a directory, is `Error::RootFilesystem`. The CLI canonicalizes DIR and names
  `--rootfs` in its error. `--rootfs /` is refused (`RootIsHostRoot`): a bind stacked on
  `/` is never reached by a lookup of `/`, so `pivot_root` would get the current root and
  fail with EBUSY; and without `--rootfs` the container already shares the host's files.
- **Exec hints** look at DIR + the program's path on the host: EACCES on a `noexec`
  mount names the option (probe 14; without `--rootfs` too), and ENOENT for a program
  that is there names the missing loader or interpreter. They're hints only: an absolute
  symlink inside DIR would be followed on the host.
- **Whatever is mounted on a target is detached first** (`umount2(".", MNT_DETACH)`, top
  first, at most 64, in the container's copy only): a chroot often has the host's
  `/dev` or `/proc` there, which root inside could otherwise uncover.
- **`/dev` is a fresh tmpfs** (`nosuid`, not `nodev`: probe 10) with six device nodes,
  five links, devpts (`newinstance,ptmxmode=0666`), shm and (with an IPC namespace)
  mqueue, all made relative to its descriptor. The process's umask is 0 while it's
  made; the command sets its own.
- **`/sys` is a fresh sysfs, read-only.** Until stage 7 it lists the host's network
  devices (probe 7, documented); under a user namespace it will be a bind of the host's.
- **The safe resolver** (`resolve.rs`) for arbitrary destinations:
  `openat2(RESOLVE_IN_ROOT | RESOLVE_NO_MAGICLINKS)` where the kernel has it, otherwise
  a walk: each name `O_NOFOLLOW`, symlinks read and walked (absolute ones restart at
  the root), `..` stopping at the root, at most 40 symlinks; missing components made as
  they're met. Destinations in `/proc` or `/sys`, at `/dev`, or `/` are refused by their
  text (runc's `/proc` CVE class), and since step 7 the opened target's file system is
  checked too: a symlink could lead there, which only the text had been checked against
  (CVE-2019-19921's class). procfs or sysfs is refused. What's created for a destination
  stays in the root filesystem afterwards, as with runc.
- **Restricting a mount only adds flags** (step 7). A bind remount *replaces* a mount's
  flags (probe 8b), so a read-only remount used to drop `nosuid,nodev,noexec`, and a
  `nosuid` remount used to make a read-only host mount writable. Now
  `mount_setattr(2)` (Linux 5.12) sets attributes and clears none, with `AT_RECURSIVE`
  for `rbind` and read-only paths (every mount below, hidden ones too). On ENOSYS the
  fallback remounts with the flags `statvfs` reports (read-only included) repeated, and
  remounts the mounts below by path. (`--read-only` and `root.readonly` remount only `/`
  itself, as runc does, through `remount`: the fallback's limits apply there on every
  kernel.)
- **A bind keeps the restrictions of the host mounts it comes from:** `exec` doesn't undo
  the host's `noexec`.
- **A bundle bind is recursive only with `rbind`,** as with runc (`type: bind` alone gets
  no `MS_REC`). A bind remount is done through a descriptor resolved again after the
  bind, so it reaches the new mount, not the directory under it.
- **Names are read from the container's files, not through NSS** (`getpwnam` would follow
  the host's `nsswitch.conf` and load its modules). First entry wins; comments, NIS
  `+`/`-` and malformed lines are skipped; files opened non-blocking, checked as regular
  once open, read with a 16 MiB limit. Only names fail hard: lookups that only fill in
  (a numeric user's entry, `HOME`, memberships) fall back to defaults. IDs are read as
  the kernel stores them (`-2` is 4294967294); 4294967295 ("leave unchanged") is refused
  for a named user or group, skipped for a membership. Names resolve in the clean image
  (the container's `/etc`); with `--no-isolation` in the supervisor (the host's); joins
  refuse names. A missing file is empty. Docker's rules: a user name must exist and gives
  the uid, group and home; a number is used as it is, with its entry's group and home if
  it has one (group 0 otherwise); a group name must exist; memberships and `--group-add`
  names are added, duplicates removed; `HOME` is filled only if unset. Memberships apply
  only to isolated runs: with `--no-isolation` they'd be the host's groups, which an
  ordinary user can't take, so `--user 1000` would have stopped working there.
- **Sysctls: only the IPC namespace's own** (`kernel.msg*`, `kernel.sem`,
  `kernel.shm*`, `fs.mqueue.*`), with an IPC namespace; `net.*` waits for stage 7;
  `kernel.hostname` and `kernel.domainname` have their own settings; anything else isn't
  namespaced. Keys are `[a-z0-9_.]` without empty parts; values have no NUL or newline;
  in a bundle, values must be strings.
  Written through a `/proc` descriptor, the last component `O_NOFOLLOW`; with a root
  filesystem through its new `/proc` before the pivot (before `/proc/sys` is read-only).
  A disallowed one in a bundle is an error, never ignored.
- **The domain name** needs a UTS namespace and follows the hostname rules. It's set
  with `setdomainname(2)` through raw `libc` (nix has no wrapper), after the hostname.
- **The config is read like hostile input:** opened non-blocking (a FIFO would block),
  checked as a regular file once open, at most 4 MiB, read with that limit. JSON errors
  give line and column. Refusals are collected and reported before validation. Absent
  masked/read-only paths mean none; `process.env` gets no `PATH` added (`HOME` is, as
  decided in step 5); `root.path` and sources are relative to the bundle and made
  canonical. Ignored with a warning: hooks, `oomScoreAdj`, `linux.cgroupsPath`, a
  `cgroup` or `cgroup2` mount at `/sys/fs/cgroup` (Podman writes the latter), and
  access-time mount options (`noatime`, `relatime`, …). The six standard mounts are accepted silently and Shipyard's own
  used (every `runc spec` config lists them). tmpfs option values may not contain a
  comma (it would smuggle in another option). A `resources` of only nulls, empty lists
  and objects asks nothing (`asks_nothing`).
- **A monitor per container** (the daemonless decision), started by re-executing the
  binary through `std::process::Command` (safe from a threaded caller). It calls
  `setsid` in the child before the exec (`pre_exec`), so it leads a session and a
  process group of its own before running any code: the caller's terminal closing, or
  a Ctrl-C meant for it, can't reach the monitor. (`setsid` fails for a process-group
  leader, so it can't come after `process_group(0)`; step 8 found it had silently done
  so.) It closes every inherited
  descriptor but its own two, spawns the container, records it, answers `create` with
  one JSON line, nulls its stdio, reaps the init and records the exit. `create` watches
  the monitor process, not only the pipe's end-of-file (a leaked copy would hide its
  death). Killing the monitor kills the container (the init's parent-death signal).
- **The state directory is reached by descriptor, not path, wherever it can outlive a
  container** (step 7): `create` opens `ROOT/ID` and passes it to the monitor, whose
  writes go through `openat`/`renameat`; a removed directory takes no more files
  (ENOENT), so a stale monitor can't write into a newer container with the same ID.
  Commands lock a descriptor and check it's still the directory at the path
  (`same_directory`); `create`'s cleanup removes only its own.
- **`flock` on its own open file description** (`openat(dir, ".")`): locks belong to
  descriptions, and the monitor's copy shares one with `create`'s. Calls wait at most
  10 s (`LOCK_EX|LOCK_NB`, polled), then fail with "container ID is busy: another call
  has held its lock for over 10 seconds" (`ContainerError::Busy`). `create` locks only
  while filling the directory in, so `state` says *creating* during the setup and
  `delete` can stop a setup that hangs: it kills the monitor recorded in `state.json`.
  A setup that does finish after its directory was removed finds the directory gone
  when it records the init, and the monitor kills the container.
- **The start gate** is an exec FIFO, passed to the clean image as an `O_PATH`
  descriptor. The init closes its report pipe ("created"), opens the FIFO for writing
  through `/proc/self/fd`, writes one byte, keeps its end open until the reader leaves
  (POLLERR), and goes on only if the byte was taken (`FIONREAD` 0); otherwise it waits
  again. A self-caused SIGPIPE (blocked, so pending even though ignored) is discarded.
  `start` reads the byte with a 5 s poll, then removes the FIFO.
- **After `start`, the command reports to the init,** which writes the explained failure
  to the container's stderr and reports 127/126 as the command's exit.
- **Process identity is the PID plus its start time** (`/proc/PID/stat` field 22,
  counted from the last `)`), and signals go through a pidfd (Linux 5.3), which can't
  be split from the check by PID reuse; without pidfds, `kill` after the check.
- **`shipyard-runtime` requires a `pid` namespace** (step 7): without one, `kill` and
  `delete` reach only the top process, and nobody is left to explain a program that
  can't run (`shipyard run --bundle` still runs such bundles: its supervisor reads the
  report itself). **`kill STOP` is refused,** suggesting TSTP: SIGSTOP can't be caught,
  so it would stop the init, not the program.
- **Status from the directory:** no init recorded → *creating*; `exit`, or an init that's
  gone, a zombie or another process → *stopped*; the FIFO → *created*; else *running*.
  `state` shows the PID only while created or running.
- **`delete`:** stopped → removed; created → SIGKILL, wait, removed (as runc); running →
  refused without `--force`, which kills it first.
- **The binary:** `--root` (default `/run/shipyard`), `--log` and `--log-format` (errors
  and warnings appended, as text or JSON lines); `create --bundle DIR --pid-file FILE
  ID` (`--console-socket` refused), `start`, `state`, `kill ID [SIGNAL]`, `delete
  [--force]`. A failed call exits 1, a command line it can't parse 2.
  `docs/user/runtime-cli.md` is generated from the flags, like `cli.md`. No `list`,
  `exec`, `pause` or terminals yet.
- **State files are written atomically** (a temporary, then `rename`); the PID file's
  temporary is created `O_EXCL`, and a PID file that can't be written fails `create`
  and deletes the container (as runc does). IDs are runc's
  (`[A-Za-z0-9][A-Za-z0-9_.+-]*`, at most 255). The state root must be a real directory
  owned by the caller, writable by nobody else; `ROOT/ID` must be a directory itself.
- **The CLI:** `--bundle` conflicts with `COMMAND` and every container option (`-e`,
  `-w`, `-u`, `--group-add`, `--ulimit`, `--hostname`, `--domainname`, `--sysctl`,
  `--rootfs`, `--read-only`, `--no-isolation`); `--stop-signal` and `--stop-timeout`
  still apply. `--domainname` and `--sysctl` also conflict with `--no-isolation`.
  `--user USER[:GROUP]` and `--group-add` take names or numbers.
- **Library messages never name a CLI flag** (stage 2's rule, enforced again in step 7):
  the CLI adds `hint: --no-isolation` only where it works (`only_namespaces`: no root
  filesystem, hostname, domain name or sysctls).

**Considered and not done, with the reason:**

- **Warning on the six standard mounts:** every `runc spec` config lists them, so a
  warning would show on every run. Documented instead.
- **Following a symlinked `proc`, `sys` or `dev` inside the root** (runc does): those are
  where the kernel interfaces go, and nothing legitimate makes them symlinks.
- **Remounting a host `noexec` submount to make programs run:** respect the host's
  choice; explain the EACCES at exec by naming the option (probe 14).
- **Signalling the command directly for `kill STOP`** (through `/proc/<init>/task/…/
  children`): fragile; the cgroup freezer (stage 5) does it properly.
- **A test hook to make a setup hang:** the hung-setup test wins a real race instead,
  retrying when it loses.

## Kernel checks

Checked before building on them, with
[`scripts/probes/rootfs.c`](../../scripts/probes/rootfs.c): Linux 7.0, OrbStack, arm64,
in a privileged `rust:1-bookworm` container, six runs with identical results and two
under x86-64 emulation. Each check runs in its own mount namespace; the probe leaves the
mount table and `/tmp` as it found them. (One early run showed nothing: its output went
through a `grep` that would also have swallowed a Docker error. The probe prints its
failures. The probes compile warning-free under `-Wall -Wextra`.)

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| 1 | `pivot_root` into a plain directory | EBUSY; after bind-mounting it onto itself, ok | Always bind the root filesystem onto itself first |
| 2 | `pivot_root` with `/` shared, and with `/` slave | EINVAL when shared; ok when slave | Stage 2's rslave fits; it must come before the pivot |
| 3 | `pivot_root(".", ".")`, then `umount2(".", MNT_DETACH)` | ok; host paths are gone; two mounts left (the root and `/proc`) | No `oldroot` directory; the host's tree is unreachable |
| 4 | `/proc` mounted under the new root before the pivot, in a new PID namespace; then unmounted from inside | only the namespace's processes; once unmounted, the root filesystem's own empty directory, nothing of the host's | Closes stage 2's "`/proc` over the host's" gap |
| 5 | `devpts` with `newinstance,ptmxmode=0666`, `/dev/ptmx` → `pts/ptmx` | a pty is created in the container's own instance | `/dev/pts` of its own |
| 6 | `mqueue` without and with a new IPC namespace | the host's queues without; only the container's with | `/dev/mqueue` only with the IPC namespace |
| 7 | `sysfs` without and with a new network namespace | both allowed; without, the host's network devices (2 here); with, only `lo` | `/sys` read-only, the limitation recorded until stage 7 |
| 8 | Read-only bind remount (`MS_BIND`, then `MS_REMOUNT\|MS_BIND\|MS_RDONLY`) | writes below get EROFS; submounts of a recursive bind stay writable | Submounts need restricting too |
| 8b | **The same remount on a `nosuid,nodev,noexec` mount** | **silently drops all three**; repeating them keeps them | Never replace flags blindly: read them, or use `mount_setattr` |
| 9 | `/dev/null` over a file; a read-only tmpfs over a directory | the file reads empty; the directory shows nothing | How masked paths work |
| 10 | `mknod` in a tmpfs as root; the same with `MS_NODEV` | works; with `nodev`, `mknod` works but opening gives EACCES | `/dev` is a tmpfs *without* `nodev` (but `nosuid`) |
| 11 | **Mounting at a symlink planted in the root filesystem** | **the mount lands outside the root filesystem** | Never resolve mount targets by path |
| 12 | `openat2(RESOLVE_IN_ROOT \| RESOLVE_NO_MAGICLINKS)`, then mounting through the descriptor | the symlink resolves *inside* the root; mounting through the descriptor works. **Under x86-64 emulation `openat2` is ENOSYS** (also before 5.6, and under some seccomp profiles) | `openat2`, with a walking fallback |
| 12b | `openat2(RESOLVE_IN_ROOT)` of `../../../etc` | the root's own `/etc` | `..` can't escape either |
| 13 | A mount target that doesn't exist | ENOENT | Make targets, safely, inside the root |
| 14 | A root filesystem with a submount on `noexec` | EACCES at exec; root *could* remount | Don't remount; name the option in the error |

**Under a user namespace (stage 4) some of these change:** creating device nodes (10) and
mounting `sysfs` without owning the network namespace (7) are refused. The design leaves
room for bind-mounting the host's device nodes and `/sys` instead.

**Also checked during the reviews:**

- a bind remount without `MS_RDONLY` makes a *per-mount* read-only host mount writable,
  while a *superblock* read-only one (`mount -t tmpfs -o ro`) stays read-only;
- `mount_setattr` exists natively (EINVAL for dummy arguments) and is ENOSYS under
  x86-64 emulation; it leaves the other flags, access times included, as they are
  (probed in step 8: `rw,nodev,noatime` became `ro,nodev,noatime`);
- a blocked signal is queued even when its disposition is "ignore" (a SIGPIPE from the
  gate's EPIPE would reach the command later);
- a FIFO's blocked writer is let through once a reader *has* opened it, even one that
  has gone since; POLLERR on a pipe's write end means no readers, and `FIONREAD` works
  on either end;
- creating a file in a removed directory, through a descriptor still open on it, fails
  with ENOENT;
- `O_CREAT|O_EXCL` fails on a symlink, dangling or not, and creates nothing at a dangling
  link's target (probed in step 8);
- a non-dumpable process can still use its own `/proc/self/fd` (the remount fallback for a
  file bind goes through it, and `a_read_only_file_bind` passes under emulation, where
  that fallback runs);
- Rust's startup reopens closed fds 0–2 on `/dev/null`;
- PID 1 must reap: a container whose PID 1 is `timeout` leaves the tests' killed
  processes as zombies (hence `docker run --init` in `scripts/test-linux.sh`).

## macOS and Linux differences

| Topic | Linux | macOS |
|---|---|---|
| Root filesystems, bundles, the runtime | as above (root) | `--rootfs`, `--bundle`, `--domainname`, `--sysctl` are errors; `shipyard-runtime` returns `LinuxOnly` for every call |
| Names with `--no-isolation` | the host's `/etc` | the host's `/etc` (`nobody` is `-2`, read as 4294967294) |
| rlimit values | `u64::MAX` is unlimited | also; values from 2^63 up are too large |
| musl | builds (no `ST_RELATIME`, so it's not used) | — |
| x86-64 emulation (OrbStack's Rosetta) | no `openat2` (the walk runs), no `mount_setattr` (the remount fallback runs), no `execveat`; the emulator reports a missing loader itself (`a_missing_loader_is_explained` skips); `/proc/PID/cmdline` starts with the emulator's path; setups are slow (the tests' deadlines allow 30 s) | — |

## Failure-mode register

| Worst case | Handling | Test |
|---|---|---|
| A planted symlink redirects a mount to the host | `O_NOFOLLOW` targets; the resolver; mounting through descriptors | `a_symlinked_mount_point_is_refused`, `a_destination_through_a_symlink_stays_inside` |
| A destination led into `/proc` or `/sys` by a symlink | `fstatfs` of the target: procfs/sysfs refused (EXDEV) | `a_destination_led_into_proc_is_refused` |
| `openat2` missing | the walk | resolver unit tests on both paths; the emulated runs |
| A read-only remount drops `nosuid,nodev,noexec` | flags kept (`mount_setattr`, or `statvfs` repeated) | `read_only_keeps_the_mounts_other_options` |
| Restricting a bind makes a host read-only mount writable | `mount_setattr` only sets; the fallback keeps `ST_RDONLY` | `restricting_a_bind_keeps_the_hosts_read_only_mounts` (native and emulated) |
| Submounts left writable (`rbind,ro`, read-only paths) | `AT_RECURSIVE`, or remounting each mount below | `a_read_only_rbind_covers_the_mounts_below`, `read_only_paths_cover_the_mounts_below` |
| A read-only *file* bind | same path, `AT_EMPTY_PATH` on the file | `a_read_only_file_bind` |
| Host mounts under the targets (a chroot's `/proc`, `/dev`, `/sys`) | detached first, in the container only | `host_mounts_on_the_targets_are_detached_inside` |
| Containers starting together on a fresh root filesystem | EEXIST is "already made" | `containers_can_start_together_on_a_fresh_root_filesystem` (8 at once) |
| `--rootfs /` | refused (`RootIsHostRoot`) | `the_hosts_own_root_is_refused` |
| A root filesystem on `noexec`, or missing its loader | hints naming the option or the loader | `a_noexec_root_filesystem_is_explained`, `a_missing_loader_is_explained` |
| A path with a NUL byte | rejected by validation | `a_root_needs_a_mount_namespace_and_plain_absolute_paths`, `mounts_are_checked` |
| `config.json` a FIFO, huge, or swapped after the check | non-blocking open, checked once open, read with a limit | `only_small_regular_files_are_read` |
| A comma in a tmpfs value smuggles an option | refused | `bad_mounts_are_errors` (mutation-checked) |
| Fields the spec defines but Shipyard ignored silently | refused or warned | `what_would_confine_less_is_refused_all_at_once`, `what_doesnt_confine_is_ignored_with_a_warning` |
| An unreadable or hostile `/etc/passwd` | only names fail; fill-ins fall back | `only_names_need_readable_files`, `a_table_that_isnt_a_small_regular_file_is_an_error` (mutation-checked) |
| ID 4294967295 in the files | refused for a named user/group, skipped as a membership | `the_leave_unchanged_id_is_refused` (mutation-checked) |
| A sysctl that would change the host | refused, never ignored | `sysctls_that_would_change_the_host_are_refused` |
| A setup failure that `--no-isolation` can't fix | no hint | `a_refused_sysctl_value_is_named`, `only_isolation_failures_suggest_no_isolation` |
| A panic in the clean image passing for the command's exit | the hook reports `InitPanicked`, exits 125 | checked with an injected panic, debug and release |
| The hidden entry points started by hand | do nothing (sealed-memfd check; the monitor's argument check) | `the_init_entry_point_by_hand_does_nothing`, `the_runtime_binary_is_its_own_monitor_and_init` |
| Shipyard dies during the setup without a PID namespace | `getppid()` check before `exec` | mutation-checked (no test can hit the window) |
| A stale monitor writes into a newer container with the same ID | writes through its directory descriptor | `a_container_made_again_at_once_is_the_new_one` (deterministic: the old monitor is stopped across the delete), `a_removed_directory_takes_no_more_records` |
| A setup that hangs | `delete` kills the recorded monitor; the init follows | `deleting_a_container_whose_setup_hangs` |
| A lock holder that's stopped | commands give up after 10 s ("busy") | checked by hand |
| A command locks a removed and replaced directory | `same_directory` after locking | — (race) |
| A `start` that dies at the gate | the byte must be taken; SIGPIPE discarded | `a_start_that_dies_at_the_gate_starts_nothing` |
| The monitor holding `create`'s pipes | closes every inherited descriptor | `the_monitor_keeps_nothing_of_the_callers` |
| An unsafe state root, or a symlinked `ROOT/ID` | refused | `an_unsafe_state_root_is_refused` |
| A reused PID | start time differs: *stopped*, never signalled | `this_process_is_its_own_init` |
| A Ctrl-C meant for `create`, or its terminal closing, reaching the monitor | `setsid` in `pre_exec`: its own session and group before it runs | `the_monitor_has_a_session_of_its_own`, `the_container_outlives_its_creator` |
| A SIGPIPE pending at the re-exec, discarded by Rust's startup | checked before the exec, passed as `sigpipe`, raised again | by injection (exit 141 with the fix, lost without) |
| A PID file that can't be written | `create` fails and deletes the container | `a_pid_file_that_cant_be_written_leaves_nothing` |
| A planted file at the PID file's temporary name | `O_EXCL`; not removed | checked by hand |
| A bundle without a `pid` namespace in the runtime | refused | `a_bundle_without_a_pid_namespace_is_refused` |
| `kill STOP` | refused | `stop_is_refused` |
| The runtime's warnings lost | printed and logged | `warnings_and_errors_reach_stderr_and_the_log` |

## Known limitations (by design, for stage 3)

- **Not a security boundary yet.** The command runs as root with every capability (stage
  4 drops them); a root filesystem keeps it from *seeing* the host by accident, not from
  breaking out.
- **`/sys` lists the host's network devices** until stage 7.
- **No terminal inside:** the container's `/dev/pts` doesn't hold the host's terminal, so
  `tty` prints "not a tty"; reading, writing, Ctrl-C and window size work. A runtime
  container created from a terminal keeps it after `create` returns (`runtime.md` says to
  redirect).
- **Signals other than KILL sent to a *created* container** wait until it starts (runc
  kills it on TERM). `delete` of a *created* container kills it, as runc does, though
  the spec says it must fail.
- **A missing working directory** is noticed at `start`, not `create`.
- **Recorded residuals:**
  - the remount fallback (kernels before 5.12) finds the mounts below a bind by path, so
    a hidden mount can make the setup fail, and it can't see `nosymfollow` (5.10, 5.11);
  - `/dev` and `/etc/passwd` are opened by name after mounting, and `read_table` opens
    before checking the type (a device node there would be opened by root);
  - `/proc` from another PID namespace would make the start-time checks look at the
    wrong processes;
  - start times are in clock ticks: a PID reused within 10 ms passes the check where
    there are no pidfds;
  - `state` between `mkdir` and the first record says `"bundle": ""`;
  - a `start` killed between reading the gate's byte and removing the FIFO leaves the
    container *created* while it runs;
  - `create` killed between spawning the monitor and recording it leaves a monitor
    `delete` can't find;
  - runc's global flags (`--systemd-cgroup`, `--debug`) and `create --no-pivot` /
    `--preserve-fds` are command-line errors.

## Carried forward

| Stage | Item |
|---|---|
| 4 (security) | Capabilities, `noNewPrivileges`, seccomp: makes the init's non-dumpable flag effective. User namespaces: `/dev` from bind-mounted host nodes and `/sys` from a host bind (probes 7, 10); `proc`/`sysfs` mounted before the pivot already. Open `/dev` and `/etc/passwd` through descriptors (`O_PATH`, `fstat`, then reopen). runc's `--no-new-keyring`. |
| 5 (cgroups) | `linux.resources`, hooks, `oomScoreAdj`, `cgroupsPath`, the cgroup mount. `pause`/`resume` and `kill STOP` (the freezer), bundles without a `pid` namespace, killing every process of a container. Reaping monitors in a long-lived engine. |
| 6 (images) | Root filesystems from images. |
| 7 (networking) | `/sys` without the host's network devices; `net.*` sysctls; `netDevices`. |
| 8 (terminals, `exec`) | A terminal inside (`process.terminal`, `--console-socket`). Names for joined commands (a read-only safe resolver). |
| 14 (conformance) | runc's global flags and `--preserve-fds`; the OCI conformance suite against `shipyard-runtime`. |
| Housekeeping | Not caught by any mutation: a shared instead of exclusive lock (no test can cause the interleavings it guards). Not observable: the memfd's sealing (nothing else holds it), `O_NOFOLLOW` on the sysctl write (`/proc/sys` holds no symlinks). |

**Resolved from stage 2:** `pivot_root` (the host's `/proc` no longer under ours, and
rslave's "host mounts appear inside" moot with the old root gone); the domain name and
namespaced sysctls from `config.json`; the working directory and user/group lookups
resolved inside the root filesystem. Stage 2's copy fallback and `pass_on_pending` went
away with the re-exec-first init (a failed re-exec is `ChildStep::Reexec`, exit 125; at
the re-exec no command exists yet, so pending signals stay pending across `exec`, except
SIGPIPE: the clean image's Rust startup ignores it, which discards a pending one. The
child therefore checks for a pending SIGPIPE just before the exec and passes a `sigpipe`
argument, and the clean image raises it again; step 8 found the gap).
**Resolved from stage 1:** `u64::MAX` rlimits mean unlimited (`RlimitValue::from_oci`);
`Env` is indexed (a `HashMap` from key to position, and `PartialEq` compares the
variables in order; 100 000 variables set twice take well under a second); `ProcessSpec::new` and `User::new` constructors, both
`#[non_exhaustive]`.

## Tests

| Kind | Where | What |
|---|---|---|
| Unit | `crates/shipyard-runtime/src/**` | the setup round trip and memfd seals; `Root`/`Mount`/sysctl validation; the resolver on both paths; `oci.rs` (full config, refusals, warnings, mounts, broken configs, FIFO and size); identity parsers and every rule; `unescape_mount_point`; `DEV_ENTRIES`; failure messages; the lifecycle (IDs, start times, status, a removed directory) |
| Root filesystem | `crates/shipyard-cli/tests/rootfs.rs` | 17 root tests and 4 as any user |
| Bundles | `crates/shipyard-cli/tests/bundle.rs` | 13 root tests and 6 as any user |
| Names | `crates/shipyard-cli/tests/names.rs` | 10 root tests and 2 as any user |
| Runtime | `crates/shipyard-cli/tests/runtime.rs` | 20 root tests and 4 as any user |
| Terminal | `crates/shipyard-cli/tests/terminal.rs` | 16 ordinary and 15 `ROOT_TESTS`, four of them new (`--rootfs` Ctrl-C, window size, "not a tty"; a runtime container on a terminal) |
| Joining and no-PID-namespace | `crates/shipyard-runtime/tests/containers.rs` (was `join.rs`) | `container`, `container-nopid` (`--churn`) and `join` modes: the command as shipyard's own child with an exact status, 20 starts while another thread creates threads, `InitDidNotStart` through `ld.so`; a joined command lands in the container's root filesystem. (These replaced a unit test: the unit-test binary doesn't call `init_entry_point()`, so the re-exec would start libtest.) |
| Kernel | `scripts/probes/rootfs.c` | the checks above |

The fixtures are in `crates/shipyard-test-support`: `RootFs` (a root filesystem copied
from the host's `sh`, `cat`, `ls`, `stat`, `touch`, `head`, `sleep`, `readlink`,
`mkdir`, `wc`, `stty`, `tty`, with the libraries `ldd` lists, at the same paths) and
`Bundle`.

**Test mechanics worth knowing:**

- The masked-file check uses whichever masked files the kernel has (OrbStack's has no
  `/proc/kcore`).
- The terminal tests' session script runs in the forked fake shell and can't capture
  variables, so the root filesystem's and bundle's paths travel in the environment, set
  before the fork: sound only because that test binary has no other thread.
- `deleting_a_container_whose_setup_hangs` makes a hang without a test hook: it holds
  the container's lock (the monitor needs it to record the init), waits for the init,
  stops the monitor, and lets go. The monitor can win the race (a setup takes about a
  millisecond), so the test detects a lost race and retries, up to 20 times; a pass
  proves the hang happened (ten native runs in a row). Waiting for the init before
  stopping the monitor came from its first emulated run, where the slow monitor hadn't
  spawned it yet.
- `a_container_made_again_at_once_is_the_new_one` stops the old monitor across the
  delete and the new create, so its late write certainly comes after.

**Mutation-checked.** Every important fix was removed on a scratch copy (`rsync -rlp`,
not `-a`: kept file times let a stale build be reused) with its own build cache, and its
test confirmed to fail:

| Mutation | Caught by |
|---|---|
| The report end left close-on-exec | `results_come_back_through_the_namespaces`, `only_isolation_failures_suggest_no_isolation` |
| No disposition reset in `command_main` | `the_command_does_not_inherit_the_inits_dispositions`, `signals_sent_to_the_init_reach_the_command` |
| The init keeps the original stdio / its report end | `the_init_does_not_hold_stdio`; the three forged-record tests |
| The `getppid` check inverted | the no-PID-namespace tests |
| No detach of the old root; nothing detached from the targets | `no_host_mount_is_left_inside`; `host_mounts_on_the_targets_are_detached_inside` |
| A non-recursive root bind | `the_root_filesystems_own_mounts_come_along` |
| Flags not kept on the read-only remount; the read-only flag not kept (emulated) | `read_only_keeps_the_mounts_other_options`; `restricting_a_bind_keeps_the_hosts_read_only_mounts` |
| Masks skipped; read-only paths not recursive | `masked_and_read_only_paths`; `read_only_paths_cover_the_mounts_below` |
| Targets without `O_NOFOLLOW`; no `/proc` check on targets | `a_symlinked_mount_point_is_refused`; `a_destination_led_into_proc_is_refused` |
| `sysfs` read-write | `sys_is_read_only` |
| EEXIST from `mkdirat` an error again | `containers_can_start_together_on_a_fresh_root_filesystem` |
| The walk follows an absolute symlink on the host | `a_destination_through_a_symlink_stays_inside`, `symlinks_resolve_inside_the_root` |
| No remount below an `rbind`; no remount of a bind's flags | `a_read_only_rbind_covers_the_mounts_below`; `a_read_only_bind_from_the_host` |
| `capabilities` accepted; `u64::MAX` a number | `a_runc_spec_config_is_refused_with_its_reasons`; `the_process_is_the_configs` |
| A comma allowed in tmpfs values; `asks_nothing` removed | `bad_mounts_are_errors`; `what_doesnt_confine_is_ignored_with_a_warning` |
| Memberships ignored; `HOME` overwritten; the last entry wins | `a_user_name_resolves_in_the_containers_files`; `the_users_own_home_wins`; `a_name_gives_the_uid_group_home_and_memberships` |
| Any sysctl allowed; the domain name not set | `sysctls_that_would_change_the_host_are_refused`; `the_domain_name_is_the_containers_own` |
| An unreadable `/etc/passwd` failing every run; 4294967295 accepted | `only_names_need_readable_files`; `the_leave_unchanged_id_is_refused` |
| No start-time check | `this_process_is_its_own_init`, `the_status_follows_the_state_directory` |
| The FIFO kept by `start`; the report end open past the gate | `the_lifecycle` and others; the suite hangs |
| No `setsid` for the monitor; inherited fds kept; an unsafe root accepted | `the_container_outlives_its_creator`; `the_monitor_keeps_nothing_of_the_callers`; `an_unsafe_state_root_is_refused` |
| The old gate (going on after any write) | `a_start_that_dies_at_the_gate_starts_nothing` |
| `delete` not killing a hung setup's monitor | `deleting_a_container_whose_setup_hangs` |
| The monitor finding its directory by path | `a_container_made_again_at_once_is_the_new_one` |
| The re-exec fails | by hand: the message, the hint, exit 125 |

**Verified at the end of the stage:** fmt; clippy on macOS, aarch64 and x86-64 Linux,
and musl; rustdoc; both generated references; the macOS suite; `scripts/test-linux.sh`
(everything, as a normal user and as root) several times; the terminal, root
filesystem, bundle, names and runtime suites under x86-64 emulation (names 11/11 and
the identity unit tests 8/8 when step 5 landed).

**CI:** the `root` job runs `--include-ignored` on Ubuntu (where `/` is shared) and
macOS, and prints the host facts the fixtures rely on (kernel, glibc, `/`'s
propagation). **Locally:** `scripts/test-linux.sh` runs clippy, the normal-user pass
(through `setpriv`), then everything as root with `/` made shared, in a privileged
container with `--init`, installing nothing. `--shell` and `--root-shell` open a shell
there. The x86-64 runs use `docker --platform linux/amd64` with their own volumes.

## Review history

Each step had a clean-code review and a worst-case ("Murphy's law") review; step 7 added
a full review by three read-only reviewers with separate briefs (kernel and security,
processes and protocols, contract and docs), every finding verified first.

- **Step 1 (probes):** claims the probe hadn't tested were tested (4, 12b); the hazards
  it hadn't looked at were added: 8b (**the** find: a remount drops flags), 14, and the
  user-namespace changes. The probes became warning-free; `join.c`'s undefined-behaviour
  cast of `pause` became a thread function. `serde`'s licences (and `itoa`, `memchr`,
  `ryu`, `proc-macro2`, `quote`, `syn`, `unicode-ident`) are MIT or Apache-2.0, and their
  minimum Rust versions (`serde` 1.56, `serde_json` 1.71) are below the workspace's 1.98.
- **Step 2 (re-exec first):** no new feature; every stage-2 test passed unchanged.
  - review: the isolation is validated again before the setup is written;
    `CleanImageFiles` replaced a tuple; `read_setup` returns an errno; stale comments;
  - worst case: the `getppid` gap without a PID namespace; the panic hook; the hidden
    entry point acting only on a sealed memfd. Checked: closed stdio, a leaked fd 900,
    SIGKILL at random moments (30 runs), an ignored SIGHUP. Accepted: without a PID
    namespace, the command could forge a status record (the exit-code cross-check
    holds, and it can signal shipyard anyway).
  - a 200 KiB environment arrives whole (one string over 128 KiB is refused by
    `execve`, `MAX_ARG_STRLEN`); 20 starts while another thread creates threads (skipped
    under Rosetta, which deadlocks in its own `clone`).
- **Step 3 (`--rootfs`):**
  - review: `/dev`'s failure indexes looked up by name (`dev_entry`) instead of a
    counter that could drift; `Target` an enum; one way to fail in the init;
    `root_filesystem()` for messages;
  - worst case: EEXIST from containers starting together (3 runs in 12); the host's
    mounts under the targets (two `/dev` and two `/proc` inside); a host sysfs on `sys`
    (EBUSY); `--rootfs /` refused; mounting through `fchdir` + `"."`; no
    `--no-isolation` hint with `--rootfs`; NUL bytes in paths. Checked: SIGKILL at
    random moments (20 runs), a read-only root filesystem without `/proc`, `proc` as a
    file, `dev` as a symlink, the command's umask.
- **Step 4 (bundles):**
  - review: `add_mount` split; one `io_errno`; `fd_path`; an exhaustive `MountKind`
    match; the `FLAG_OPTIONS` table; `unescape_mount_point` tested;
  - worst case: the config swapped for a FIFO between check and read; a comma
    smuggling a tmpfs option; `{"devices": []}` refused; a `cgroup2` mount refused; "it
    has no type". Checked: a NUL in `args`, a symlink inside a bound host directory, 8
    bundles making the same mount points, `rbind,ro` over three levels of host mounts, a
    bound device file.
  - housekeeping: the mutation driver's `rsync -a` could reuse a stale build (no result
    affected: every mutation was in one crate); now `-rlp`, and one target per mutation
    on macOS.
- **Step 5 (names, domain name, sysctls):**
  - review: `identity::resolve` split (the passwd lookup had been written three times);
    a home with a NUL ignored instead of failing silently; `user_label`/`group_label`;
    `sysctl::apply_at`;
  - worst case: an unreadable `/etc/passwd` stopped every run (now only names fail);
    4294967295 from the files hidden behind "reading its setup: Invalid argument" (now
    named); negative IDs skipped (now read as stored); the `--no-isolation` hint shown
    where it can't work (`only_namespaces`). An existing test (`run.rs`) now works out
    `HOME` from `/etc/passwd`.
- **Step 6 (`shipyard-runtime`):**
  - a test bug: a `Bundle` dropped as a temporary deleted its root filesystem before
    `start` ("command not found");
  - review: the container module split by platform (`lifecycle.rs`, `elsewhere.rs`,
    `ContainerError::LinuxOnly`); `POLL_INTERVAL`; bundles kept in locals;
  - worst case: the monitor held `create`'s inherited pipes (`3>&1 … | cat` hung); an
    unsafe state root and a symlinked `ROOT/ID`; `create` held the lock during the whole
    setup; a Ctrl-C before the monitor's `setsid`. Checked: `create` killed mid-way, the
    monitor killed before answering, `start` against a dead init, a reused PID, a stale
    state root after reboot;
  - runs: two failures came from `timeout` as the container's PID 1, which never reaps
    (hence `--init`); under emulation the monitor's `cmdline` starts with the emulator's
    path.
- **Step 7 (terminals, CI, docs, the full review):**
  - the full review's fixes: restricting a bind made host read-only mounts writable
    (reproduced: both writes went through; `mount_setattr`); read-only paths left
    submounts writable; a destination led into `/proc` by a symlink; `type: bind` was
    recursive; the init became non-dumpable late; the PID file's predictable temporary;
    a stale monitor writing into a newer container; a hung setup that `delete` couldn't
    stop; a lock on a replaced directory; a stopped lock holder blocking everyone; a
    dead `start` at the gate starting the container (or SIGPIPE-ing it); a panic after
    the gate passing for the program's exit; `kill STOP`; bundles without a `pid`
    namespace; and the contract: `cargo run` ambiguous with two binaries
    (`default-run`), the runtime dropping warnings, "or without isolation" where it
    can't work, spec fields ignored silently, `unbindable` refused for a false reason,
    access-time options refused, and the docs (install, exit codes, conflicts, `PATH`,
    the state root's owner, FEATURES, the README);
  - review: `lock_at` returns a plain result (the monitor had passed `""` as an ID);
    `same_directory`; `add_restrictions`; `wait_at_gate` split into its protocol;
    `refuse_if_asked`; an `eventually` test helper;
  - worst case: the hung-setup test itself was racy (one native failure after five
    passes: the monitor recorded the init first); it now retries a lost race and passes
    only when the hang happened. The recreate test depended on a 50 ms sleep; it now
    stops the old monitor across the delete. `lock` called any open failure "no
    container"; the PID file's cleanup removed files it hadn't created. New tests: a
    read-only file bind, `create` with its stdio closed. Under emulation the hung-setup
    test first failed: the slow monitor hadn't spawned the init when it was stopped.
    Checked and fine: `delete --force` while the old monitor is stopped (the init is a
    zombie, which counts as gone, so the delete finishes and the monitor's late write
    lands nowhere); masked paths under a read-only path (a `/dev/null` bind) become
    read-only too, so writes fail instead of vanishing; a destination reached through a
    bind of the host's `/proc` is refused by the `fstatfs` check as well (conservative,
    and the message says why); the panic hook's `exit` can't deadlock on a held stdout
    lock (Rust's exit cleanup only tries it).
  - found while writing the manual tests: a PID file that couldn't be written left the
    container *created*.
- **Step 8:** these notes, the book (chapters 13–15), `scripts/test-linux.sh
  --root-shell` (chapter 12's "Try it" used `--shell` and `sudo`, which step 7's
  apt-free script no longer had).
  - The rewrite was checked fact by fact against the old step-by-step notes (544 facts):
    67 had been dropped and one misattributed; all were put back.
  - Chapter 12's "Try it" was re-run against today's code: six blocks had drifted
    (stage 3's re-exec, renamed test binary, new tests, the shared `/` of the root
    shell), now updated.
  - The book agents, re-running everything for real, found four code issues, all
    fixed: the monitor never got a session of its own (`setsid` after
    `process_group(0)` fails with EPERM, ignored; now `pre_exec`, tested by
    `the_monitor_has_a_session_of_its_own`, which failed before); a SIGPIPE pending
    at the re-exec was discarded by Rust's startup, a regression from stage 2's
    `pass_on_pending` (now passed on; checked by injecting one: exit 141 with the fix,
    lost without); `config.json` errors gave the position twice (tested); the init was
    named `shipyard-runtim` until the gate (now `shipyard-init` from the start). The
    probe failed where `/` is shared (the root shell's, a systemd host's): a check's
    mount propagated out and blocked the cleanup; each check now makes `/` private
    first, and the results are identical either way. Docs: a `runc spec` config also
    asks for a terminal and a device rule; a bundle ID of 4294967295 is refused;
    `--log` writes warnings too; the monitor shows as `shipyard-monitor` in `ps -f`.
  - Worst case: `scripts/test-linux.sh` died at once under macOS's own `/bin/bash` 3.2
    (an empty array is "unbound" to `set -u` before bash 4.4); Homebrew's bash had hidden
    it. Fixed with `${a[@]+"${a[@]}"}`, and checked with `/bin/bash`. Two facts these
    notes had only reasoned (`mount_setattr` and access times, `O_EXCL` and symlinks)
    were probed.

**Lessons worth keeping:**

- **A remount replaces; it doesn't add.** Any flag not repeated is lost, read-only
  included. Prefer an interface that only sets (`mount_setattr`).
- **Never reach a container's files, or a container's state, by path** once something
  else can change what the path means: resolve inside the root, mount through
  descriptors, write state through a directory descriptor.
- **"Opened" is not "read":** a FIFO's writer is let through by a reader that may be
  gone. Confirm the other side did its part.
- **A blocked signal is kept even when ignored;** discard what you cause yourself.
- **A test that depends on timing proves nothing when it passes.** Make the order
  certain (stop a process, hold a lock), or detect a lost race and retry.
- **PID 1 must reap**, in the containers we build and in the ones we test in.
- **Check the reproduction reproduces the right thing:** a superblock read-only mount hid
  the per-mount bug.
- **Mutation runs need their own build cache, and a copy that doesn't keep file times.**

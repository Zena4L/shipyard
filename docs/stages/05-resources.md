# Stage 5 notes: resource control

The engineering record of stage 5: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [Cgroup](../user/run.md#cgroup),
[Limits](../user/run.md#memory-cpu-and-io-limits), [Devices](../user/run.md#devices),
[Cgroups in bundles](../user/bundles.md#cgroups), [Hooks](../user/bundles.md#hooks),
[The OCI runtime](../user/runtime.md) and [Exit codes](../user/exit-codes.md).

## Outcome

Stage 5 asked for cgroups v2: limits on memory, CPU, processes and block I/O, OOM
reporting, pause and resume, and device access control ([ROADMAP](../ROADMAP.md),
[FEATURES §5](../FEATURES.md#5-resource-control--stage-5)). On Linux, every isolated container now gets a **cgroup of
its own**. It's born into it, sees only it, and the cgroup is gone when the container ends.
Root's containers get one below `/sys/fs/cgroup/shipyard/`, rootless ones inside the subtree
systemd delegates to the user. Limits are written into it before anything runs. The OOM
killer's work is reported. The runtime pauses, resumes, updates, lists and signals what's
in it. And an eBPF program on it decides which devices the container may use. OCI hooks,
warned about since stage 3, run at all six of their phases.

| Requirement | Where | Tested by |
|---|---|---|
| A cgroup per container, born into it (`CLONE_INTO_CGROUP`, else `cgroup.procs`), its own cgroup namespace and a read-only `cgroup2` mount, removed at the end | `cgroup/tree.rs` (`Tree::find`, `create`, `claim_run`, `sweep_stale_runs`), `cgroup/manage.rs` (`Cgroup`, `RunCgroup`, `remove`), `process/clone.rs`, `spawn.rs` (`before_go`), `init.rs`, `rootfs.rs` | `cgroups.rs`'s `a_run_*_has_its_own_cgroup`, `a_finished_run_leaves_no_cgroup`, `the_next_run_sweeps_cgroups_of_dead_runs`; `runtime.rs`'s `a_container_has_a_cgroup_for_its_life` |
| Limits: memory, swap (Docker's rule), CPU (`--cpus`, shares, quota, cpuset), pids, block I/O (weight, rates) | `resources.rs`, `cgroup/manage.rs` (`enable`, `apply`, `limit`), the CLI's `limits.rs` | `limits_are_in_the_containers_cgroup`, `swap_follows_dockers_rule`, `the_process_limit_holds`; unit tests |
| OCI `linux.resources` (with `unified`) and `linux.cgroupsPath` | `oci/resources.rs`, `oci/mod.rs`, `container/lifecycle.rs` (`limited_cgroup`) | `a_bundles_limits_are_its_cgroups`, `a_cgroups_path_is_honored`, `a_busy_cgroup_takes_no_container_below_it`; OCI unit tests |
| OOM reporting: `shipyard run`'s "the command was killed: out of memory", the runtime's exit record | `process/supervisor.rs`, `lifecycle.rs` (the monitor) | `the_oom_killer_is_reported`, `shipyards_own_kill_is_not_an_oom_kill`, `an_oom_kill_is_recorded`, `a_kill_after_an_oom_kill_is_a_kill` |
| Rootless cgroups, in the delegated subtree | `cgroup/tree.rs` (`choose_base`, `NoTree`), `Record.cgroup_base` | `rootless_cgroups_follow_delegation`, `an_undelegated_controller_is_named`, `a_rootless_containers_cgroup_follows_delegation`, `a_rootless_container_is_deleted_from_another_session` |
| The runtime's `pause`, `resume`, `update`, `ps`, `events [--stats]`, `kill --all` | `lifecycle.rs`, `cgroup/stats.rs`, the CLI's `runtime_commands.rs` | `pause_and_resume`, `a_created_container_pauses`, `kill_all_reaches_every_process`, `update_changes_limits_live`, `ps_lists_the_containers_processes`, `events_stats_are_runcs_json`, `events_report_an_oom_kill`, `rootless_cgroup_commands_follow_delegation` |
| `oomScoreAdj` and `--oom-score-adj` | `process/spawn.rs` (`before_go`) | `the_oom_score_adjustment_is_the_bundles`, `the_oom_score_is_lowered_from_outside` |
| OCI hooks, all six phases | `hooks.rs`, `init.rs` (`stop_for_hooks`, `start_container`), `spawn.rs` (`PendingHooks`), `lifecycle.rs` | `hooks_run_at_their_phases`, `a_run_runs_every_hook`, `poststart_runs_while_the_program_does`, `deleting_during_a_hook_stops_it`, and the hook tests below |
| Device access control: an eBPF program on the cgroup, runc's default list, `linux.devices`, `--device`, `--device-cgroup-rule` | `devices/` (`mod.rs`, `ebpf.rs`, `program.rs`), `cgroup/devices.rs`, `rootfs.rs` (nodes), the CLI's `devices.rs` | `devices.rs` (8 tests); the property test; `the_verifier_accepts_the_programs` |
| Terminals with cgroups | (no new code) | `terminal.rs`'s `ctrl_c_during_a_hook_ends_the_run`, `ctrl_z_and_fg_with_limits`, `an_oom_kill_on_a_terminal`, `pause_on_a_terminal`, as root and rootless |

**Moved out of stage 5:**

| Item | Now in | Why |
|---|---|---|
| `shipyard stats`, `top`, `pause`, `update` | stage 9 | they name containers, which `shipyard` keeps from stage 9 |
| Placing joined processes (`setns`) in the container's cgroup | stage 8 (`exec`) | joining is `exec`'s design |
| The systemd cgroup driver (`slice:prefix:name`), cgroup v1 | not planned | cgroupfs works under systemd's delegation; v1 is on its way out |

Paths below are relative to `crates/shipyard-runtime/src/` and `crates/shipyard-cli/`.

## Architecture

```
shipyard run (isolated) / run --bundle                    shipyard-runtime create
  supervisor.rs: make_run_cgroup                           lifecycle.rs: create
    Tree::find (root: /sys/fs/cgroup; rootless: the          Tree::find; no tree → refuse limits,
      delegated base, else run without, refuse limits)         or a bundle without a pid namespace
    sweep_stale_runs; claim_run → run/<pid>, flock-held      limited_cgroup: Tree::create(cgroupsPath
    limit (enable from the base down, then the files)          or runtime/<id>) → Record (path, base,
    enforce_devices (load, attach the device program)          inode) written at once → limit →
                                                               enforce_devices → start_monitor
  └─ spawn()                                                        │
       ├─ clone3(CLONE_INTO_CGROUP) — or clone, then           the monitor: container_setup (the
       │  cgroup.procs in before_go, with the maps and           record's cgroup and hooks) → spawn
       │  /proc/PID/oom_score_adj                                → record_init → reap → OOM? (not
       └─ "go"                                                   if `killed`) → remove the cgroup →
                                                                 the exit record
the clean image (init.rs)
  unshare(CLONE_NEWCGROUP) → rootfs::enter: mounts, cgroup2 read-only at /sys/fs/cgroup,
  device nodes (mknod, or the host's bound in a user namespace)
  → before_pivot: the AtHooks record; the parent runs prestart, createRuntime; the hooks pipe
  → createContainer → pivot_root → [gate] → startContainer (a confined child of the init)
  → the command

later calls (lifecycle.rs, under the state directory's lock)
  start: the FIFO's byte, FIFO closed → poststart (lock let go)
  pause/resume (cgroup.freeze), update (limit), ps, stats/events, kill --all (pidfds)
  kill/delete KILL: the `killed` marker; delete: remove the cgroup (if it's still the
  recorded inode) → poststop (lock let go)
```

## Decisions and why

**Made with the user, before starting:**

- **cgroupfs, no D-Bus.** Shipyard writes `/sys/fs/cgroup` itself, as runc's default driver
  does. Root's containers go below `/sys/fs/cgroup/shipyard/`; rootless ones below the
  caller's systemd-delegated subtree (`user@UID.service`), found from `/proc/self/cgroup`.
  No new crates.
- **cgroup v2 only.** Where cgroups can't be used (a v1 host, rootless without delegation,
  Shipyard inside a container that delegated nothing), a run without limits works as
  before; asking for a limit fails (exit 125) and says why; such a container has no pause
  or stats.
- **Device control is Shipyard's own eBPF, in Rust,** as seccomp is: an assembler and the
  program generated from the rules, loaded with the raw `bpf(2)` call, and testable on
  macOS.
- **Commands:** `shipyard run` gets Docker's limit flags and OOM reporting;
  `shipyard-runtime` gets runc's cgroup commands. `shipyard`'s own wait for stage 9.

**Made with the user, step by step:**

- **Missing controllers are turned on from the root down,** as runc's cgroupfs driver does.
- **Swap follows Docker:** as much as memory, unless `--memory-swap` says otherwise.
- **The OOM killer takes one process** (no `memory.oom.group`), as Docker and runc.
- **The runtime honours `linux.cgroupsPath`** in cgroupfs form; systemd's is refused.
- **Rootless containers go below the delegation root,** the highest cgroup above
  shipyard's own that the user owns. With no delegation (an SSH session's scope is root's),
  runs go on without a cgroup, and limits are refused with the fix, `systemd-run --user
  --scope shipyard run …`. A rootless bundle's `cgroupsPath` counts from the delegated base.
- **CI runs the normal-user tests inside the runner's systemd user manager.**
- **OCI hooks are their own step,** and faithful: `prestart` and `createRuntime` run before
  `pivot_root`, with the container's mounts ready; the container's init runs the
  in-container phases; `shipyard run --bundle` runs every phase, as `runc run` does; no
  default timeout.
- **`events` prints runc's JSON; `ps` reads `/proc` itself,** with runc's `--format json`;
  a paused container follows runc's rules (and, from step 8, a created one may be paused,
  as with runc).
- **Devices:** bundles and Docker's flags as the front ends; runc's default list (step 8
  made it exactly runc's); rootless tries, and goes without, warning only when rules were
  asked for; tested with Shipyard's own eBPF interpreter and property tests against a
  plain-Rust model.

**Engineering:**

- **The model** (`resources.rs`, cross-platform): `Resources`, one field per cgroup file,
  in the kernel's own terms. `validate` checks each value as the kernel would take it.
  The front ends' units are converted here as Docker and runc convert them: `parse_size`
  (exact, 128-bit), `cpus` (`--cpus 1.5` is `150000 100000`), `weight_from_shares` and
  `io_weight_from_blkio` (runc's formulas, 0 meaning "not set"), `swap_from_total`
  (Docker's memory-plus-swap, -1 unlimited). `files()` lists the files in the order they
  must be written: the cpuset first, OCI's `unified` last so it wins, `io.max` once per
  device.
- **Bundles** (`oci/resources.rs`): `linux.resources` as runc reads it. Swap is memory
  plus swap, `-1` no limit, `0` not set; shares and block I/O weights are converted to
  v2's ranges; OCI's "defaults" (`memory.kernel: -1`, `swappiness: -1`) are accepted.
  Refused by name: what cgroup v2 can't apply (kernel memory, swappiness,
  `disableOOMKiller`, real-time CPU, leaf weights, `network`), and what Shipyard doesn't
  support yet (`burst`, `idle`, per-device weights, huge pages, `rdma`). `unified` keys must be a controller's file: no `/`, and
  no `cgroup.*` (`cgroup.procs` or `cgroup.kill` would move or kill, not limit).
  `update -r` reads the same object (`oci::resources_from_json`), without device rules.
- **The init counts against `pids.max`:** with a limit of 1 it can't start the command,
  and the failure says so (`FailureContext.pids_max`).
- **One rule for where a container's cgroup goes** (`cgroup/tree.rs`). `Tree::find` gives
  the base: for root, the whole tree when it's writable, whoever owns root's own cgroup
  (`sudo` from a desktop terminal); for anyone else, the delegation root, with its
  `cgroup.procs` and `cgroup.subtree_control` writable. `choose_base` is the pure core.
  `NoTree` says what's missing (`NotV2`, `NoOwnCgroup`, `NotDelegated` with the fix,
  `NotWritable`).
- **Only parents hand controllers down, only the leaf holds processes** (probe G2). Shipyard's
  own parents (`shipyard/`, `run/`, `runtime/`) hand down everything, before the leaf
  exists; a bundle's `cgroupsPath` parents get only what its limits need. Nothing is ever
  written to a cgroup that holds processes (`holds_processes`), except the real root,
  which the kernel exempts.
- **Born in it:** `clone3` with `CLONE_INTO_CGROUP`; where there's no `clone3` (Docker's
  seccomp profile, QEMU), the parent writes the PID to `cgroup.procs` in the go-byte
  window. Nothing of the container ever runs outside its cgroup.
- **Its own view:** the clean image unshares the cgroup namespace once it's in the cgroup,
  so `/proc/self/cgroup` says `0::/`, and mounts `cgroup2` read-only at `/sys/fs/cgroup`.
  Without a root filesystem, an empty tmpfs covers the host's tree first: the kernel won't
  mount cgroup2 on its own mount.
- **Removal:** `cgroup.kill` (before 5.14, SIGKILL to each process, repeated every poll),
  wait for `populated 0`, `rmdir`, deepest first; 5 s, then left in place with a warning.
- **Liveness by lock:** a run holds an `flock` on `run/<pid>` for its whole life
  (`RunCgroup`). The sweep removes only what it can lock, and checks that the locked
  directory is still the one at the path; `claim_run` retries if not.
- **The record is the truth, but checked** (`Record`): the cgroup's path, its base and
  its inode, written into `state.json` right after `mkdir`, before the limits and device
  program (both from step 8), so a `create` killed half-way leaves a cgroup `delete` can
  find. `Cgroup::recorded(path, base)` accepts only a container cgroup below the base,
  never the root, shipyard's parents, `.`/`..`, the caller's own cgroup or one above it,
  or a base this user doesn't own; a directory with a different inode is someone else's and counts as gone.
- **Limits are written while the cgroup is empty,** before anything runs (`Cgroup::limit`
  = `enable` + `apply`). Every write goes through `write_file` (never `O_CREAT`), so a
  missing file is ENOENT and named (`NoSuchLimit`, `NoSwapAccounting`), not EACCES. A
  refusal partway says what was written before it (`PartlyApplied`).
- **OOM is a SIGKILL plus `oom_kill` going up** (probe G4), read before the cgroup goes.
  It's believed only for a SIGKILL Shipyard didn't send: the supervisor knows whether its
  stop timeout fired; the runtime leaves a `killed` file before any SIGKILL it sends
  (`kill`, `kill --all`, `delete`).
- **The cgroup commands** run under the container's lock (`with_cgroup`), so a `delete`
  can't come in between. `kill --all` sends SIGKILL through `cgroup.kill` and other
  signals through pidfds, checking each process is still in the cgroup first
  (`signal_member`). `freeze` waits for `cgroup.events`, and writes `0` back if the
  freeze doesn't land in time. `update --memory-swap` alone counts from the current
  memory limit (`container::memory_limit`).
- **`oom_score_adj` is written by Shipyard from outside,** before the go byte: going below a
  process's floor (`oom_score_adj_min`, normally 0) needs `CAP_SYS_RESOURCE` in the
  host's user namespace, which no process in a user namespace has.
- **Hooks** (`hooks.rs`, cross-platform): plain `fork` + `exec`, exactly the hook's `args`
  and `env`, the state on stdin (written non-blocking against the deadline), the bundle as
  the working directory, only stdio inherited; a `timeout` enforced by polling, then
  SIGKILL. The init stops before `pivot_root` with an `AtHooks` report record (`ChildStep`
  54) and waits on the hooks pipe while the parent runs `prestart` and `createRuntime`;
  `createContainer` runs in the init (its failure: `CreateContainerHook`, 55);
  `startContainer` runs in a child of the init that confines itself first
  (`StartContainerHook`, 56; `HookRunnerCapabilities`, 58, if it can't). Those two get
  the init's PID as the container sees it (1), the others the host's, as the spec says.
  `create` records the hooks, and the monitor, `start` and `delete` run that copy.
  `start` and `delete` let go of the lock before theirs. Failures of `poststart` and
  `poststop` are warnings, as spec 1.2 says (Shipyard's `ociVersion`; 1.3 makes a
  `poststart` failure stop the container).
- **How hooks see the container:** a hook outside it reaches it through `/proc/PID/root`
  before the pivot. The init isn't dumpable, but rootless, its memory belongs to the
  container's user namespace, whose owner (the caller) has every capability in it, so
  `ptrace_may_access` lets the caller's hooks in. Where every signal is blocked (the
  init), the SIGPIPE from a hook that doesn't read its stdin is taken back
  (`sigtimedwait` with a zero timeout), so it's never forwarded to the command.
- **Device rules** (`devices/mod.rs`): later rules win, per access bit; `Devices::allows`
  is the model. The default is runc's list exactly (`runc_allowed`): mknod of any device,
  `null`, `zero`, `full`, `random`, `urandom`, `tty`, `ptmx`, `/dev/pts/*`, `/dev/net/tun`.
  A bundle's rules come first, then that list; its `linux.devices` nodes get no rules of
  their own (runc's behaviour).
- **The device program** (`devices/program.rs`): one pass per access bit asked for, rules
  walked last to first, the first match deciding. Shaped for the verifier: nothing changes
  as rules match, each rule loads its fields fresh, and a rule covering every device ends
  its pass (no unreachable code). `MAX_RULES` is 256.
- **Device policy** (`cgroup/devices.rs`): real root (euid 0 with the identity uid map)
  fails closed on any refusal, lifting `RLIMIT_MEMLOCK` once on EPERM for kernels before
  5.11; anyone else goes without, warning only if rules were asked for. A refused load is
  retried with the verifier's log, whose last lines go into the error. All of this needs
  the container's cgroup: with none (v1, a read-only tree), root's container has no device
  control either, warned about only if rules were asked for, and the warning says that
  only its capabilities limit it (rootless: only the user's rights).

**Considered and not done, with the reason:**

- **The systemd driver (D-Bus):** a new dependency and a daemon to talk to; delegation
  gives the same placement.
- **A process group per hook** (to kill what a timed-out hook left behind): it would take
  the hook out of the terminal's foreground group, and Ctrl-C would no longer reach it.
- **libbpf or `bpftool`:** C, and the program is a dozen instructions per rule; our own
  assembler is testable everywhere.
- **Joining a cgroup that exists** (`cgroupsPath` naming another container's): refused,
  as is one below a cgroup that holds processes.

## Kernel checks

Checked before building on them, with
[`scripts/probes/cgroups.c`](../../scripts/probes/cgroups.c): Linux 7.0, OrbStack, arm64,
in a privileged `rust:1-bookworm` container (cgroup v2, a private cgroup namespace), as
root; the delegation and device checks drop to `nobody` (65534) themselves. Then under
x86-64 emulation. Each check runs in a forked child, inside a scratch cgroup the probe
removes.

The container's cgroup namespace root held processes, so it couldn't hand controllers
down (EBUSY): the probe first moved them into a leaf (`/sys/fs/cgroup/init`), as
docker-in-docker does. A host's real root cgroup is exempt from that rule.

**The tree and placement** (`cgroups(7)`, the kernel's `cgroup-v2.rst`):

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| G1 | `/sys/fs/cgroup`'s file system; `/proc/self/cgroup`; the controllers | `cgroup2`; `0::/`; `cpuset cpu io memory pids` | v2 is detected with `statfs`; a v2 line is `0::PATH` |
| G2 | "no internal processes": a cgroup holding a process, enabling a controller for its children | `memory` (a domain controller): EBUSY while the cgroup holds a process, EOPNOTSUPP after `pids` was enabled; `pids` (threaded): allowed. Moving the process into a child whose parent hands down a domain controller: EOPNOTSUPP (the child is "domain invalid"). With the process in the child first: ok | The order is fixed: make the leaf, move the process into it, then enable controllers above it |
| G3 | `clone3` with `CLONE_INTO_CGROUP` | the child starts in the target cgroup | A container's first process is born in its cgroup: nothing runs outside it, not even for an instant |
| G3 | without `clone3` (x86-64 emulation: ENOSYS), the parent writes the child's PID to `cgroup.procs` before letting it go | the child is there before it does anything | The fallback fits the existing go-byte window |

**Limits:**

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| G4 | `memory.max` 32 MiB, `memory.swap.max` 0, a process touching 128 MiB | killed by SIGKILL; `memory.events`: `oom 1`, `oom_kill 1` | An OOM kill is a SIGKILL plus `oom_kill` going up: that's how Shipyard tells "out of memory" from any other SIGKILL |
| G4 | `memory.oom.group` 1 | the process over the limit *and* an idle one in the same cgroup are killed | Available, not used: Shipyard lets the OOM killer take one process, as Docker and runc do |
| G5 | `pids.max` 5 | a process there forks 4 children (5 with itself), then EAGAIN; `pids.events` `max 1` | A fork bomb is stopped at the limit |
| G6 | `cpu.max` "10000 100000" (10% of one CPU), 0.5 s of busy loop | `nr_throttled 6`, `usage_usec` about 60,000 | Quota and period map straight to Docker's `--cpus` |
| G6 | `cpu.weight` 50; `cpuset.cpus` "0" | both accepted; `cpuset.cpus.effective` 0 | — |
| G7 | `io.max` for a real disk (`MAJ:MIN rbps=… wiops=…`); for a device that doesn't exist; `io.weight` | accepted and read back with every key (`wbps=max …`); ENODEV; **`io.weight` doesn't exist here** (it comes from the iocost controller, `CONFIG_BLK_CGROUP_IOCOST`, which this kernel lacks; BFQ has its own file, `io.bfq.weight`) | `io.max` needs real device numbers, checked when given; a weight can't be promised everywhere: refused clearly where the file is missing |

**Control:**

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| G8 | `cgroup.freeze` 1 | `cgroup.events` says `frozen 1`; a busy loop makes no progress for 200 ms; SIGKILL still kills it | Pause and resume, and a paused container can still be killed |
| G9 | `rmdir` of a cgroup with processes; `cgroup.kill` 1; `rmdir` once empty | EBUSY; both processes killed, one that ignored SIGTERM too; ok | Cleanup is `cgroup.kill`, wait until `populated 0`, then `rmdir` |

**Namespaces and delegation:**

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| G10 | a cgroup namespace (`CLONE_NEWCGROUP`), as root | `/proc/self/cgroup` says `0::/`; a `cgroup2` mount inside shows the namespace's own subtree only | The container sees its cgroup as the root, and a real `cgroup2` mount replaces stage 4's empty tmpfs |
| G10 | the same as root of a user namespace (`nobody` outside) | the mount works; writing its files fails (EACCES) where the host owner isn't mapped | Rootless containers need a delegated cgroup to change anything |
| G11 | `nobody`, given a cgroup as systemd's `Delegate=` does (the directory, `cgroup.procs`, `cgroup.subtree_control`, `cgroup.threads`) | makes a child; sets `pids.max` in it; can't change the delegated cgroup's own limit (EACCES); moves itself into its child; can't move outside (EACCES: the common-ancestor rule); the child has the delegated controllers (`memory pids`) | Rootless works inside the delegated subtree only, and can't loosen the limits the delegator set |

**Devices:**

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| G12 | a `BPF_PROG_TYPE_CGROUP_DEVICE` program (allow only char 1:3), loaded and attached by root | `/dev/null` opens; `/dev/zero` EPERM | Seven instructions are enough: the device program is small, generated from the rules |
| G12 | loading it as `nobody` | EPERM | A device program needs `CAP_BPF` and `CAP_NET_ADMIN` (or `CAP_SYS_ADMIN`) in the host's user namespace, whatever `unprivileged_bpf_disabled` says |
| G12 | as root of a user namespace, and attaching it to a cgroup delegated to that user | worked in the probe's setup (OrbStack's Linux 7.0) | Not what mainline does (it checks the host's user namespace, unless a BPF token, 6.9+, delegates it), and not relied on: in step 7, `tester`'s load was refused (EPERM), so rootless tries and goes without |

**Under x86-64 emulation** every result is the same (the full outputs diffed), except
`clone3`, which doesn't exist there (ENOSYS): the `cgroup.procs` fallback is what runs.
The device program loads and attaches under emulation, unlike seccomp.

**Also checked during the steps:** `flock` works on a cgroup directory and doesn't stop
`rmdir`; a cgroup namespace's root has `cgroup.kill`, the host's root cgroup doesn't; a
process in a cgroup namespace can't move out of it; a missing cgroup file opened with
`O_CREAT` fails with EACCES, without it ENOENT; `io.max` takes whole disks (a partition:
ENODEV); a file of a controller that isn't handed down is simply missing; cgroupfs doesn't
reuse inode numbers (64-bit inode numbers, since about Linux 5.5); a user namespace without its own PID namespace can't mount `/proc`
(only the owner of a PID namespace may).

## macOS and Linux differences

| Topic | Linux | macOS |
|---|---|---|
| Limits, devices, `--oom-score-adj` | as above | errors, like the other isolation flags |
| `resources.rs`, `cgroup/stats.rs`, `devices/` (rules, assembler, program, interpreter), `hooks.rs` | — | built and unit-tested on macOS too (the device property test included) |
| `shipyard-runtime`'s cgroup commands | as above | `LinuxOnly` |
| musl | builds | — |
| x86-64 emulation | no `clone3`: the `cgroup.procs` fallback places the child; the device program loads | — |

## Failure-mode register

| Worst case | Handling | Test |
|---|---|---|
| A container running, even briefly, outside its cgroup | born there (`CLONE_INTO_CGROUP`), or moved before the go byte | the `a_run_*_has_its_own_cgroup` tests; mutation |
| A recorded path naming `/sys/fs/cgroup` (the fallback kill would hit every process) | `Cgroup::recorded` accepts only a container cgroup below its base | unit tests |
| A cgroup recreated at a container's path by another container (`cgroupsPath`): `delete` of the first kills the second | the recorded inode; a different directory counts as gone | `a_cgroup_made_again_is_left_to_its_owner` |
| A `cgroupsPath` below another container's cgroup (its limits, freezing and kill) | refused: "it holds processes" | `a_busy_cgroup_takes_no_container_below_it` |
| A cgroup that exists already taken over | `Exists`, never joined | `a_cgroups_path_is_honored` |
| `create` killed while it sets limits: a cgroup nobody recorded | recorded right after `mkdir` | by the code's order |
| An old monitor removing the next same-ID container's cgroup | removal only under the lock, while the state directory still exists | `a_container_made_again_at_once_is_the_new_one` |
| The sweep removing a live run's cgroup (another PID namespace) | runs hold an `flock`; the sweep removes only what it locks | `the_next_run_sweeps_cgroups_of_dead_runs` |
| A run claiming a cgroup a sweep just removed | `lock_dir` checks the locked directory is still at the path; `claim_run` retries | 45 `userns.rs` runs in a row |
| Real root running without its cgroup (so without its device rules) after a failed claim | an error | by the code |
| `+pids` on a cgroup with processes breaking every later move (EOPNOTSUPP) | never written there (`holds_processes`) | `a_busy_cgroup_takes_no_container_below_it` |
| `cgroupsPath` parents under `system.slice` given every controller | only what limits need | `a_cgroups_path_is_honored` |
| A missing limit file reported as "Permission denied" | `write_file` without `O_CREAT`; `NoSuchLimit` | `a_missing_limit_file_is_named` |
| Docker's 0 ("not set") read as 0 bytes or 0 CPUs | 0 is not set, flag by flag | `zero_is_not_set_as_docker_reads_it` |
| `--cpu-shares 0` starving a container | 0 means not set | unit tests |
| An I/O limit on a partition: "No such device" | `NotADisk` | `a_partition_is_not_a_disk` |
| A device rate of 0 refused | 0 is no limit | `a_zero_device_rate_is_no_limit` |
| A limit refused partway, unsaid | `PartlyApplied` lists what was written | unit test |
| Shipyard's own SIGKILL blamed on the OOM killer | the stop timeout's kill isn't OOM | `shipyards_own_kill_is_not_an_oom_kill` |
| A `kill` after an earlier OOM kill recorded as "(out of memory)" | the `killed` marker | `a_kill_after_an_oom_kill_is_a_kill` |
| `kill --all` signalling a stranger (a reused PID) | pidfd, membership check, signal through the pin | by the code's pattern (reuse can't be made on demand) |
| `kill --all` counting processes it never reached | only those signalled | unit test |
| A freeze that timed out left half-frozen | `0` written back | by the code (a process stuck in the kernel can't be made on demand) |
| A lowered OOM score refused in a user namespace | written from outside | `the_oom_score_is_lowered_from_outside` |
| `events` failing when the container is deleted between ticks | ends cleanly | `events_report_an_oom_kill` |
| `update`, `ps`, `stats` acting after `delete` | under the lock (`with_cgroup`) | by the code |
| A rootless container that can't be deleted from another session | `Record.cgroup_base` | `a_rootless_container_is_deleted_from_another_session` |
| `sudo shipyard run` losing its cgroup | root takes the whole tree when it can write it | `root_manages_the_whole_tree` |
| `startContainer` running with the init's full power (an escape through the image's binary) | a confined child of the init | `a_run_runs_every_hook` (its `CapEff`) |
| `poststart` holding the program back (the FIFO kept open) | the FIFO closed once its byte is read | `poststart_runs_while_the_program_does` |
| `delete` during `createRuntime` leaving the hook running | `killpg` of the monitor's group | `deleting_during_a_hook_stops_it` |
| `poststop` twice | only from whoever removed the container | `deleting_during_a_hook_stops_it` |
| The monitor running a container without its hooks or cgroup | it refuses (`container_setup`) | `deleting_a_container_whose_setup_hangs` |
| The monitor trusting a bundle edited after `create` | `create`'s checks again when there's no cgroup | by the code |
| Hooks inheriting the monitor's answer pipe | close-on-exec; only stdio | `hooks_get_only_their_stdio_and_run_in_the_bundle` |
| In-container hooks told the host's PID (meaningless inside) | the init's own PID in their state | `hooks_run_at_their_phases` |
| A hook that never reads its stdin holding the runtime past its timeout | non-blocking writes against the deadline | unit test |
| A program over the verifier's complexity limit (E2BIG) | stateless passes, fields reloaded | `the_verifier_accepts_the_programs` |
| "unreachable insn" | passes stop at a rule covering everything | the property test's reachability check |
| A low `RLIMIT_MEMLOCK` failing every root run (before 5.11) | lifted once for the retry, then put back | `the_memlock_limit_is_lifted_and_put_back` |
| A kernel without cgroup BPF: a bare "Invalid argument" | the message names the kernel options | `a_refusal_says_why` |
| A rootless device refusal failing the run | goes without, warning if asked | `rootless_devices` |
| `--device /dev` pulling in the host's terminals and `/dev/shm` | stays on one file system | by the code |

## Known limitations (by design, for stage 5)

- **cgroup v2 only;** on v1 a container runs without a cgroup: no limits, pause, stats or
  device control. That holds for root too: without a cgroup, a rootful container's
  devices are limited only by its capabilities (Docker's include `CAP_MKNOD`).
- **Rootless needs delegation** (systemd's `user@UID.service`), and gets the controllers it
  delegates (`cpu memory pids` by default). A rootless container ends with the user
  manager, at the last logout without lingering. *(Corrected in stage 9, step 9.5c, on
  Ubuntu 24.04: a container made in a login session keeps that session "closing", and
  the user manager with it, so it runs on with logind's `KillUserProcesses=no`; with
  `yes` it ends at logout. See docs/user/containers.md, "Logging out".)*
- **Rootless device control** depends on the kernel letting the user load the program;
  where it doesn't, the container can still use only what the user can.
- **`io.weight`** comes from the iocost controller; refused where the file is missing.
  BFQ's own weight (`io.bfq.weight`, 1–1000), which runc falls back to, isn't used yet.
- **A writable cgroup mount** (a bundle without `ro`) lets root inside change its own
  limits, as with runc.
- **`unified`** can write any controller file, as with runc.
- **Hooks:** a timed-out hook's own children keep running; while the runtime's hooks run,
  `shipyard run` acts on signals only after them (Ctrl-C reaches the hook directly);
  hooks the init runs count against the container's `pids.max`; `startContainer` has
  less power than with runc (only `CAP_KILL`).
- **Device rules don't change with `update`** (runc's don't either).
- **Recorded residuals:**
  - `freeze`'s wait for the kernel survived mutation: every test process freezes at once;
  - `ps`'s table reads `/proc/PID` after the lock is let go, so a PID reused in between
    shows another process's line (nothing is done to it);
  - `holds_processes` checks, then writes, so a process entering in between would still
    trip the kernel's rule;
  - controllers turned on at the root stay on, and parents a `cgroupsPath` made stay
    behind, as with runc;
  - real root refused a device program isn't tested (it needs such a kernel);
  - `remove_own` while a process lingers in a failed create's cgroup can leave it for
    `delete`.

## Carried forward

| Stage | Item |
|---|---|
| 8 (`exec`) | Placing joined processes in the container's cgroup. |
| 9 (named containers) | `shipyard stats`, `top`, `pause`, `update`. |
| 14 (hardening) | Removing the device program from inside needs `CAP_SYS_ADMIN` (to get its descriptor by ID; before 5.8 `CAP_NET_ADMIN` too), not a default; BFQ's `io.bfq.weight` as `--blkio-weight`'s fallback, as runc does; fuzzing the OCI resources reader; the OCI conformance suite (moved to stage 8, H5). |

**Resolved from stage 4:** device access control (eBPF on the cgroup), and the cgroup
namespace.

**Resolved from stage 3:** `process.oomScoreAdj` and `hooks`, warned about until now, are
applied; `linux.resources` is honoured; a bundle without a `pid` namespace is accepted
when the container has a cgroup.

## Tests

| Kind | Where | What |
|---|---|---|
| Unit | `crates/shipyard-runtime/src/**`, `crates/shipyard-cli/src/**` | `Resources` (validation, sizes, CPUs, shares, weights, swap, the files and their order, `unified` keys); the cgroup parsers, `choose_base`, `handing_down`, the recorded-path guard, the sweep's choice; `Stats` and `io.stat`; OCI resources, `cgroupsPath`, hooks and devices (every refusal); hooks (argv, env, stdin, timeouts, a 1 MiB state, records); the device rules, the assembler, the interpreter's strictness, **the property test** (random rules and requests against `Devices::allows`, nothing out of reach), the longest program; the `bpf_attr` layouts; every new failure message; each CLI flag and Docker's rules |
| Cgroups | `crates/shipyard-cli/tests/cgroups.rs` | 12 root tests and 2 as any user: a run's cgroup with and without a root filesystem and in a user namespace, gone after, the sweep, limits, swap, OOM, pids, rootless delegation, an undelegated controller, no delegation, a partition, shipyard's own kill |
| Runtime | `crates/shipyard-cli/tests/runtime.rs` | adds: a container's cgroup for its life, no `pid` namespace, a bundle's cgroup mount and limits, a missing limit file, OOM in the exit record, kill after OOM, `cgroupsPath` (honoured, made again, below a busy cgroup), cross-session delete, pause/resume (created too), `delete --force` when paused, `kill --all`, `update`, `ps`, `events`, the OOM score, every hook phase and failure, `poststart` with the program running, delete during a hook, hooks' descriptors |
| Bundles | `crates/shipyard-cli/tests/bundle.rs` | `run --bundle`'s hooks (`a_run_runs_every_hook`, `a_failing_hook_fails_the_run`, `hooks_that_cant_run_are_refused`) |
| Others | `userns.rs`, `rootless.rs`, `run.rs` | the OOM score: lowered from outside (`the_oom_score_is_lowered_from_outside`), only raised rootless (`a_rootless_command_can_only_raise_its_oom_score`), without isolation (`the_oom_score_adjustment_reaches_the_command`) |
| Devices | `crates/shipyard-cli/tests/devices.rs` | 6 root tests and 2 as any user: the defaults, `--device` (and `:r`, a directory), `--device-cgroup-rule`, a bundle's rules and nodes through `run --bundle` and the runtime, an allow rule enforced, rootless |
| Terminal | `crates/shipyard-cli/tests/terminal.rs` | 4 new `ISOLATED_TESTS` (root and rootless): Ctrl-C during a hook, Ctrl-Z and `fg` with limits, an OOM kill, pause |
| Kernel | `scripts/probes/cgroups.c` | the checks above |

**Test mechanics worth knowing:**

- **The test environment mimics a systemd host** (`scripts/test-linux.sh`): the container's
  processes move into `/sys/fs/cgroup/init`, every controller is enabled at the
  namespace's root, and `tester` gets `user.slice/tester` (chowned as `Delegate=` does,
  under a root-owned `user.slice` handing down only `cpu memory pids`), its shells in
  `tester/session`. It acts only at the root of its own cgroup namespace, so
  `--cgroupns=host` is left alone.
- **`SHIPYARD_TEST_DELEGATED=required`** (CI and `test-linux.sh`) turns a missing
  delegation into a failure instead of the no-delegation branch.
- **Loop devices** (`b 7:N`) for device tests: `/dev/kmsg`'s open refuses without
  `CAP_SYSLOG` with the same EPERM, so it couldn't tell the program's refusal from the
  driver's.
- **OOM hogs** are a command substitution of 200 MiB of `/dev/urandom` (the shell holding
  it is the largest process); dash gives up at its first refused fork, so the pids test
  reads the shell's stderr.
- **`Bundle::checked_start_container`** logs `startContainer`'s `CapEff`, so every hook
  test checks its confinement.

**Mutation-checked.** Every important behaviour was removed on a scratch copy
(`rsync -rlp`), each with its own target directory, and its tests confirmed to fail:

| Step | Mutations, each caught |
|---|---|
| 2 | no placement; no `unshare`; the mount read-write; no removal; the sweep taking live runs; the monitor's removal without the directory check; `recorded` accepting any path; the sweep ignoring the lock; the mount kept without a namespace; the rootless mount kept |
| 3 | limits never written; Docker's swap rule off; OOM never reported; `enable` skipping the root; the `.`/`..` check off; limits without a cgroup; `holds_processes` always false; `O_CREAT` writes; OOM blamed for shipyard's kill; the partition hint gone; foreign parents handed everything; `--memory 0` read as 0 |
| 4 | `enable` above the base; the writability check skipped; rootless finding no tree; the guard accepting the base's parents; the `/proc` rule removed; the root rule off; `delete` requiring this session's tree |
| 5 | `status_of` never paused; `kill --all` reaching only the init; `update` applying Docker's swap; the OOM score never written; `events` missing the OOM; the score not written from outside; `events` failing on a deleted container. **Survived:** `freeze` not waiting (above) |
| 6 | hooks after the pivot; `createContainer` outside the namespaces; no timeout; no `poststop` from `delete`; a failing `createRuntime` ignored; the umask not given back; `startContainer` told `creating`; the environment inherited; no `poststart` for a run; `start` holding the lock; `startContainer` unconfined; no `killpg`; `poststop` twice; the answer pipe inherited and no working directory |
| 7 | a deny that allows; first rule winning; the access ignored; a pass past a rule covering everything; no program attached; a bundle without the default list; `--device` without its rule; the memlock limit left lifted; the "no device programs" branch removed |
| 8 | the FIFO kept through `poststart`; no inode check; no `killed` check; pause of a created container refused |

**Verified at the end of the stage:** fmt; clippy on macOS, aarch64, x86-64 and musl with
warnings as errors; rustdoc as CI runs it; both generated references; the macOS suite;
`scripts/test-linux.sh` (everything as a normal user, with the rootless, delegated and
terminal tests, then everything as root); the whole workspace under x86-64 emulation.

**CI:** the Linux job starts the runner user's systemd manager (`loginctl enable-linger`)
and runs the normal-user tests in a scope inside it (`systemd-run --user --scope`), with
`SHIPYARD_TEST_DELEGATED=required`; the `root` job runs cgroups, device programs and loop
devices on GitHub's kernel. Both are checked only once pushed.

## Review history

Each step had a clean-code review and a worst-case ("Murphy's law") review; step 8 added
a full review by three read-only reviewers (the cgroup layer; the lifecycle with hooks;
devices and the CLI), every finding checked in the code or reproduced before it was fixed.

- **Step 1 (probe, `resources`, cgroup parsers, the test environment):**
  - review: one `scale_decimal` (the first `parse_size` made `1.5g` two bytes short);
    "at least" for open-ended limits; only -1 is "no limit" for swap; `TooFewCpus`;
  - worst case: **`--cpu-shares 0` would have starved a container** (0 is "not set");
    **the test script could have rearranged a real cgroup tree** under
    `--cgroupns=host`, and its move loop could spin; **the probe wrote a host's root
    `subtree_control`** every run; `delegation_root` refused a delegated namespace root;
    a reservation above the limit (`ReservationAboveLimit`).
- **Step 2 (a cgroup per container, rootful):**
  - found on the way: the kernel won't mount cgroup2 on its own mount (a tmpfs cover);
    **a monitor could have killed the next container with the same ID** (removal under
    the lock, while the directory exists);
  - review: a doc comment on the wrong function; the mount's flags worked out three times
    (`CgroupMount::mount_flags`); `mount_cgroup_over_the_hosts`; `make_run_cgroup`;
  - worst case: **a recorded path was trusted blindly** (`Cgroup::recorded`); **the sweep
    judged life by `/proc`** (the run lock); the kill looped only once before 5.14; a
    bundle's cgroup mount without a namespace showed the host's tree; a rootless bundle's
    mount dropped silently.
- **Step 3 (limits and OOM):**
  - found on the way: **a limit could break every later run** (`+pids` on a busy root:
    `holds_processes`); **a run could claim a just-swept cgroup** (`lock_dir`'s check);
  - review: `resources::positive` for "zero or negative is no limit"; `Cgroup::limit`;
    one wording of "needs root and cgroup v2" (later replaced by step 4's `NoTree`);
    `block_device` never falls back to `0:0`;
  - worst case: **every "missing file" check was dead code** (`O_CREAT`: `write_file`);
    Docker's zeros; **shipyard's own kill blamed on OOM**; `cgroupsPath` parents under
    systemd's slices; a `cgroupsPath` inside `run/`; OCI's "default" values; partitions.
- **Step 4 (rootless cgroups):**
  - found on the way: **a user namespace without a PID namespace can't mount `/proc`**
    (`UserRootNeedsPid`); the sweep test raced other runs;
  - review: `NoOwnCgroup`; the no-delegation message; a redundant check in
    `is_container_cgroup`;
  - worst case: **`sudo shipyard run` lost its cgroup** (the root rule); **a rootless
    container couldn't be deleted from another session** (`Record.cgroup_base`);
    trailing slashes in messages.
- **Step 5 (the runtime's cgroup commands, `oomScoreAdj`):**
  - review: **`update`, `ps` and `stats` acted after letting go of the lock**
    (`with_cgroup`); `kill_all` instead of a flag argument; `LIVE`;
  - worst case: **`kill --all` could signal a stranger** (pidfds); **a lowered OOM score
    failed in a user namespace** (written from outside); `events` on a deleted container;
    `update --memory-swap` alone.
- **Step 6 (OCI hooks):**
  - review: `HookFailure::Wait`; the record's detail as bytes; one `discard_pending`;
    `rootfs::enter` generic over its `before_pivot` step; `hooks_pipe`; one `state_json`;
  - worst case: **`startContainer` ran with the init's full power** (a confined child);
    **`delete` during setup left its hooks running** (`killpg`); `poststop` twice; **the
    monitor ran without hooks or cgroup** on an unreadable record; `start` failing after
    starting; hooks inheriting the answer pipe; hooks' working directory; non-UTF-8
    bundle paths;
  - two test races fixed (`events_report_an_oom_kill`,
    `a_start_that_dies_at_the_gate_starts_nothing`), and a test rewritten
    (`deleting_a_container_whose_setup_hangs`).
- **Step 7 (device control):**
  - found on the way: **the verifier's E2BIG** (stateless passes, fields reloaded) and
    **"unreachable insn"** (passes stop at a rule covering everything);
  - review: **two ways to warn** (the CLI's `Lines` formatter); `skip_unless_covered`,
    `covers_every_device`; `DeviceRule::new`; `CgroupError::InvalidDevices`;
    `devices_without_cgroup`; `bind_host_device_node` and `mknod_device`;
  - worst case: **`RLIMIT_MEMLOCK` before 5.11** (lift and restore); **a kernel without
    cgroup BPF** (the message); the docs promising more than there is.
- **Step 8 (terminals, CI, docs, the full review):** four terminal tests; CI read through
  and pushed; every user page checked against the code. The review:
  - **`start` held the exec FIFO through `poststart`,** so the program waited for every
    hook (the most serious finding);
  - **cgroup identity:** a cgroup made again at a container's path; a `cgroupsPath` below
    a busy cgroup; the early record; real root's failed claim;
  - **a kill blamed on OOM** after an earlier OOM kill (the `killed` marker);
  - **the monitor's re-read bundle** (`without_cgroup`); pause of a created container;
    the lock wait (15 s, above `delete`'s two 5 s waits); `ContainerError::Cgroup`;
    `HookRunnerCapabilities`; `container::memory_limit`; `--pid-file`'s clean-up
    reported;
  - **the cgroup layer:** a freeze that timed out; `kill --all`'s count; `PartlyApplied`;
  - **devices and the CLI:** runc's exact default list and no node rules; rootless going
    without on any refusal; an existing node's mode and owner; `--device DIR` on one file
    system; `--no-isolation`'s hint; a device rate of 0; `--memory-swap -1`.
- **Step 9:** these notes rewritten, the book's chapters 20–23, and chapters 1–19
  rechecked against the code; the user pages and a few doc comments corrected on the way.
  Its worst-case review checked the kernel facts against the kernel's source and docs,
  every quoted message against the code, and every "Try it" for what it would do on a
  reader's own machine:
  - **in-container hooks got the host's PID** (the spec says the PID seen inside): fixed,
    tested, mutation-checked;
  - **the "no device control" warning told root "only what your user can"**: it says
    "nothing but its capabilities" now; and "real root fails closed" holds only where it
    has a cgroup, now said everywhere;
  - **kernel facts corrected:** `io.weight` is iocost's (BFQ's is `io.bfq.weight`); device
    programs need `CAP_BPF` and `CAP_NET_ADMIN`; `O_PATH` works for
    `CLONE_INTO_CGROUP` (checked); the `oom_score_adj` floor; `BPF_F_ALLOW_MULTI`'s
    meaning; terminals above 255 stay major 136; hooks' working directory and environment
    are Shipyard's choices, not runc's;
  - **the book's examples bound the host's `/usr` read-write and never unmounted it,** so
    a later chapter's `chown -R` (or a clean-up `rm -rf /lab`) would have reached the
    host's `/usr`: every bind is read-only now, every chapter cleans up, and the README
    says the examples are for the throwaway test container only;
  - **a book agent wrote two bytes to `/dev/loop0`,** the OrbStack VM's own disk, while
    trying an example: they fell in btrfs's unused first 64 KiB and were put back
    (checked with `od` and `blkid`); examples only open loop devices now.

**Lessons worth keeping:**

- **Order is the rule:** make the leaf, put the process in it, then hand controllers down
  above it; write the limits while the cgroup is empty; record the cgroup before the slow
  part.
- **A path isn't an identity.** A cgroup, like a PID, can be replaced at the same name:
  pin it (a lock, an inode, a pidfd) and check it's still the one.
- **A dead check is worse than none:** `O_CREAT` turned every "missing file" branch into
  EACCES, and only a test that reached the branch showed it.
- **Attribute carefully:** a SIGKILL is the OOM killer's only if nobody else sent one.
- **Shape code for the verifier you have,** and teach your own interpreter its rules, so
  the property test catches what the kernel would refuse.
- **A reader that stays open is a lock:** the exec FIFO held the program back as long as
  `start` held it.

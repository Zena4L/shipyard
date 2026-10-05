# Stage 2 notes: namespaces

The engineering record of stage 2: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [Running commands](../user/run.md#isolation) and
[Exit codes](../user/exit-codes.md).

## Outcome

Stage 2 asked for a command in its own namespaces, with its own session, a minimal PID-1
init and a way to join a running container ([ROADMAP](../ROADMAP.md),
[FEATURES §2](../FEATURES.md#2-namespaces--stage-2)). On Linux, `shipyard run` now
isolates by default; `--no-isolation` keeps stage 1's behaviour, and macOS keeps it until
the VM (stage 12).

| Requirement | Where | Tested by |
|---|---|---|
| New PID, mount, UTS and IPC namespaces | `isolation.rs`, `process/clone.rs`, `spawn.rs` (`launch`) | `every_namespace_is_new`, `without_isolation_the_namespaces_are_shared`, `clone3_without_namespace_flags_works_like_fork`, `the_clone_fallback_works_like_fork` |
| Mounts that never reach the host, a fresh `/proc`, the hostname | `child.rs` (`set_up_container`) | `mounts_made_inside_do_not_reach_the_host`, `a_host_unmount_reaches_the_container`, `the_command_runs_under_an_init_and_sees_only_its_own_processes`, `the_hostname_is_the_containers_own` |
| A session of its own; the terminal in isolated mode | `child.rs` (`start_session`), `supervisor.rs` | the 11 `ROOT_TESTS` in `tests/terminal.rs` |
| A minimal init: PID 1, forwarding, reaping, the exact status | `process/init.rs`, `channel.rs` (status pipe), `spawn.rs` (`init_outcome`) | `sigterm_stops_a_command_without_a_handler`, `sigint_ends_the_command_and_shipyard`, `death_by_a_signal_comes_through_the_init`, `the_init_reaps_orphans`, `the_containers_other_processes_end_with_the_command` |
| The init carries none of shipyard's state | `init.rs` (`reexec`, `entry_point`) | `the_init_carries_none_of_shipyards_secrets`, `the_init_does_not_hold_stdio`, `the_init_does_not_hold_leaked_descriptors`, `an_init_that_cannot_start_is_reported` |
| Joining a running container (`setns`) | `process/join.rs`, `Supervisor::supervise_joined` | the 15 tests of `crates/shipyard-runtime/tests/join.rs` |
| The setup handshake | `channel.rs` (go pipe), `child.rs` (`wait_for_go`, `die_with_parent`) | `the_container_dies_with_shipyard`, `the_init_survives_a_hostile_parent` |
| Clear refusals: no root, refused, invalid | `error.rs`, `failure.rs`, `isolation.rs` (`validate`) | `without_root_isolation_is_refused_with_advice`, `only_isolation_failures_suggest_no_isolation`, `joins_are_checked_like_isolations`, unit tests |

**Moved out of stage 2**, each to the stage that gives it meaning (the user's decision,
recorded in [FEATURES](../FEATURES.md)):

| Item | Now in | Why |
|---|---|---|
| Network namespace | stage 7 | means nothing without veth, bridge and addresses |
| User namespace | stage 4 | is rootless support |
| Cgroup namespace | stage 5 | belongs with cgroups |
| Time namespace | not planned | no use case yet |
| Domain name, namespaced sysctls | stage 3 | come from OCI `config.json` (`domainname`, `linux.sysctl`) |
| `unshare` | dropped | `clone3` creates the namespaces; `unshare` would add nothing |

Paths below are relative to `crates/shipyard-runtime/src/` and `crates/shipyard-cli/`.

## Architecture

```
shipyard run (CLI: run.rs) ─ main() calls process::init_entry_point() first
  └─ Supervisor::supervise(spec, stop, isolation) / supervise_joined(spec, stop, join)
       ├─ check_isolation / check_join            supervisor.rs: validate, Linux, root, entry point
       ├─ spawn()                                 spawn.rs
       │    ├─ report pipe, go pipe; with an init: status pipe + /proc/self/exe
       │    ├─ joining: open /proc/<pid> and its ns/*                     join.rs
       │    ├─ ChildPlan (ContainerSetup + InitSetup, or JoinSetup)       plan.rs
       │    └─ launch, all signals blocked:
       │         Host → fork │ New → clone3 (→ clone) │ Join → setns(pid) → fork → setns(back)
       ├─ child: set_up_and_exec()                child.rs (after-fork rules)
       │    handshake → container: setsid → rslave / → /proc → hostname
       │              → with a PID namespace: init::run(); without: prepare_and_exec()
       │    or join: setns(UTS, IPC, mnt) → setsid → prepare_and_exec()
       ├─ the init (PID 1)                        init.rs
       │    clone the command (PID 2) → close inherited fds → chdir / → pass_on_pending
       │    → stdio to /dev/null → execveat(/proc/self/exe) (→ execve /proc/self/fd/N)
       │    → entry_point(): name, SIGPIPE default → wait_and_forward():
       │         non-dumpable → start mark → sigwaitinfo loop: forward, reap
       │         → status record → _exit(the command's code)
       │    (if both execs fail: wait_and_forward() as the copy, start mark C)
       └─ watch(): the event loop; reap → init_outcome(status pipe, the init's status)
```

## Decisions and why

- **Isolate by default on Linux** (the user's decision): new PID, mount, UTS and IPC
  namespaces. That needs root until rootless support (stage 4). `--no-isolation` keeps
  stage 1's behaviour; macOS keeps it until the VM.
- **Our own init is always PID 1, with the command as PID 2** (the user's decision). The
  kernel drops any signal a namespace's PID 1 has no handler for, so a plain `sleep` as
  PID 1 would ignore Ctrl-C and `docker stop` (`pid_namespaces(7)`). The init blocks
  every signal and waits with `sigwaitinfo`, which the kernel still delivers to PID 1.
  It forwards every signal and reaps every child, orphans included (TLPI ch. 26). When
  the command exits, the init exits, and the kernel kills the rest of the namespace.
- **Raw `clone3`, falling back to `clone`** on ENOSYS (old kernels, Docker's seccomp,
  x86-64 emulation), E2BIG and EPERM (older seccomp profiles). glibc has no usable
  wrapper for a fork-like clone (TLPI ch. 28). The fallback passes full-width null
  pointers through C's `...`, and converts the flags bit for bit.
- **A go-pipe handshake** replaces stage 1's `getppid()` check, which returns 0 inside a
  PID namespace. The child waits for "go". The parent keeps its end open until `exec`,
  so a hang-up means the parent died.
- **`/` is remounted recursively slave first, then a fresh `/proc`.** Nothing propagates
  out, even where `/` is shared (systemd), and host unmounts still propagate in, so a
  disk unmounted on the host isn't kept busy by the container. This is runc's choice
  (`mount_namespaces(7)`). It was rprivate until the stage review showed the pinning.
- **A status pipe carries the command's exact end.** PID 1 can't die "by" a signal, so
  only the init's report can say "killed by SIGINT".
  - **A strict protocol:** a start mark (`R` for clean, `C` for copy), then one record.
  - **A cross-check:** the record is believed only if it matches the init's own exit
    code, which mirrors the command's.
  - **Non-blocking writes:** a container can reach the pipe through `/proc/1/fd`, and
    must never be able to freeze the init.
- **The init re-execs itself clean.** As a copy of shipyard it held shipyard's command
  line (`-e` values, world-readable in `/proc/1/cmdline`), environment, memory and stdio.
  - **How:** it execs the binary opened as `/proc/self/exe` before the clone, with
    `execveat`, falling back to `execve` of `/proc/self/fd/N`.
  - **The new image:** a minimal argv, an empty environment, stdio on `/dev/null`, and
    `PR_SET_DUMPABLE 0`.
  - **Every binary** must call `init_entry_point()` first; isolation with a PID namespace
    is refused otherwise.
- **No init without a PID namespace:** without one, PID 1's rules don't apply and nothing
  needs it.
- **After a raw clone, no glibc function that keeps process-wide state.** glibc's
  `setuid` family takes the thread-list lock and signals every thread, which deadlocked a
  child cloned from a multi-threaded process. The child uses the raw
  `setgroups`/`setresgid`/`setresuid` system calls; credentials are per thread in the
  kernel. They rely on stage 1's 64-bit-only build (`lib.rs`): on 32-bit, those syscall
  numbers are the old 16-bit calls.
- **Joining switches `pid_for_children` around a fork in the parent.** `setns` of a PID
  namespace never moves the caller, only its future children. The joined process must be
  shipyard's own child: an orphan goes to the container's init, never to a subreaper
  outside. The child joins UTS, IPC and then mount itself, since a multi-threaded
  process can't join a mount namespace. A failed switch back aborts the process.
- **`/proc/PID` directory descriptors, not pidfds,** name the target. `openat` through one
  gives ESRCH once the PID is reused, which works on every kernel since 3.8. And
  `pidfd_send_signal(0)` isn't a liveness check: it succeeds on a zombie.
- **Leftovers end with the command,** joined or not, as with `run`. Docker's `exec` keeps
  them, so that's for the `exec` CLI to decide (stage 9).
- **A session of its own** for isolated and joined commands. Keys, window-size changes and
  hangup reach them only through shipyard's forwarding. A continued shipyard continues
  them too.
- **Runtime messages never name a CLI flag.** The CLI adds `hint: --no-isolation` for
  failures that isolation causes (`SpawnErrorKind::Isolation` and the refusals).

**Considered and not done, with the reason:**
- **Catching or blocking signals 32 and 33 in shipyard:** glibc uses them internally
  (cancellation, setxid), so touching them would break a multi-threaded embedder.
  Documented instead.
- **Refusing a mount-only join:** allowed, and documented on `Join::validate`. A process
  that joins the mount namespace without the PID one sees the container's `/proc`, whose
  PIDs aren't its own. But `cp` into a container (stage 8) wants exactly that.
- **Detecting "started through the dynamic loader" up front** (`getauxval(AT_BASE)`):
  static binaries look the same. The clear `InitDidNotStart` error covers it.
- **pidfds for joining:** `/proc/PID` directory descriptors give the same identity
  guarantee on every kernel since 3.8, and `pidfd_send_signal(0)` can't tell a zombie
  from a live process.

## Kernel checks

Checked before building on them: Linux 7.0, OrbStack, arm64, in `rust:1-bookworm`
(privileged for the namespace checks, unprivileged for the seccomp one). Run the probes
as their headers say.

**[`scripts/probes/namespaces.c`](../../scripts/probes/namespaces.c):**

| Behaviour | Result | Consequence |
|---|---|---|
| A namespace's PID 1 gets signals it **blocks**, without a handler | Yes: SIGINT, SIGUSR1 and SIGTERM from outside all arrived through `sigwaitinfo`, with `si_pid` 0 | The init blocks everything and waits |
| PID 1 gets an **unblocked** signal with no handler | No: SIGUSR2 was dropped | Never rely on default actions in the init |
| `clone3` under Docker's default seccomp | ENOSYS | Fall back to `clone` |
| `clone(CLONE_NEWPID)` without privileges | EPERM | "needs root" |
| A fresh `proc` in a new mount + PID namespace | shows only the namespace's processes; the host's mounts unchanged | `/proc` after the propagation change |
| Propagation of `/` | private inside Docker; shared on systemd hosts | Always change `/`'s propagation first (now rslave) |

**[`scripts/probes/join.c`](../../scripts/probes/join.c):**

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| 1 | Another thread forks while one is switched | its children stay on the host | Safe in a multi-threaded program |
| 1b, 11 | A switched thread creates a thread, or a PID namespace | EINVAL | Nothing but the fork while switched |
| 2 | The forked child | inside (PID 2), `getppid()` 0, our child: we reap its status | The joined command is supervised directly |
| 3 | Switching back to our own PID namespace | allowed | Restored right after the fork |
| 4 | An orphan of the joined process | goes to the container's init, not to our subreaper | The command must be our child |
| 5 | Joining mount + UTS + IPC | moves root and cwd to the container's root | `cwd` and `PATH` resolve inside |
| 6 | Dead init: `setns(pidfd)`; `pidfd_send_signal(0)`; fork into its namespace | ESRCH; succeeds on a zombie; ENOMEM | No pidfd liveness check; `ContainerExiting` |
| 7 | `setns(NEWNS)` with two threads | EINVAL | The mount join happens in the child |
| 8 | A killed init while our joined process is an unreaped zombie | the init stays in S until we reap; then Z | A joiner that can't reap holds up the container's end |
| 9 | `ns/mnt` of an exiting or zombie init | ENOENT (its `ns/pid` still opens) | Tells "exiting" from a real ENOMEM |
| 10 | Fork into a zombie init's namespace | ENOMEM | `ContainerExiting` |
| 12 | A `/proc/PID` descriptor after the PID is reused | ESRCH | All `ns/*` are opened through one such descriptor |

**Also checked during the reviews:**
- ignoring a signal discards one that's already pending, even while blocked (hence
  `pass_on_pending`);
- execve through a `noexec` `/proc/self/fd` works (noexec applies to the target's mount);
- a tmpfs the host mounts after the start appears inside (rslave);
- TSTP to an orphaned process group is discarded (vim's Ctrl-Z inside).

## macOS and Linux differences

| Topic | Linux | macOS |
|---|---|---|
| Isolation | namespaces, by default (root) | not yet: runs as with `--no-isolation`; the runtime returns `LinuxOnly`; `--hostname` is an error |
| `--stop-signal 32` / `33` | refused (glibc's internal signals) | no such signals |
| A terminal's signals in a shared group | kernel source: never forwarded twice | look like `kill`: the terminal-signal list prevents a second forward (only macOS catches a WINCH mutation) |
| 32-bit | not built (stage 1's 64-bit-only rule, which the raw credential calls now rely on) | not built |
| x86-64 emulation (OrbStack's Rosetta) | no `clone3`, no `execveat`: both fallbacks run there. Rosetta deadlocks on a raw `clone` while threads are being created, so the setxid stress test skips itself there | — |

## Failure-mode register

| Worst case | Handling | Test |
|---|---|---|
| `clone3` blocked (seccomp, old kernel, emulator) | `clone` fallback | `the_clone_fallback_works_like_fork`; every isolation test under x86-64 emulation |
| Not root, or root but refused | `IsolationNeedsRoot` / `IsolationRefused` with hints | `without_root_isolation_is_refused_with_advice`; the refusal was checked by hand in unprivileged Docker |
| Namespace limits reached | ENOSPC names `/proc/sys/user/max_*_namespaces` | — (changes global sysctls) |
| `/` shared (systemd): mounts leak out | rslave first | `mounts_made_inside_do_not_reach_the_host` (root pass makes `/` shared) |
| Host mounts pinned by the container | rslave: host unmounts propagate in; the init `chdir`s to `/` | `a_host_unmount_reaches_the_container`, `the_init_does_not_hold_shipyards_working_directory` |
| `/` not a mount point (`chroot`) | EINVAL, with the message and docs | checked by hand |
| PID 1 ignores unhandled signals | our init forwards everything | `sigterm_stops_a_command_without_a_handler`, `signals_sent_to_the_init_reach_the_command` |
| Death by a signal hidden by PID 1 | status record → `die_from` | `sigint_ends_the_command_and_shipyard`, `death_by_a_signal_comes_through_the_init` |
| Zombies and leftovers inside | the init reaps; its exit ends the namespace | `the_init_reaps_orphans`, `the_containers_other_processes_end_with_the_command` |
| The stop timeout kills the init | no record → the init's own status (137) | `a_command_ignoring_the_stop_signal_is_killed_after_the_timeout` |
| Leaked descriptors pinned by PID 1 | the init closes all but two (`close_range`, or the plan's list) | `the_init_does_not_hold_leaked_descriptors`, `ranges_leave_out_only_the_kept_descriptors` |
| Hostile parent (SIGCHLD ignored or blocked) | reset in the child; the mask is all-blocked anyway | `the_init_survives_a_hostile_parent` |
| Shipyard dies | pdeathsig on the init, plus the go-pipe hang-up | `the_container_dies_with_shipyard` |
| The init exposes shipyard's argv, environment, stdio | clean re-exec | `the_init_carries_none_of_shipyards_secrets`, `the_init_does_not_hold_stdio` |
| `execveat` unavailable | `execve` of `/proc/self/fd/N` | the x86-64 emulation runs |
| The re-exec fails | carries on as the copy, with a warning | — (can't fail on demand) |
| The re-exec'd image can't start (`ld.so`, missing environment) | no start mark → `InitDidNotStart` (125, hint) | `an_init_that_cannot_start_is_reported` |
| A program embedding the runtime forgets `init_entry_point` | `NoInitEntryPoint` | `isolation_needs_the_init_entry_point` |
| Isolation without a PID namespace | no init: the command is the direct child | `isolation_without_a_pid_namespace_starts_no_init` |
| setxid deadlock after a raw clone | raw credential syscalls | `isolation_without_a_pid_namespace_starts_no_init` (50 spawns while another thread starts and ends threads, with a watchdog) |
| A forged status record | strict protocol + exit-code cross-check | `a_forged_status_record_is_not_believed`, `a_forged_record_cannot_hide_a_kill` |
| A full status pipe freezing the init | non-blocking writes | `a_full_status_pipe_does_not_freeze_the_init` |
| Signals pending at the re-exec (SIGPIPE discarded) | passed on first, at most 64 | — (can't be arranged on demand) |
| Stop signals the init can't forward (CHLD, 32, 33) | refused | `stop.rs` unit test |
| A signal ignored at startup | stays ignored inside | `signals_ignored_at_startup_stay_ignored_inside` |
| Join: PID reused | one bound `/proc/PID` descriptor | probe check 12 |
| Join: the target is gone, or exiting | `NoSuchProcess`; `ContainerExiting` (only if `ns/mnt` is gone, so a real ENOMEM isn't misreported) | `joining_a_container_that_has_exited_fails`, `joining_a_container_that_is_exiting_fails` |
| Join: `/proc` of another PID namespace | `ForeignProc`. (Where the target's PID namespace isn't nested in ours, the join would fail anyway; the guard gives the reason, and covers the cases where nesting would let the wrong process through.) | `with_another_namespaces_proc_joining_is_refused` |
| Join: other threads forking during the switch | per-thread `setns` | `other_threads_keep_starting_processes_on_the_host` |
| Join: the switch back fails | abort (impossible in practice) | — |
| A joiner that can't reap holds the container's end | the kernel's rule; documented, a requirement for `exec` | `a_stopped_joiner_holds_up_the_containers_end` |
| Ctrl-C, Ctrl-\, WINCH forwarded twice or not at all | sources and the session rules | `isolated_ctrl_c_is_forwarded_exactly_once`, `isolated_ctrl_backslash_quits_the_command`, `window_size_changes_reach_the_command_once` |
| An isolated command stops itself (terminal "hangs") | Ctrl-Z, then `fg`: a continued shipyard continues the command | `an_isolated_command_that_stops_itself_resumes_with_ctrl_z_and_fg` |
| Hangup | forwarded (129); a command ignoring it keeps running | `isolated_hangup_ends_the_command_and_shipyard`, `an_isolated_command_ignoring_hangup_keeps_running` |
| A setup failure that isolation didn't cause | no `--no-isolation` hint | `only_isolation_failures_suggest_no_isolation` |

## Known limitations (by design, for stage 2)

- **Not a security boundary.** The container is root on the host's files. Its `/proc` is
  mounted over the host's, and root inside could unmount it. It can reach the init's
  status pipe and memory (`PR_SET_DUMPABLE` only binds once capabilities are dropped).
- **No root filesystem or network of its own** yet. Host mounts made after the start
  appear inside (rslave).
- **No controlling terminal.** Programs that open `/dev/tty` (password prompts) fail. A
  backgrounded isolated command still reads the terminal. Ctrl-Z stops only shipyard.
- **A joined process stays in shipyard's cgroup,** outside the container's limits (no
  cgroups yet).
- **Signals 32 and 33 sent to shipyard** end it: glibc keeps them, so they can't be
  caught.
- **A setuid exec clears the parent-death signal,** so with `--no-isolation` or when
  joined, such a program can outlive a SIGKILLed shipyard. An isolated run is safe: the
  init holds its own.
- **An exiting init waits for outside parents to reap:** a stopped or hung joiner holds
  up its container's end. It resolves once the joiner reaps, or dies: its zombie is then
  re-parented to the exiting init, which releases it.

## Carried forward

| Stage | Item |
|---|---|
| 3 (filesystem, OCI) | `pivot_root`, leaving the old root unreachable: hides the host's `/proc` under ours, and makes rslave's "host mounts appear inside" moot. Domain name and namespaced sysctls from `config.json`. `cwd` and user/group lookups resolved inside the root filesystem. |
| 4 (security) | User namespaces (rootless, and the "needs root" errors). Dropping capabilities: makes `PR_SET_DUMPABLE` effective, and the status-pipe forgery moot. |
| 5 (cgroups) | Cgroup namespaces. Joined processes join the container's cgroup. A cgroup kill catches setuid survivors. |
| 7 (networking) | Network namespaces. |
| 8 (terminals) | A monitor-owned pty for `-it`: a controlling terminal inside, job control, and input only in the foreground. `exec` on top of `supervise_joined`. |
| 9 (engine state) | **`exec` requirements:** the joiner must not sit stopped while its command has exited; warn when the init can't finish exiting (put the status pipe in the event loop); decide whether leftovers outlive an `exec` (Docker keeps them). |
| Housekeeping | One mutation no test catches: the init forgetting to close its copy of the report pipe only delays seeing `exec` succeed (shipyard notices once the init exits). Paths no test covers, because their trigger can't be caused on demand: `pass_on_pending` (a signal pending at the exact moment of the re-exec), the copy-mode warning (a failing re-exec), `target_is_exiting`'s fork-time branch (a container that starts exiting between opening and forking, or a real ENOMEM). |

**Resolved from stage 1:** `die_with_parent`'s `getppid()` check, which returns 0 inside
a PID namespace, became the go pipe's hang-up. Reaping inside the container is the
init's job.

## Tests

| Kind | Where | What |
|---|---|---|
| Unit | `crates/shipyard-runtime/src/**` | validation (`Isolation`, `Join`, hostnames, stop signals), clone layout and both paths, the status protocol (`init_reports_parse_strictly`), failure messages and hints, fd ranges, no init without a PID namespace (root) |
| Isolation | `crates/shipyard-cli/tests/isolation.rs` | 27 root tests and the "needs root" one |
| Terminal | `crates/shipyard-cli/tests/terminal.rs` | 16 ordinary and 11 `ROOT_TESTS`, picked like ignored tests on Linux |
| Joining | `crates/shipyard-runtime/tests/join.rs` | `harness = false`; the binary is its own runtime (`container` and `join` modes); 13 root tests, "needs root", and `LinuxOnly` on macOS |
| Kernel | `scripts/probes/*.c` | the checks above |

The end-to-end helpers live in `crates/shipyard-test-support` (a dev-only crate), since a
`tests/common` module compiled into several test binaries confused IDEs.

**Mutation-checked.** Every important fix was removed on a scratch copy, and its test
confirmed to fail. These runs use their own build cache (`shipyard-mutation-target`):
cargo reuses a mutated binary at the same path otherwise.

**Paths no test covers**, and why, are in the Housekeeping row above.

**CI:** the `root` job runs `--include-ignored` on Ubuntu and macOS. **Locally:**
`scripts/test-linux.sh` runs clippy, the normal-user pass, then the root pass with `/`
made shared. The x86-64 runs use `docker --platform linux/amd64` with their own volumes.

## Review history

- **Step 2:**
  - `clone`'s variadic arguments were 32-bit zeros (now full-width null pointers);
  - `clone3` EPERM now falls back;
  - root but refused is now `IsolationRefused`.
- **Step 3:**
  - the init as a copy leaked `-e` values through `/proc/1/cmdline` (now the clean
    re-exec);
  - no init without a PID namespace;
  - the setxid deadlock after a raw clone (now raw syscalls);
  - the x86-64 run found `execveat` missing (now the `/proc/self/fd` fallback).
- **Step 4:**
  - ENOMEM is no longer always "exiting";
  - a foreign `/proc` is refused;
  - the stopped-joiner rule is recorded.
- **Step 5, the full review (three adversarial reviewers with separate briefs; every
  finding verified first):**
  - the status pipe could be forged or filled;
  - the init could fail to start silently;
  - rprivate pinned host mounts;
  - SIGPIPE was lost across the re-exec;
  - stop signals the init can't forward;
  - 32-bit syscall numbers (already refused by stage 1's 64-bit-only build, it turned
    out; a duplicate check added then was removed in step 6's review);
  - an abort on a failed switch back;
  - the docs (README, `run.md`'s isolated mode, API docs, an evidence trail backed by new
    probe checks).
- **Found by the learning book's own Try-it:** Ctrl-Z then `fg` didn't resume a
  self-stopped isolated command.
- **Step 5's Murphy review:**
  - `pass_on_pending` bounded;
  - the hint for `InitDidNotStart`;
  - test cleanup, a static-build skip, and a timing margin.

  Checked and found fine there:
  - the status protocol against a forger racing the start mark, or padding the pipe so
    the init's writes fail: the length check rejects every such case, and the only
    well-formed forgery needs the init killed, which the cross-check catches;
  - the cross-check never rejects a genuine status;
  - `InitDidNotStart` can't be reported falsely;
  - `PR_SET_DUMPABLE` is set after the exec, which would reset it.

- **Step 6's Murphy review:**
  - the rewrite had dropped facts (the uncaught report-pipe mutation, the "considered and
    not done" choices), now restored;
  - a duplicate 32-bit check was removed;
  - the book's references into these notes were fixed;
  - stage 1's notes wrongly said PID namespaces fix macOS;
  - running the documented x86-64 command verbatim found that Rosetta deadlocks in the
    setxid stress test (a thread blocked in the clone, all signals blocked, waiting on an
    emulator lock; native: 0 hangs in 12). The test skips under Rosetta.
  - Also seen once under emulation: `the_container_dies_with_shipyard` timed out. It
    didn't reproduce in 8 parallel emulated runs or 40 direct kill checks, and never
    natively. Recorded, not changed.

- **Found by CI on real x86-64:** in the foreign-`/proc` join test, the joiner's thread
  watcher read `/proc/<child>/ns/pid`. The child's number is from the joiner's own new PID
  namespace, while `/proc` was the host's. On a GitHub VM, PID 2 is `kthreadd`, so the
  watcher read *its* namespace and reported a stray (exit 98 instead of 125). Locally the
  number was free, the read failed, and it was ignored. The watcher now gathers no
  evidence unless `/proc` is its own, the same rule as `proc_is_ours`. Reproduced by
  occupying low PIDs (`ns_last_pid`), with the old helper failing and the fix passing.

**Lessons worth keeping:**
- **Check kernel behaviour with a probe** before building on it.
- **A documented recovery path needs a test.**
- **A check that can't see must not count as a failure,** and one that can see the
  *wrong thing* is worse: a PID only names a process in a `/proc` of its own namespace.
- **Helper processes in a supervising test** are reaped by the subreaper.
- **A SIGKILLed test helper's forked copies** keep the test's pipes open.
- **Mutation runs need their own build cache.**

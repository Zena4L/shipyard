# Stage 1 notes: process lifecycle

The engineering record of stage 1: what was built, why it's built that way, what can go
wrong and how that's handled, and what was left for later stages. For how to *use* it, see
[Running commands](../user/run.md) and [Exit codes](../user/exit-codes.md).

## Outcome

Stage 1 asked for `shipyard run CMD` that spawns the command, waits for it, forwards
signals, stops it gracefully, returns its exit code, and reaps and cleans up everything it
started ([ROADMAP](../ROADMAP.md), [FEATURES §1](../FEATURES.md)). There's no isolation
yet: that starts in stage 2. All of it runs natively on macOS and Linux.

| Requirement | Where | Tested by |
|---|---|---|
| Spawn with exactly the requested environment, working directory, user, groups, umask and rlimits | `process/plan.rs`, `child.rs`, `spawn.rs` | `tests/run.rs` (environment, workdir, limits, umask, root user switch) |
| Faithful exit status: own code, `128 + N`, 125/126/127 when it never ran | `status.rs`, `failure.rs`, `error.rs` | `tests/run.rs` (exit codes, 125/126/127 cases), `failure.rs` unit tests |
| Signal forwarding | `signals.rs`, `supervisor.rs` (`should_forward`) | `tests/signals.rs`, `tests/terminal.rs` (kill to foreground/background) |
| Graceful stop: stop signal, `--stop-timeout`, SIGKILL | `supervisor.rs` (`StopTimers`), `stop.rs` | `tests/signals.rs` (stop signal, timeout escalation) |
| Reaping and cleanup of leftovers | `supervisor.rs` (`reap_children`, `kill_leftovers`) | `tests/signals.rs` (leftovers; escapees on Linux), `tests/run.rs` (inherited children survive) |
| Terminal-aware job control, `TERM` passthrough | `supervisor.rs` (`Terminal`, `follow_stops`) | `tests/terminal.rs` (15 scenarios on a pseudo-terminal) |

Paths below are relative to `crates/shipyard-runtime/src/` and `crates/shipyard-cli/`.

## Architecture

```
shipyard run (CLI: run.rs)
  └─ Supervisor::install()                     signals.rs: handlers, wake pipe, subreaper
  └─ Supervisor::supervise(spec, stop)
       ├─ spawn()                              spawn.rs
       │    ├─ report pipe + reader            channel.rs   (before fork: can fail)
       │    ├─ ChildPlan                       plan.rs      (before fork: allocates)
       │    ├─ fork, all signals blocked
       │    └─ child: child_main()             child.rs     (after fork: no alloc, no panic)
       │         reset signals → process group → mark fds CLOEXEC → rlimits → umask
       │         → chdir → setgroups/setgid/setuid + verify → pdeathsig (Linux)
       │         → clear the mask → execve (PATH search)
       │         a failure is sent as {step, errno, detail} on the report pipe, then _exit
       └─ watch(): the event loop              supervisor.rs
            poll(wake pipe, report pipe) → drain signals → forward / stop request
            → stop timers → exec report → reap (peek first) → follow stops
```

The rule that shapes everything: **between `fork` and `exec` only `child.rs` runs**, and it
may not allocate, panic or take locks (TLPI §21.1.2 and ch. 24–27). Everything it needs is
prepared in `plan.rs`. `CLAUDE.md` states the rule for contributors.

## Decisions and why

- **Our own `fork` + `execve`, not `std::process::Command`.** Stage 2 needs the same child
  pipeline behind `clone3`, and the child setup order (rlimits before the user switch,
  pdeathsig after it) must be under our control (TLPI ch. 24, 27).
- **`nix` first, raw `libc` where nix can't help:** `execve` (nix's allocates), `waitid`
  (missing on macOS), real-time signals, `setgroups` (nix's `Gid` isn't guaranteed to
  have `gid_t`'s layout). Recorded in the [ROADMAP](../ROADMAP.md#decisions).
- **A close-on-exec report pipe** carries setup failures from the child as a 16-byte
  record, so the parent can explain "permission denied" versus "interpreter missing", and
  map them to 125/126/127 like Docker (TLPI §27.4 and the "pipe trick").
- **Supervision in the CLI process.** Daemonless: `shipyard run` itself is the supervisor.
  A self-pipe wakes one `poll` loop (TLPI §63.5.2); handlers only set atomic flags.
- **Process groups depend on the terminal.** With a controlling terminal the command
  shares shipyard's group, so the terminal's Ctrl-C, Ctrl-Z and hangup reach both and the
  shell's job control just works (TLPI ch. 34). Without one (CI, services) the command gets
  its own group, so cleanup can kill all of it. Handing the terminal to the command is
  stage 8 (pseudo-terminals).
- **Where a signal came from decides whether it's forwarded.** The handler records each
  delivery's source from `siginfo`. On Linux `si_code > 0` means kernel/terminal; the
  sender's PID can't be trusted alone (a sender in an ancestor PID namespace shows PID 0).
  macOS can't tell a terminal's Ctrl-C from a `kill` (both `si_code` 0), so in a shared
  group in the foreground it doesn't forward the terminal's signals.
- **SIGTERM is a stop request, like `docker stop`**: the stop signal, SIGCONT, then
  SIGKILL after `--stop-timeout`. SIGTERM, SIGCHLD and SIGCONT are caught even if the
  parent ignored them, and every caught signal is unblocked: a blocked SIGCHLD would hang
  shipyard forever, an ignored SIGTERM would make it unstoppable.
- **`nohup` and `&` keep working.** SIGHUP, SIGINT and SIGQUIT that were ignored at
  startup stay ignored in the command. Every other disposition is reset.
- **Stop-following.** With a terminal, if the command stops itself, shipyard stops too
  (SIGSTOP), so the shell sees a stopped job. It never does that during a stop request
  (it must stay awake to SIGKILL) or without a job-control shell above it (an orphaned
  group: nothing would resume it).
- **Linux subreaper.** Shipyard adopts what the command leaves behind and kills it when the
  command exits; children it already had (`helper & exec shipyard run app`) are left alone.
- **Docker's exit codes:** 125 for shipyard's own errors, 126 for "found but can't run",
  127 for "not found", `128 + N` for death by signal N.

## macOS and Linux differences

| Topic | Linux | macOS |
|---|---|---|
| Child dies with shipyard | `PR_SET_PDEATHSIG` | not available: a SIGKILLed shipyard leaves the command running |
| Leftover processes | subreaper: all of them killed, even after `setsid` | only the command's own group (no terminal); with a terminal they can leak |
| Marking inherited fds close-on-exec | one `close_range` call | loop over the `/dev/fd` list |
| Terminal Ctrl-C vs `kill` | `si_code` tells them apart | indistinguishable; foreground rule instead |
| `waitid(WEXITED)` | exits only | also reports *stopped* children (filtered by `si_code`) |
| `setpgid` from parent and child at once | fine | the child's call can fail with EPERM; judged by the result, retried |
| Orphaned background group using the terminal | allowed (without `TOSTOP`) | writes fail with EIO, and shipyard finds no usable terminal (it runs as if there were none) |
| `nofile=unlimited` | refused (capped at `fs.nr_open`) | accepted by recent versions; older ones fall back to `kern.maxfilesperproc` |
| Supplementary groups | `setgroups` sets them exactly; up to 65536 | the primary group shares the list; up to 15 extra |
| Credential switch | `setresgid`/`setresuid` | `setgid`/`setuid` (as root they set all three IDs) |

## Failure-mode register

| Worst case | Handling | Test |
|---|---|---|
| Parent leaked descriptors | marked close-on-exec in the child, including above a lowered limit | `leaked_descriptors_do_not_reach_the_command`, `descriptors_above_a_lowered_limit_do_not_reach_the_command` |
| Parent ignored or blocked signals | dispositions reset, mask cleared in the child; caught signals unblocked in shipyard | `ignored_signals_are_reset_for_the_command`, `blocked_signals_are_unblocked_for_the_command`, `a_blocked_sigchld_does_not_hang_shipyard`, `a_blocked_sigterm_still_stops_the_command` |
| Parent ignored SIGCHLD (Linux would discard the exit status) | SIGCHLD always caught | `an_ignored_sigchld_does_not_lose_the_exit_status` |
| Parent ignored SIGTERM | always caught | `a_sigterm_ignored_at_startup_still_stops_the_command` |
| A signal arrives between `fork` and `exec` | all signals blocked across `fork`; the child resets before unblocking | covered by every spawn |
| Something fails after `fork` | nothing fallible after `fork` in the parent; the command's PID is recorded while signals are still blocked | — |
| PID reuse | signals only unreaped children; peeks with `WNOWAIT`, so the group can't be reused while it's killed | `stop_peeks_see_only_a_current_stop_and_consume_nothing` |
| Command ignores the stop signal | SIGKILL after `--stop-timeout` | `a_command_that_ignores_the_stop_signal_is_killed_after_the_timeout` |
| Command answers SIGTERM by stopping itself | no stop-following during a stop request | `a_stop_request_still_kills_a_command_that_stops_itself` |
| Command stuck in the kernel after SIGKILL | waits, warning after 5 s and every 30 s; never hangs silently | — (needs a "D"-state process) |
| Command paused by someone else | a stopped child isn't taken for an exited one | `a_command_paused_and_resumed_by_someone_else_carries_on` |
| Signals coalesce | per-signal flags; the pipe is drained before the flags | `a_flag_set_after_the_pipe_was_read_is_still_collected` |
| Double delivery of Ctrl-C | kernel/terminal signals not forwarded in a shared group | `ctrl_c_reaches_the_command_exactly_once` |
| Command signals shipyard | not bounced back | `signals_from_the_command_are_not_bounced_back` |
| `nohup` / `&` in scripts | kept ignored in the command; no re-raise of an ignored SIGINT | `nohup_still_protects_the_command`, `ignoring_sigint_is_respected_like_a_background_job_in_a_script`, `shipyard_does_not_re_raise_a_sigint_it_was_told_to_ignore` |
| Command stops itself in a terminal | shipyard follows; only with a job-control shell above it | `a_command_that_stops_itself_stops_shipyard_too`, `without_job_control_shipyard_does_not_stop_itself` |
| Terminal hangs up | the command gets SIGHUP; shipyard exits 129 | `hangup_ends_the_command_and_shipyard` |
| Children shipyard had before | left running (Linux cleanup skips them) | `children_shipyard_had_before_are_left_running` |
| `/proc` from another PID namespace | cleanup skipped with a warning instead of killing the wrong PIDs | — (needs `unshare -p` without `--mount-proc`) |
| Secrets in errors or logs | values never logged; a bare `-e "$TOKEN"` isn't logged as a name | `environment_values_never_reach_the_logs`, `debug_hides_values` |
| `--user 4294967295` (means "unchanged") | rejected | `reserved_ids_are_rejected` |
| Root's groups leaking into the command | `setgroups` always called; the switch is verified, including that root can't be regained | `root_switches_user_and_drops_its_own_groups` (root) |
| Installed setuid or with file capabilities by mistake | refuses to start (`startup.rs`) | — (needs a privileged install) |

## Known limitations (by design, for stage 1)

- **macOS, with a terminal:** leftovers the command starts in the background can outlive
  it. (Linux never had this gap: the subreaper finds them.) Isolation removes it, which
  macOS gets with the VM (stage 12).
- **macOS, foreground:** `kill -INT <shipyard>` isn't passed on (it looks exactly like the
  terminal's Ctrl-C). `SIGTERM` and other signals are.
- **A signal sent to shipyard's whole group in terminal mode** reaches the command twice
  (once directly, once forwarded); the same trade-off as sudo.
- **macOS has no parent-death signal:** if shipyard is killed with SIGKILL, the command
  keeps running.
- **Only 64-bit platforms** build (a `compile_error!`).

## Carried forward

| Stage | Item |
|---|---|
| 2 (namespaces) | `child.rs` `die_with_parent`: `getppid()` returns 0 inside a new PID namespace, so the "parent still alive" check must change with `clone3`. The supervisor's reaping becomes the PID-1 init's job. |
| 3 (OCI bundles) | rlimits: map `u64::MAX` to unlimited and reject other values at or above infinity. `Env` is O(n²) for `set`/`get`: index it before loading large OCI env lists. `ProcessSpec`/`User` need a constructor or builder before new fields arrive. User and group *names*. |
| 4 (security) | `verify_credentials`: for a non-root target, also require empty effective, permitted and ambient capability sets (`capget`). |
| 12 (macOS VM) | `MAX_GROUPS` is a compile-time constant of the build target; check it where the process is spawned (inside the VM). |
| Housekeeping | Pin CI actions to commit SHAs (with Dependabot) when the repository goes public. `RlimitKind::ALL` is maintained by hand. The test files still have two timeout strategies (a thread with a channel, and `try_wait` polling). |

## Tests

| Kind | Where | What |
|---|---|---|
| Unit | `crates/shipyard-runtime/src/process/*.rs` | parsing and validation (including property tests), failure messages, the report record, `waitid` peeks, signal-source policy |
| End to end | `crates/shipyard-cli/tests/run.rs` | exit codes, environment, limits, credentials, hostile parents |
| Signals | `crates/shipyard-cli/tests/signals.rs` | forwarding, stop requests, cleanup, without a terminal |
| Terminal | `crates/shipyard-cli/tests/terminal.rs` + `crates/shipyard-test-support/src/pty.rs` | a fake job-control shell on a pseudo-terminal; single-threaded, `harness = false` |
| Root only | `#[ignore]`d tests | run by CI's `root` job on Linux and macOS through `sudo -E` |

Every fix from the reviews has a regression test, and the important ones were checked by
mutation: the fix removed on a scratch copy, the test confirmed to fail.

**CI** (`.github/workflows/ci.yml`): fmt; clippy and tests on Linux and macOS; the tests as
root on Linux and macOS; clippy for musl; docs with warnings as errors; cargo-deny.

**Locally:** see [DEVELOPMENT.md](../DEVELOPMENT.md). On a Mac, `scripts/test-linux.sh`
runs the Linux checks in a container, as a normal user and as root.

## Review history

Each step ended with a review; the notable finds:

- **Steps 1–3:**
  - errno read after `close`, which can overwrite it;
  - a secret could be echoed in an `--env` error;
  - macOS treats `RLIMIT_RSS` as `RLIMIT_AS`, so `rss` became Linux-only.
- **Step 4 (three parallel reviews):**
  - the sender's PID can't classify signals (double Ctrl-C on macOS, `docker stop`
    ignored across PID namespaces);
  - a blocked SIGCHLD hung shipyard;
  - `nohup` was defeated;
  - relative paths were checked from the wrong directory;
  - macOS `nofile=unlimited` lowered the limit;
  - a 1-in-1,000 `setpgid` race on macOS.
- **Step 5:** the pseudo-terminal harness found that macOS's `waitid(WEXITED)` reports
  stopped children, so shipyard SIGKILLed any paused command.
- **After step 5 (three parallel reviews):**
  - stop-following could defeat a stop request, or freeze an orphaned job;
  - Linux cleanup killed children shipyard inherited through `exec`;
  - `/proc` from another PID namespace could name the wrong processes;
  - an ignored SIGTERM made shipyard unstoppable;
  - a token could reach the debug log;
  - the Ctrl-C test passed without testing anything;
  - dash's special-builtin rule would have failed Linux CI.
- **Step 6:** the whole suite run on Linux for the first time (`scripts/test-linux.sh`).
  Every end-to-end, signal and terminal test passed there, as a normal user and as root.
  It found three unit tests that only passed on macOS: two still expected
  `nofile=unlimited` to parse (Linux now refuses it), and one depended on whether the
  tests ran as root (fixed by recording "running as root" in the failure context).

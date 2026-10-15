# Stage 8 notes: terminals, I/O and logs

The engineering record of stage 8: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [Running commands](../user/run.md), [The OCI
runtime](../user/runtime.md), [OCI bundles](../user/bundles.md) and [Exit
codes](../user/exit-codes.md).

## Outcome

Stage 8 asked for a per-container monitor, pseudo-terminals, attach and detach, `exec`, log
files with rotation and `cp` ([ROADMAP](../ROADMAP.md), [FEATURES
§8](../FEATURES.md#8-terminals-io-and-logs--stage-8)). It also took on descriptor passing
(`--preserve-fds`, socket activation), other firewalls (F14), and from the hardening track
real-host CI (H4), differential tests and the OCI conformance suite (H5), timing budgets
(H7) and the engine crate (H1). D13 set the scope: every piece is built, but the commands
by name wait for stage 9's state. A user can now:

- run `shipyard run -it --image alpine sh`: a pseudo-terminal made inside the container,
  with job control, `vi`, password prompts and resizing; without `-t` a container never
  holds the user's terminal, and without `-i` its stdin is `/dev/null`;
- run bundles that ask for a terminal (`process.terminal`, as every `runc spec` config
  does), relayed by `shipyard run --bundle` or sent to `create --console-socket`;
- `shipyard-runtime create --log-dir DIR [--stdin]`, then `logs [-f]`, `attach` (and
  detach with Ctrl-P Ctrl-Q), `exec [-t]` and `cp`, by runtime ID, root or rootless;
- pass descriptors and systemd's listening sockets into a container.

**The milestone** is that whole chain, rootless, on one container. It's proved by
`the_stage_8_milestone_one_rootless_container_end_to_end` (`tests/runtime.rs`: create with
logs and `--stdin`, `logs --follow`, attach and detach, `exec -t`, `cp` in and out, kill
and delete, then the state directory, the cgroup and the monitor all gone). It was also run
by hand on 2026-10-11 (step 8.8b), rootless, on two Lima VMs: Ubuntu 24.04 with AppArmor's
user-namespace restriction on and Shipyard's profile installed, and Fedora with SELinux
enforcing and firewalld running. A script drove a pseudo-terminal through:

- `shipyard run -it --image alpine sh`: the prompt, a background job and `jobs`, Ctrl-Z
  stopping it, `vi` writing a file, a hidden `read -s` password prompt, Ctrl-C ending the
  foreground program, `exit 0`;
- `shipyard-runtime create --log-dir --stdin`, `start`, `logs --follow`, `attach` (typing
  reaches the program; Ctrl-P Ctrl-Q then Enter detaches and it runs on), `exec -t` (a
  `/dev/pts` terminal), `cp` in and out, then `kill` and `delete` (nothing left).

14 of 14 checks passed on both hosts. The run found one real bug, fixed in 8.8b (see the
[register](#failure-mode-register)): attaching from a terminal to a container *without* a
terminal, Ctrl-P Ctrl-Q never detached.

The stage was built in seventeen steps, each planned, built, reviewed twice (clean code,
then what could go wrong), mutation-checked and run by hand on Linux, then reviewed as a
whole (see [Review history](#review-history)).

| Requirement | Where | Tested by |
|---|---|---|
| The engine crate (H1), only the CLI may depend on it | `crates/shipyard-engine`, `deny.toml` | `cargo deny check` (a runtime → engine dependency fails it, checked in a scratch copy) |
| No host terminal for a container (D16); stdin `/dev/null` without `-i` (D17); `-i` | `process/stdio.rs` (the relay), the supervisor, the engine's `streams.rs` | `streams.rs`'s 8 tests; `without_i_stdin_is_empty`, `with_i_piped_input_reaches_the_command` (`rootless.rs`); `an_isolated_command_never_holds_the_host_terminal`, `a_bundle_run_never_holds_the_host_terminal` (`terminal.rs`) |
| `-t`: a pty from the container's own devpts, the master back over `SCM_RIGHTS`, raw mode put back on every way out | `process/terminal.rs`, `stdio.rs` (`PtyRelay`), `init.rs` (`make_terminal`), `child.rs` (`take_terminal`) | `terminal.rs`: `with_t_tty_names_the_containers_own_terminal`, `with_t_stty_size_follows_the_window`, `with_t_dev_tty_opens`, `with_it_ctrl_z_stops_the_inner_job_not_shipyard`, `the_terminal_is_restored_after_kill_9_of_the_command`, `a_background_job_holding_the_terminal_doesnt_hang_the_run`, `the_terminal_survives_stops_around_the_end_of_runs` |
| `-t` without isolation, macOS too | `terminal.rs` (`host_pty`, `host_hung_up`) | `unisolated_t_gives_a_terminal_without_one_around` (`run.rs`), `unisolated_t_gives_a_terminal_of_its_own` (`terminal.rs`) |
| OCI terminals: `process.terminal`, `consoleSize`, `--console-socket` (runc's protocol) | `oci/mod.rs`, `container/lifecycle.rs` (`console_matches`), `init.rs` | `create_with_a_console_socket_sends_the_master`, `the_console_size_is_the_terminals`, `a_long_console_socket_path_is_reached`, `a_rootless_container_gets_its_console`; `a_runc_spec_config_runs` (`bundle.rs`); `a_terminal_bundle_runs_on_its_own_terminal` |
| The monitor's event loop; CRI logs, rotated, versioned by `log.json` (D15) | `container/lifecycle/monitor.rs`, `capture.rs`; `logs/` (`time.rs`, `cri.rs`, `info.rs`, `writer.rs`) | `logs/`'s 26 tests (`rotation_keeps_the_bound_and_the_order`, `the_same_as_date_says`, …); `a_flood_stays_within_the_rotation_bound`, `a_dead_monitor_means_a_broken_pipe`, `the_monitor_stays_small_while_it_logs`; the `cri_record` and `logs_dir` fuzz targets |
| `logs [-f] [--tail] [--since] [-t]` | the engine's `logs.rs`, the CLI's `runtime_commands.rs` | the engine's 7 tests (`following_survives_rotations`, …); `logs_tail_since_and_timestamps`, `follow_goes_on_across_rotations_until_the_end` |
| Attach: a versioned frame protocol, `attach.sock`, `SO_PEERCRED`, 16 clients × 256 KiB, detach keys, `--stdin`, `--stdin-once` | `shipyard-runtime/src/attach.rs`, `container/lifecycle/attach.rs`, the engine's `attach.rs` | `attach.rs`'s 8 tests; the 14 attach tests in `runtime.rs` (`a_stalled_client_doesnt_slow_the_container`, `the_seventeenth_client_is_refused`, `another_user_cant_attach`, `attach_with_the_longest_id`, …); `attach_to_a_terminal_container`, `attach_from_a_terminal_to_a_plain_container`; the `attach_frames` fuzz target |
| `exec`, confined (F6): a joiner from the sealed copy, non-dumpable, placed at birth, confined from `exec.json` | `process/exec.rs`, `child.rs`, `spawn.rs` (`spawn_joined`), `container/lifecycle/exec.rs` | the 23 exec tests in `runtime.rs` (`the_container_cant_reach_exec_before_it_runs`, `exec_never_runs_the_binary_on_disk`, `exec_holds_no_host_descriptors`, `editing_config_json_after_create_changes_nothing`, `exec_cant_widen_the_container`, `exec_runs_in_the_containers_cgroup`, `a_rootless_exec_joins_its_user_namespace`); `lifecycle/exec.rs`'s 7; the `process_json` fuzz target |
| `exec -t`, `exec -d -t --console-socket` | the joiner, `init::terminal_from` | `runtime.rs`'s 7 (`exec_d_t_sends_the_master_to_the_console_socket`, `exec_t_refuses_a_ptmx_that_isnt_one`, `a_rootless_exec_gets_its_terminal`, …); `exec_t_on_a_terminal`, `exec_t_in_the_background_starts_its_command` |
| `cp` both ways, Docker's path rules, `-` tar streams, `-a`, `-L` | `container/helper.rs`, `process/helper.rs`, the engine's `cp/`, `shipyard-image`'s `unpack_tar` | the 20 cp tests in `runtime.rs` (`cp_through_a_swapped_link_stays_inside`, `cp_never_replaces_a_device`, `cp_streams_agree_with_gnu_tar`, `a_rootless_cp_reaches_files_of_subordinate_ids`, …); `unpack.rs`'s copy tests; the `cp_unpack` fuzz target |
| `--preserve-fds N` and socket activation (`LISTEN_PID` the process's own) | `process/passed.rs`, `child.rs` (`place_passed`) | the 10 fd-passing tests in `runtime.rs`; `preserved_descriptors_reach_the_command` (`run.rs`); `an_isolated_run_passes_socket_activation_on` (`isolation.rs`); `passed.rs`'s 3 |
| Other firewalls (F14): firewalld, Docker, ufw, reject-ending chains; `doctor`'s `firewall` line | `shipyard-network/firewalls.rs`, `nft/decode.rs`, the CLI's `network.rs`, `doctor.rs` | `firewalls.rs`'s 10 tests; `firewall.rs`: `dockers_firewall_is_warned_about_and_its_printed_fix_works`, `firewalld_is_warned_about_until_the_bridge_is_trusted`, `firewalld_that_doesnt_forward_keeps_the_way_out_until_trusted`, `an_nftables_chains_printed_fix_works` |
| Rootless pasta under SELinux: the network namespace as a file, a watching launcher | `shipyard-network/pasta.rs` | `pasta_gets_the_namespace_as_a_file_the_host_never_sees_mounted`, `roots_pasta_run_leaves_no_mount_on_the_host` (`tests/pasta.rs`) |
| Real hosts in CI (H4), budgets (H7), differential tests (H5, H8) | `.github/workflows/ci.yml` (built; not yet run on stage 8: see D14), `scripts/ci/`, `tests/budgets.rs`, `route/linux_tests.rs`, `nft/linux_tests.rs` | `apparmor-control.sh` (ok, fail without the profile, ok); `nft_reads_and_rebuilds_shipyards_table`, `dig_reads_the_servers_answers_as_meant`, `the_same_as_date_says`; `a_terminal_adds_little_to_a_run` (`rootless.rs`), `an_exec_starts_and_ends_quickly` (`runtime.rs`) |
| The OCI conformance suite (H5) and what it found: mounts in order (F32), `poststart` after `execve` (F35), tmpfs options on binds (F33) | `scripts/conformance/`, `isolation.rs` (`mount_order`, `check_mount_order`), `rootfs.rs`, `init.rs` | the gate (28 pass, 13 fail for a stated reason, 17 skip on arm64); `mounts_are_made_in_the_order_listed`, `a_mount_a_later_one_would_hide_is_refused`, `poststart_runs_once_the_program_has_started`, `a_hooks_leftover_doesnt_hold_poststart_back`, `only_poststart_hooks_make_a_started_fifo` |

**Moved out of stage 8:**

| Item | Now in | Why |
|---|---|---|
| `logs`, `attach`, `exec`, `cp` by name; `run -d`; `ps` | stage 9 | D13: they need the one state model (H2) |
| `run -it` and `run -d` through the monitor | stage 9 | `run` supervises in-process today; the monitor serves only `shipyard-runtime` |
| `cp` from a stopped container | stage 9 | its root filesystem is reached through stage 9's state |
| `HOME` and user names for `exec`, names in `cp -a` | stage 9 | with names (`/etc/passwd`) |
| Joining a `pid`, `ipc`, `uts` or `mount` namespace by path (F34) | stage 10 | Compose's `service:` forms need it; joining safely is its own design |
| Mutants and scheduled fuzzing in CI (H5) | stage 9 | only the conformance and differential parts of H5 were stage 8's |
| `logs`/`attach` entry points by root and ID in the engine (F38) | stage 11 | the API needs them; the CLI holds that logic today |

## Architecture

```
shipyard-runtime create ID  [--console-socket S] [--log-dir D [--stdin [--stdin-once]]]
                            [--preserve-fds N]
  checks the bundle and the flags (runc's two console refusals, D's rules)
  ROOT/ID/ (0700): state.json, exec.fifo, exec.started (only with poststart hooks)
  spawn_monitor: /proc/self/exe "__shipyard-monitor" BUNDLE DIR READY
                 [console=N] [logs=N] [passing=JSON]   (a session of its own, cwd "/")
  └─ shipyard-monitor        one per container; the container's parent; no daemon
       Capture::new: output pipes, or a socket pair the master will come on;
                     a stdin pipe with --stdin (else /dev/null)
       spawn: clone3 → the child re-execs the sealed copy as "__shipyard-init"
       └─ init (PID 1; a session of its own; opens ptmx O_NOCTTY, never takes the tty)
            pty: ptmx → unlock → TIOCSWINSZ → TIOCGPTPEER → fchown → send the master:
                 to the monitor (socket pair) or the caller's console socket ("/dev/ptmx")
            waits at exec.fifo for `start`; holds exec.started (close-on-exec)
            └─ the command (PID 2): setsid, TIOCSCTTY, passed fds dup2'd onto 3..,
               LISTEN_PID filled in place, execve
       exec.json (v1: the process and its confinement, as spawned), then "created"
       poll loop: pidfd, output, attach.sock (+ ≤16 clients), the stdin queue
         output → log dir/container.log (CRI, rotated), and each client's 256 KiB
       the end: reap → cgroup removed → drain (≤2 s) → exit recorded → Exited to clients

shipyard-runtime exec [-t] [-d] ID CMD
  └─ the sealed copy as "__shipyard-exec": the joiner (its setup in a sealed memfd)
       limits, OOM score; namespaces owned outside the container's user namespace;
       PR_SET_DUMPABLE 0; setns(user), 0 again; [-t: ptmx through /proc/<init>/root,
       checked to be devpts 5:2]; setns(pid); clone3(CLONE_PARENT|CLONE_INTO_CGROUP)
       writes {pid, in_cgroup}; exits
  └─ the exec process (exec's own child; after a raw clone): waits for go, joins the
       rest (mount last), close_range all but report, go (and its terminal),
       confinement, credentials (non-dumpable after each), chdir inside, execve

shipyard-runtime cp ID:SRC DEST | SRC ID:DEST
  └─ the sealed copy as "__shipyard-helper" (cp-out | cp-in): non-dumpable; joins user
       then mount; root inside with CHOWN, DAC_OVERRIDE, DAC_READ_SEARCH, FOWNER, FSETID
       and no_new_privs; openat2(RESOLVE_IN_ROOT|RESOLVE_NO_MAGICLINKS) from "/";
       a tar stream on its stdin or stdout, to or from the engine's cp

shipyard-runtime attach ID   → /proc/self/fd/N/attach.sock (N: the state directory)
shipyard-runtime logs ID     → the log directory, read through its descriptor
shipyard run -it             → the in-process supervisor: the same init and pty, the
                               master over a socket pair, relayed by the CLI
```

**The processes.** Every one is Shipyard's own binary started again with a hidden first
argument, so each binary calls `process::init_entry_point()` and
`container::monitor_entry_point()` first in `main`. The init, the joiner and the helper go
into the container, so they're started from a sealed in-memory copy (CVE-2019-5736): their
`/proc/self/exe` is `memfd:shipyard-init`, never the file on disk. The monitor stays on the
host and runs `/proc/self/exe`. The joiner and the helper share `process/internal.rs`
(`start_sealed`, the setup in a sealed memfd, never argv, the namespace split).

**How descriptors travel:**

- **Gate FIFOs.** `exec.fifo` holds the init until `start`. `exec.started` (8.7b) is
  opened `O_RDWR|O_CLOEXEC` by the init; the command inherits it and loses it at
  `execve`, so `start` sees no writer left once the program runs.
- **The master,** over `SCM_RIGHTS`: to the CLI or the monitor on a socket pair, with a
  one-byte acknowledgement; to a console socket with the name `/dev/ptmx` and no answer
  (runc's). No other descriptor crosses.
- **Report pipes:** the init's, the joiner's 8-byte `{pid, in_cgroup}` record, the
  helper's `HelperFailure`. Failures after a fork are numbers (`ChildStep`, now up to
  `StartedFifo`, 75).
- **The monitor's arguments** are tagged descriptor numbers; passed descriptors go as
  their own numbers with CLOEXEC cleared in the fork's `pre_exec`.
- **Passed descriptors** cross four sweeps (the monitor's, the re-exec's, the init's, the
  command's). Each hop renumbers what the command uses above `3+n`, then `dup2`s the
  sources onto `3..`.

**State directory** (`ROOT/ID`, 0700): `state.json`, `exec.fifo`, `exec.started`, `exit`,
`killed`, `exec.json` (0600, v1), `attach.sock` (0600). The log directory is the caller's:
`container.log`, its rotated files, `.container.log.next` during a rotation, and
`log.json` (format `cri`, version 1, `maxSize`, `maxFiles`). It outlives `delete`.

## Decisions and why

**Made with the user, before building (2026-10-09):**

- **D13, scope:** pieces now, commands by name in stage 9. Stage 8 ships what needs no new
  state: `run -it`, and in `shipyard-runtime` terminals, a confined `exec`, the monitor's
  logs and attach, and `logs`/`attach`/`cp` by runtime ID. v0.2 stays after stage 9.
- **D14, real hosts:** CI jobs that run rootless on Ubuntu with AppArmor's restriction
  left on and `contrib/apparmor/shipyard` installed, and in a Fedora VM (SELinux
  enforcing, firewalld) under QEMU/KVM. *Built, not yet run on stage 8's code:* CI is
  manual-only while billing is off, and its Ubuntu `check` and `root` jobs are switched
  off. Stage 8's evidence is `scripts/test-linux.sh` (both passes) and the 8.8b manual
  runs on Ubuntu 24.04 and Fedora VMs, all on arm64: nothing in stage 8 has run on
  x86-64 yet.
- **D15, logs** in the CRI ("k8s-file") format, `RFC3339NANO stream P|F text`, rotated by
  size and count. *Amended after the plan review:* the files stay pure CRI, with no header
  line, so `crictl` and fluent-bit's `cri` parser read them. The version is in a sidecar
  `log.json`, written before the first record; an unknown version is refused.
- **D16:** without `-t`, an isolated command never holds a host terminal descriptor. Where
  Shipyard's stdio is a terminal, the command gets a pipe Shipyard relays. This closes
  `TIOCSTI` into the user's shell and F17 (settings not restored) for every run.
  *Extended in 8.8c* to `shipyard-runtime exec` (runc passes the terminal on): relayed,
  or `/dev/null` with `-d`. A container `create`d from a terminal without `--log-dir`
  still keeps it, as with runc: that's the caller's choice of stdio.
- **D17:** without `-i`, the command's stdin is `/dev/null`, as Docker's.
- D16 and D17 change v0.1's behaviour; the CHANGELOG says so under "Changed".

**Made in the steps:**

- **No empty `terminal` module** (8.1a): it arrived with its first code in 8.2b. A
  cargo-deny rule keeps the engine above the runtime.
- **The pty is made inside the container** (8.2b), from its own devpts, and the slave
  comes from `TIOCGPTPEER`, never a path a container could overmount (T3). The init
  opens `ptmx` with `O_NOCTTY` (T2); only the command takes the terminal (T4).
- **The run ends on the command's exit status,** not the master's EIO, then drains for a
  bounded time (T8): a background job can hold the slave.
- **Raw mode only from the foreground** (T5): `tcgetpgrp` = `getpgrp` first; in the
  background `run -it` waits for `fg`. A SIGTSTP handler restores, stops, and on resume
  goes raw again only in the foreground.
- **Hang-up is asked of the terminal** (`host_hung_up`), not read from SIGHUP's sender:
  the kernel signals only the session leader, which passes it on.
- **Without a root filesystem the container still gets its own devpts** (8.2a, pulled
  forward by a review): the host's terminals aren't reachable by path.
- **OCI terminals follow runc** (8.3a): a terminal without `--console-socket` and the
  reverse are refused; the master is sent with a name and no acknowledgement. The
  default size is 24x80 (runc leaves 0x0, which full-screen programs can't use). `run
  --bundle` treats `process.terminal` as `-t`. The init gives the slave as its own stdio
  on a console socket, so `create`'s caller gets its stdio back at once.
- **Capture is opt-in** (8.3b): `--log-dir DIR`, not annotations, so a bundle can't switch
  it on. 10 MiB × 5 by default (containerd's). The directory must be new (made 0700) and
  outlives `delete`.
- **If the monitor dies,** it takes the init down; a process that escaped the init (no
  `pid` namespace) gets EPIPE on its next write (conmon's): no spare reader.
- **Timestamps are Shipyard's own** small UTC module, with a differential test against
  `date` (an owned component, H8).
- **Attach** (8.3c): stdin is opt-in (`--stdin`); detaching never closes it, except with
  `--stdin-once` (pipes only). 16 clients × 256 KiB; a client that falls behind is
  dropped, never slowing the container. New output only: the past is `logs`. The detach
  keys are matched by the monitor, so every client behaves the same.
- **Exec's joiner is an intermediate process from the sealed copy** (8.4a): a rootless
  caller can't `setns` a PID namespace from its own thread, and the process that enters
  the container must not be the binary on disk. `CLONE_PARENT` makes the exec process
  `exec`'s own child.
- **Non-dumpable before the first `setns`,** re-asserted after `setns(user)` and after
  each credential call (T11, E3). Hosts with `fs.suid_dumpable=1` are refused.
- **`exec.json`, a versioned snapshot** written by the monitor from the bundle it spawned:
  editing `config.json` later changes nothing. `--process` and flags narrow, never widen.
- **EBUSY is refused, not worked around** (no sub-cgroups of Shipyard's); a paused
  container is refused (T13: a frozen leaf wouldn't run). Numeric users only.
- **Exit codes as runc:** `exec`'s own failure is 255; 8.8a gave `attach` the same rule.
- **`exec -t`'s terminal is made by the joiner** through `/proc/<init>/root` (8.4b), with
  `ptmx` opened `O_PATH|O_NOFOLLOW` and checked to be devpts's 5:2 before it's opened.
- **`cp` is a helper inside the container** (8.5a), not a host-side walk of
  `/proc/<pid>/root`: a rootless user can't read subordinate IDs' files from there (T12).
  The runtime holds a generic helper hook; the archive logic is the engine's. The `tar`
  crate's `Builder` is fed only descriptors Shipyard opened. A copy never replaces a
  device, FIFO or socket.
- **`LISTEN_PID` is the process's own PID** (8.5b), written after the fork into a slot
  made before it, since Shipyard's init is PID 1 (runc writes 1). runc's layout: listening
  sockets first, then the preserved ones.
- **Other firewalls are detected, never changed** (8.6). firewalld's fix is its trusted
  zone; Docker's is three `DOCKER-USER` rules (never `-i BRIDGE` alone). A fix counts as
  applied only rule by rule, in the right direction.
- **The conformance gate** (8.7a): runtime-tools pinned at `8a4db579`; every test passes,
  or fails for a reason its output must match, or is skipped with a reason.
- **Mounts in the listed order** (8.7b): unlisted standard mounts first, or after their
  listed parent; an order that would hide a mount is refused at load.
- **`poststart` waits for the command's `execve`,** only when the bundle has such hooks;
  on a timeout the hooks run anyway with a warning (`start` mustn't fail once the
  container is let go).
- **Flow control off while attached** (8.8b): attaching from a terminal to a container
  without one, `RawMode::without_flow_control` turns off only `IXON`/`IXOFF`, so Ctrl-Q
  reaches the detach matcher; line editing and echo stay.

## Kernel checks

`scripts/probes/terminal.c` (8.1a), as root on the test container (OrbStack, Linux 7.0.14,
arm64, `legacy_tiocsti=1`), under x86-64 emulation, and on a Lima Ubuntu 24.04 VM (Linux
6.8, `legacy_tiocsti=0`):

| | Question | Result |
|---|---|---|
| T1 | devpts `newinstance` rootless, with `gid=5` unmapped / without | EINVAL / ok; `ptmx` through a `pts/ptmx` link opens a master |
| T2 | `TIOCGPTPEER` before / after `unlockpt` | EIO / ok. Without `O_NOCTTY`, a session leader with no terminal acquires it |
| T3 | another devpts mounted over the first | `TIOCGPTPEER` still gives the first's slave; the path is ENOENT |
| T4 | PID 2 `setsid` + `TIOCSCTTY`; PID 1 tries | ok, `/dev/tty` works / EPERM, stealing too |
| T5 | `^C`, `^Z`, `^\`; a background read; a background `tcsetattr` | signals to the foreground job; SIGTTIN; SIGTTOU stop, or success with it blocked |
| T6 | `SCM_RIGHTS` out of user + mount + PID namespaces | the master arrives and works |
| T7 | a new pty's size; resize; the same size again | 0x0; SIGWINCH; none |
| T8 | the master after the slave's last close; with another holder | data readable, then EIO / no HUP until the holder goes |
| T9 | the master closed | SIGHUP to the session leader; a stopped job stays stopped |
| T10 | `TIOCSTI` | where `legacy_tiocsti=1`, root of a user namespace injects into its own terminal; `=0`: EIO; a pipe: ENOTTY |
| T11 | `PR_SET_DUMPABLE 0` across `setns` | stays 0; the container's root can't open `exe`, `fd` or attach. Dumpable: all three work, even with Yama 1. A credential switch sets `fs.suid_dumpable`, never 1 |
| T12 | `openat2` through `/proc/<pid>/root` | `RESOLVE_IN_ROOT` keeps links and `..` inside; magic links ELOOP with `NO_MAGICLINKS`; a subordinate ID's 0600 file is EACCES from the host |
| T13 | `CLONE_INTO_CGROUP` | EBUSY with controllers in `subtree_control`; a frozen leaf: created, doesn't run |
| T14 | a pidfd in `poll` beside a pipe | readable at the child's exit |

Under x86-64 emulation everything matched, except `openat2` and `clone3` (ENOSYS): exec
falls back to `cgroup.procs`, and resolution to stage 3's walk.

**The monitor's memory** (`scripts/probes/monitor-memory.sh`): a release monitor holding a
`sleep` is 3156 KiB RSS and 2041 KiB PSS, mostly the binary's shared text, and 240 KiB
`Private_Dirty`. The budget test (`the_monitor_stays_small_while_it_logs`) checks its
anonymous memory (`Anonymous` in `smaps_rollup`) against 1 MiB during a 30 MB flood
with no attach clients; it was 472 kB (8.3b). Each of up to 16 slow clients can add
256 KiB.

`scripts/probes/exec.c` (8.4a, test container):

| | Question | Result |
|---|---|---|
| E1 | `clone3(CLONE_PARENT \| CLONE_INTO_CGROUP)` after `setns(user)` and `setns(pid)` | works as root and rootless; PID 2 inside, in the cgroup, the joiner's parent's child. `exit_signal` must be 0 |
| E2 | `NS_GET_USERNS` | tells which namespaces the container's user namespace owns |
| E3 | dumpable 0 across `setns(user)` by the owner | no reset |
| E4 | container root against a non-dumpable process | `exe`, `environ`, `fd` EACCES (`fd` listable for an identity-mapped root: a DAC check) |
| M4 | rootless `clone3` into its own cgroup from a root-owned one | EACCES (the common ancestor's `cgroup.procs`) |

`scripts/probes/pasta-netns.sh` (8.1c): Fedora 44's `pasta_t` can't open an unconfined
process's `/proc/<pid>/ns` or write under `~/.local`, but takes a namespace file in
`$XDG_RUNTIME_DIR`; the host never sees the bind; the parent-death signal survives
`setns(USER)`.

**The FIFO** (8.7b): `start` polls `read()`, which returns 0 once no writer is left. 8.7b
recorded that `poll()` gives no POLLHUP for a writer already there when the reader
opened; a probe in 8.8b (Linux 7.0) showed it does. The kernel holds POLLHUP back only
for a reader opened `O_NONBLOCK` while there was no writer. The read loop is kept: it has
no such exception.

## macOS and Linux differences

- `shipyard-engine`, the attach protocol, the CRI log modules, `process::passed` and the
  terminal primitives are `cfg(unix)` and tested on macOS. Containers, the monitor, exec,
  the joiner and the helper are Linux-only; `container/elsewhere.rs` returns
  `ContainerError::LinuxOnly`.
- `-t` works without isolation on both, with a host pty (`openpty`).
- A hung-up terminal: on Linux its files fail with EIO; on macOS `poll` reports POLLHUP
  (`host_hung_up` reads both).
- Unisolated `-t` jobs in groups of their own: Linux's subreaper ends them with the run;
  on macOS they can outlive it.

## Failure-mode register

| Case | Handling | Test |
|---|---|---|
| A container typing into the user's shell (`TIOCSTI`) or changing its settings | never holds a host terminal: pipes (D16) or a pty of its own | `an_isolated_command_never_holds_the_host_terminal` |
| `exec` without `-t` from a terminal: a process it leaves behind keeps the user's terminal (8.8c, whole-stage review) | `Stdio::sparing_terminals`: relayed pipes, `/dev/null` with `-d` | `exec_from_a_terminal_never_gets_it` |
| No sealed copy (`vm.memfd_noexec=2`): the init, or a rootless `exec` with Shipyard installed in the user's home, runs the binary file the container could write (8.8c) | one rule, `fallback_allowed`: the file only where its owner, a group that may write it, and others are out of the container's reach; else `Error::Unsealed` | `the_binarys_file_runs_only_out_of_the_containers_reach`, `host_ids_inside_and_procs_maps` |
| A caller's `ulimit -f` smaller than Shipyard's binary stops its sealed copy (`EFBIG`), and 8.8c's rule then refused root containers, blaming the memfd policy (found by a mutant that failed for that reason) | the soft limit raised to the hard one for the copy, then put back; a hard limit too small is named in the refusal | `a_hard_file_size_limit_below_the_binary_is_named`, `a_failed_rotation_keeps_the_files_length` (soft limit) |
| A nested user namespace (made by the container's init) or unwritten maps make the joined process's maps look harmless (8.8c's own Murphy review) | the maps are read only for a user namespace whose parent is Shipyard's own (`NS_GET_PARENT`); empty maps are no maps | `host_ids_inside_and_procs_maps` (empty maps); the parent check has no end-to-end test (it needs a nested namespace in a container) |
| A rootless container with a single ID mapped keeps the caller's supplementary groups: a binary one of them may write is in reach (8.8c's Murphy review) | the caller's own groups count as inside | `the_binarys_file_runs_only_out_of_the_containers_reach` |
| A signal between going raw and installing the restoring handlers (8.8c's Murphy review) | the ending signals and SIGTSTP blocked across both | reviewed; no test can hit the window |
| `exec … &` from a terminal, stdin relayed: Shipyard stopped (SIGTTIN), the output with it (8.8c's Murphy review) | stdin `/dev/null` when not in the foreground | reviewed (`in_foreground`); no end-to-end test |
| `logs -f` across a delete and create with the same path, on ext4 or xfs, where the new directory can get the old inode (8.8c's Murphy review) | `LogsOf::instance` (the monitor's PID and start time) compared too | `following_ends_when_the_id_and_path_are_reused` covers tmpfs, where inodes always differ |
| A torn record that can't be cut back (an append-only file) glues the next record to it (8.8c's Murphy review) | ended with a newline | none (needs `chattr +a`, not on tmpfs) |
| `attach` killed (SIGTERM, SIGHUP) with the terminal raw (8.8c) | the restoring handlers in both modes, where the signal is at its default action | `a_killed_attach_puts_the_terminal_back` |
| A full disk (`ENOSPC`) made the writer rotate, dropping the oldest file; a short write cut back to a stale length left a NUL hole (8.8c) | rotate early only on `EFBIG`; finish or cut back by what was written, from the file's own length | `a_full_disk_drops_and_keeps_the_files`, `a_file_truncated_outside_gets_no_hole` |
| A log directory replaced at its path: removed by path (`--pid-file`), read, or followed forever (8.8c) | its identity (dev, inode) recorded at `create`, checked on every use | `failed_logs_are_removed_only_from_the_directory_made`, `logs_refuses_a_directory_put_in_its_place`, `following_ends_when_the_id_and_path_are_reused` |
| `container.log` deleted, then `logs -f` (8.8c) | waits for the next rotation's | `following_waits_for_a_deleted_current_file` |
| `attach` meets the container's end (its monitor gone, or refusing) | the recorded exit code | `attach_exits_as_it_went` (every arm) |
| The user's terminal left raw: `kill -9` of the command, SIGTSTP, a panic | restored on every way out; the SIGTSTP handler restores and re-raws only in the foreground | `the_terminal_is_restored_after_kill_9_of_the_command`, `the_terminal_is_restored_while_shipyard_is_stopped`, `the_terminal_survives_stops_around_the_end_of_runs` |
| A background job holding the slave keeps the run alive | end on the exit status, bounded drain | `a_background_job_holding_the_terminal_doesnt_hang_the_run` |
| `-t >> file` overwrote the file (8.2b) | files and pipes shared, not reopened | `t_appends_to_its_output_file`, `unisolated_t_appends_to_its_output_file` |
| Resuming in the background went raw (8.2b) | raw only in the foreground | `a_raw_run_continued_in_the_background_leaves_the_terminal_alone` |
| A stopped run waiting for `fg` after its container died (8.2b) | the pending pty dropped; the stop comes after reaping | `a_background_run_whose_container_dies_doesnt_wait_for_fg` |
| A hang-up of the user's terminal not seen (8.2b) | ask the terminal | `with_t_hangup_ends_the_input_of_a_command_ignoring_it` |
| A stalled terminal ignoring SIGTERM during the drain (8.2a) | the drain in the supervisor, reacting to signals | `a_stalled_terminal_doesnt_keep_shipyard_after_a_stop_request` |
| `create` holding its caller's stdout until `start` (go-runc's `Create` and `$(…)` deadlock) (8.3a) | the init lets go of it on a console socket | `create_on_a_console_lets_go_of_its_callers_output` |
| Hooks filling the output pipe during `create` (8.3b) | the monitor drains while it waits for the report | `a_chatty_hook_doesnt_hang_create` |
| A rotation failing halfway (8.3b), or cutting the file short (8.8a) | `.container.log.next` first, renames undone; the length kept | `a_rotation_that_cant_happen_moves_nothing`, `a_failed_rotation_keeps_the_files_length` |
| A full disk or `ulimit -f` | SIGXFSZ ignored; a short write truncated back and retried in a new file | `a_failed_rotation_keeps_the_files_length` |
| `logs -f` losing or double-reading at a rotation; forever after a dead monitor (8.3b) | follow by inode; finished when the exit is recorded or the monitor is dead | `follow_goes_on_across_rotations_until_the_end`, `a_record_just_before_a_rotation_is_read` |
| `logs -f ID` following a new container with the same ID (8.8a) | ends if the ID names other logs | `following_ends_when_the_id_is_another_containers` |
| A failed `create` deleting its output, or a swapped path (8.3b, 8.8a) | kept if written; removed through descriptors, only if still the one made | `a_failed_create_keeps_what_was_written`, `a_failed_creates_logs_go_by_descriptor_not_path` |
| `container.log` or the log directory deleted while running (8.8a) | replaced at the next rotation; a gone directory stops writing | `a_deleted_current_file_is_replaced_at_the_next_rotation`, `deleted_logs_stop_being_written` |
| Planted or linked log files | a fresh 0700 directory; `O_EXCL\|O_NOFOLLOW` through its descriptor | `files_are_private_and_never_followed` |
| The monitor's working directory pinning a mount (8.8a) | it runs in `/` | `the_monitor_holds_no_working_directory` |
| A slow or stalled attach client | 256 KiB, then `Dropped`; a leaving client closed after 5 s | `a_stalled_client_doesnt_slow_the_container`, `a_dropped_client_doesnt_keep_the_monitor_busy` |
| Many clients; another user | 16, then refused; `SO_PEERCRED` owner or root | `the_seventeenth_client_is_refused`, `another_user_cant_attach` |
| A hostile client's frames | bounded (32 KiB), hello first; drops that client only | `what_isnt_the_protocol_is_refused`, `any_bytes_are_read_or_refused`; `attach_frames` fuzzing |
| A long state path beyond `sun_path` | connect and bind through `/proc/self/fd/N/attach.sock` | `attach_with_the_longest_id` |
| Typing after the detach keys; a naive matcher (`a,a,b`) (8.3c) | nothing after a match passes on; a KMP-like fallback | `nothing_after_the_detach_keys_is_passed_on`, `the_matcher_finds_the_keys_wherever_they_are` |
| A CPU spin on a leaving client with unread input (8.3c) | polled for POLLOUT only | `a_dropped_client_doesnt_keep_the_monitor_busy` |
| The end race: attach connects as the container ends (8.3c) | ECONNREFUSED reads the exit code | `attach_ends_with_the_containers_exit_code` |
| Ctrl-Q swallowed by the user's terminal's flow control: no detach from a plain container (8.8b) | `IXON`/`IXOFF` off while attached, restored on every way out; a SIGINT/SIGQUIT/SIGHUP/SIGTERM handler restores, then re-raises | `attach_from_a_terminal_to_a_plain_container` (both mutants caught) |
| `exec` of a program the container lacks (8.8b, found writing chapter 34) | the failure's explanation looks in the container's files (`/proc/<init>/root`), not the host's: "command not found" | `exec_of_a_program_the_container_lacks_is_not_found` |
| The container reaching exec before it runs: ptrace, `exe`, `fd`, `environ` | non-dumpable from before the first `setns` until `execve` | `the_container_cant_reach_exec_before_it_runs`, `a_plain_container_cant_reach_exec_before_it_runs` |
| A `/bin/sh` → `/proc/self/exe` image (CVE-2019-5736) | the exec process is a clone of the sealed copy | `exec_never_runs_the_binary_on_disk` |
| Leaked descriptors or working directory (CVE-2016-9962, CVE-2024-21626) | `close_range` after joining; `chdir` after `setns(mnt)` | `exec_holds_no_host_descriptors` |
| A bundle edited after `create` widening exec | the `exec.json` snapshot; narrowing checked | `editing_config_json_after_create_changes_nothing`, `exec_cant_widen_the_container` |
| A reused PID, a replaced cgroup | start time read through `/proc/<init>`; the cgroup's inode checked | `exec_refuses_a_replaced_cgroup` |
| A hostile SIGSTOP or freeze holding the lock (8.4a) | `ExecHeldUp` after 10 s | `a_command_stopped_before_it_runs_fails_exec` |
| Sub-cgroups (EBUSY); an undelegated cgroup | refused with why; the `systemd-run --user --scope` hint | `exec_refuses_a_cgroup_with_sub_cgroups`, `a_rootless_exec_from_an_undelegated_cgroup` |
| A container-controlled `ptmx` opened as a device (8.4b) | `O_PATH`, checked devpts 5:2 first | `exec_t_refuses_a_ptmx_that_isnt_one` |
| A background `exec -t` holding the container's lock (8.4b) | no stop for the terminal while exec is pending | `exec_t_in_the_background_starts_its_command` |
| `cp` through a link the container plants or swaps in (CVE-2018-15664) | inside, `RESOLVE_IN_ROOT\|RESOLVE_NO_MAGICLINKS` | `cp_never_follows_a_containers_link_out_of_it`, `cp_through_a_swapped_link_stays_inside` |
| `cp ID:F /dev/null` as root replaced the host's `/dev/null` (8.5a) | a copy never replaces a device, FIFO or socket | `cp_never_replaces_a_device` |
| A cut-off tar stream reported as success; a failed writer ending its archive (8.5a) | the end marker required; `Stoppable` | `cp_reads_a_stream_to_its_end_and_refuses_a_cut_one` |
| The helper reachable or holding host files | non-dumpable, five capabilities, every other fd closed | `cp_helper_holds_nothing_of_the_hosts`, `a_rootless_cp_goes_both_ways_and_hides_its_helper` |
| Terminal escapes in error messages | control characters escaped | `messages_show_control_characters_escaped` |
| Passed descriptors leaking to the monitor or init, or kept after they're handed on (8.5b) | each hop drops its copies once the next has them | `only_the_containers_process_holds_what_is_passed` |
| Activation meant for another process | `LISTEN_PID` must be the caller's | `socket_activation_for_another_process_is_ignored` |
| A bundle's own `LISTEN_PID=1` kept (8.5b) | replaced by the slot | `activation_comes_before_preserved_and_a_bundles_own_gets_its_pid` |
| Too few descriptors for what's passed | `NofileTooLow`, before start | `a_descriptor_limit_without_room_is_refused` |
| firewalld's rejects unseen; Docker's fix incomplete; a fix counted from the wrong rules (8.6) | rules decoded with direction; fixes matched rule by rule | `firewalld_is_warned_about_until_the_bridge_is_trusted`, `dockers_firewall_is_warned_about_and_its_printed_fix_works` |
| pasta outliving a killed Shipyard under SELinux (8.1c) | a launcher watches pasta and kills it | `pasta_gets_the_namespace_as_a_file_the_host_never_sees_mounted` |
| A mount hidden by a later standard one (8.7b) | refused at load | `a_mount_a_later_one_would_hide_is_refused` |
| A `startContainer` hook's leftover holding `exec.started` (8.7b) | the init's descriptor is close-on-exec; the leftover's is dropped | `a_hooks_leftover_doesnt_hold_poststart_back` |
| Root `create` with a user namespace stuck at its gate (from before stage 8) | `exec.fifo` given to the container's root (`fifo_owner`) | `the_container_cant_reach_exec_before_it_runs` (a root `create` with a user namespace) |

## Fuzzing

Fourteen cargo-fuzz targets, their seeds in `scripts/fuzz-seeds.py`. Stage 8 added five:
`cri_record` (a CRI line), `logs_dir` (a log directory of files), `attach_frames` (the frame
decoder, with a matcher oracle and a bound on held bytes), `process_json` (runc's
`process.json`) and `cp_unpack` (a tar stream into a directory). At their steps:
`cri_record` 12.7 million inputs and `logs_dir` 1.8 million, `attach_frames` 4.39 million,
`process_json` 7.15 million, each in 120 seconds.

- **A fuzz-found fix (8.5a):** `cp_unpack` found a stage-6 bug. `fsat::remove_tree_at`
  couldn't empty a directory its owner could read but not search, so an image layer with
  one couldn't be removed. `open_for_removal` now makes it searchable first; a regression
  test catches the old code.
- **A target fix (8.3b):** `cri_record` fed raw bytes with newlines to `parse_record`,
  which the reader never does; the target now cuts at the first newline.
- **The final run (8.8a):** `test-linux.sh --fuzz 120`, all 14 targets 120 seconds each,
  no crash.

## Known limitations (by design, for stage 8)

- **`kill -9` of Shipyard, or SIGSTOP,** leaves the terminal raw (`reset` fixes it), as
  Docker. Any other signal that ends it puts the terminal back first. The terminal also stays raw while leftovers are killed (up to 2 s) and through
  `poststart` hooks.
- **A container's output reaches the terminal unfiltered,** escape sequences included,
  through `run -it`, `attach` and `logs` (an accepted risk, in `run.md`).
- **Shipyard's own warnings** can overtake a container's buffered output.
- **`logs -f` polls** every 200 ms; lines rotated out between two polls are lost, as for
  any follower. Logs outlive `delete`: the directory is the caller's.
- **Attach shows new output only.** Typing beyond 64 KiB into a slow reader is dropped
  (so detaching always works). A client that can't take its `Dropped` frame is closed
  without it after 5 seconds.
- **No end-of-file through a terminal** for `exec -t` (runc's too); `--stdin-once` with a
  terminal is refused.
- **`exec`:** numeric users only; paused containers and sub-cgroups refused;
  `fs.suid_dumpable=1` hosts refused; a `-d` process goes to the host's subreaper, which
  must reap it; Ctrl-C in its first milliseconds gives 255, not 130.
- **`cp`:** stopped containers refused; a symlinked destination is replaced, not followed;
  sparse files are copied whole; device nodes are never copied out. A container sharing
  the host's PID namespace can signal the helper.
- **Socket activation** needs `LISTEN_PID` to name Shipyard itself (an `sh -c` wrapper's
  is ignored).
- **iptables-legacy rules** are invisible to the firewall checks; a policy-routed uplink
  isn't followed.
- **The monitor's hooks inherit SIGXFSZ ignored.**
- **No sealed copy, no container** where the container could write Shipyard's binary
  (8.8c): on `vm.memfd_noexec=2` hosts, root containers without a user namespace are
  refused, and rootless ones when Shipyard is installed in the user's home.
- **A log directory's identity is its device and inode:** with a state root that
  survives a reboot (not the default `/run`) and logs on a filesystem whose device
  number is given at mount (btrfs, NFS), `logs ID` may refuse them afterwards; `logs
  --log-dir DIR` reads them. After a failed `create --pid-file`, nothing holds the
  directory open: one deleted and made again at its path in that instant could, on
  ext4, get its inode back, and be emptied if nothing was written to it.
- **Log times are wall-clock:** a clock set back makes later lines look older to
  `--since`.
- **A rootless state root under `$XDG_RUNTIME_DIR`** goes when logind removes it at the
  last logout (no lingering): a container then runs on, but `state`, `attach` and
  `delete` can't reach it. (From stage 4; stage 9's state model.) *Settled in stage 9,
  step 9.5c, on Ubuntu 24.04:* with logind's default (`KillUserProcesses=no`) the
  directory isn't removed while a container runs: its monitor keeps the login session
  "closing", and the user with it. With `KillUserProcesses=yes` the session's processes
  end at logout, a monitor in the session's scope among them (linger or not).
- **`poststart`** runs anyway after 5 seconds plus the `startContainer` hooks' timeouts;
  a hook without a timeout can make it run before the program starts.
- **Refused by design** (the conformance gate's `fail` lines): `shared` (and for now
  `unbindable`) propagation, SELinux and AppArmor labels, FIFO devices, deleting with
  runc's semantics, prestart during `create`.

## Carried forward

- **Open from earlier stages:** F24 (an OCI bind source re-resolved at start), F29 (the
  library's subreaper and SIGPIPE, stage 11), F30 (DNS TCP slots, an input chain option,
  stage 9), F31 (stages 7.5 and 7.6's residuals), F10 and F18 (stage 9).
- **Stage 8's findings:** F32, F33 and F35 closed in 8.7b. F34 (joining namespaces by path)
  stage 10; F36 (user mounts an image's symlink leads into `/proc`, `/sys` or `/dev`)
  stage 9; F37 (the cross-step refactors: one passed-fd taker, one inherited-fd list, a
  console variant, a runtime resize watch) stage 9; F38 (logs and attach entry points in
  the engine) stage 11; F39 (a `stopping` state, the monitor's lock give-up, versions on
  every record) stage 9. From the 8.8c review: F40 (attach's frame decoder does
  quadratic work on a flood of small frames from a same-user client, and resizes
  aren't coalesced) stage 9; F41 (the monitor serves no attach client while it waits
  for the lock at the end, up to about 35 s) stage 9.
- **Stage 9:** the commands by name, `run -d`, `run -it` through the monitor, `cp` from a
  stopped container, `HOME` and names for `exec` and `cp -a`; mutants and scheduled
  fuzzing in CI.
- **Later:**
  - an inotify wake-up for the log follower;
  - `cp` following a symlinked destination, sparse files; `cp` storing a symlink's target
    as read (the tar crate normalizes it: a link to `/` is written `//`, which resolves the
    same; GNU tar writes `/`);
  - accepting a parent's `LISTEN_PID` behind a flag;
  - reading iptables-legacy (`IPT_SO_GET_ENTRIES`), policy routing for the uplink, a
    host-netns firewalld test in the Fedora CI job;
  - the joiner started with `execveat` rather than `/proc/self/fd/N`; an unreadable
    `fs.suid_dumpable` taken as allowed; one sealed copy per exec;
  - end-to-end tests for the unsealed refusal (needs `vm.memfd_noexec=2`), attach's
    EMFILE backoff and a terminal resized before its master arrives;
  - `exec`'s failure hints (`/proc/<init>/root/…`) looked up by path, so a link in the
    container is followed on the host (hints only: nothing is opened); `cp`'s helper
    outside the container's cgroup (its cost isn't charged to it);
  - edge hosts for pasta: a chroot, `TMPDIR` under `$HOME` with SELinux, bookworm's pasta;
  - conformance on amd64 once CI runs it (`process_rlimits` is skipped on arm64).

## Tests

Counted at the end of the stage (`#[test]`, or the names in `terminal.rs`'s lists):

| File | Tests | Stage 8's |
|---|---|---|
| `shipyard-cli/tests/runtime.rs` | 169 | 105 in its stage-8 sections (console sockets 9, logs 20, attach 14, exec 23, exec terminals 7, cp 20, fd passing 10, the milestone and exec's budget 2), plus 8.7b's mount-order and `poststart` tests |
| `shipyard-cli/tests/terminal.rs` (`harness = false`) | 72 | 19 in `TESTS`, 53 in `ISOLATED_TESTS` (run as root with `--ignored`, or rootless) |
| `shipyard-cli/tests/rootless.rs` | 22 | 6 on standard input and output (8.2a) |
| `shipyard-cli/tests/run.rs` | 35 | 5 on `-i` and `-t` (8.2a, 8.2b) |
| `shipyard-cli/tests/isolation.rs` | 33 | socket activation (8.5b) |
| `shipyard-cli/tests/firewall.rs` | 17 | the other-firewall tests (8.6) |
| `shipyard-cli/tests/doctor.rs` | 7 | |
| `shipyard-cli/tests/bundle.rs` | 24 | `runc spec` configs and terminals (8.3a) |
| `shipyard-cli/tests/budgets.rs` | 4 | the H7 budgets (8.1b) |
| `shipyard-runtime/src/logs/` | 26 | all |
| `shipyard-runtime/src/attach.rs` | 8 | all |
| `shipyard-runtime/src/container/lifecycle/exec.rs`, `monitor.rs` | 7, 3 | all |
| `shipyard-runtime/src/process/terminal.rs`, `passed.rs` | 4, 3 | all |
| `shipyard-engine` (`streams`, `logs`, `cp`) | 24 | all |
| `shipyard-network/src/firewalls.rs` | 10 | all |

**At 8.8b:** Linux 2647 tests passed across both passes of `test-linux.sh` (root, and a
normal user), macOS 865. The conformance gate: 28 pass, 13 fail for a stated reason, 17
skip, on arm64. Each step's mutants were caught, controls first, or argued equivalent in
its row below.

## Review history

| Step | What | A finding of its own reviews |
|---|---|---|
| 8.1a | the terminal and monitor-memory probes; the engine crate | an unchecked `fork()` result could reach `kill(-1, SIGKILL)` as root (critical); `alarm()` does nothing in a namespace's PID 1 |
| 8.1b | real-host CI (Ubuntu with the profile, a Fedora VM); budgets; differential tests | the set `masquerade` was an nft keyword (`nft -f` refused the table); `add_route` dropped the route's type |
| 8.1c | pasta through a namespace file and a launcher | pasta's exec into `pasta_t` cleared its parent-death signal; tmpfs charges SELinux labels to the inode budget |
| 8.2a | streams: D16, D17, `-i`, the relay | without a root filesystem the host's terminals were reachable by path; the drain ignored signals |
| 8.2b | `-t`: a pty of the container's own | `-t >> file` overwrote the file; resuming in the background went raw |
| 8.3a | OCI terminals, `--console-socket`, `consoleSize` | the init held `create`'s stdout until `start` (deadlocks go-runc) |
| 8.3b | the monitor's event loop, CRI logs, `logs` | a rotation failing halfway; `logs -f` losing a record at rotation |
| 8.3c | attach | the client never said its input ended; a CPU spin on a leaving client |
| 8.4a | `exec`, confined (F6) | a hostile SIGSTOP or freeze holding the lock; an unsealed exec without a user namespace |
| 8.4b | `exec -t`, `-d -t --console-socket` | a container-controlled `ptmx` opened as a device; a background `exec -t` holding the lock |
| 8.5a | `cp` | `cp ID:F /dev/null` replaced the host's `/dev/null`; a failed `tar::Builder` wrote a valid end |
| 8.5b | `--preserve-fds`, socket activation | `create` held three copies of each fd; a bundle's own `LISTEN_PID=1` kept |
| 8.6 | firewalld, Docker, ufw (F14) | "fixed" counted by rules, not matched by direction |
| 8.7a | the conformance gate | hook tests passed vacuously (`1..0` with an error) (critical) |
| 8.7b | mounts in order (F32), `poststart` after `execve` (F35), F33 | a hook's background process could hold the started FIFO; `runc spec --rootless`'s `/sys` refused |
| 8.8a | the milestone test, the whole-stage reviews, fuzzing | a failed `create` removed its log dir by path; a deleted `container.log` written on unseen |
| 8.8b | these notes, the book, the docs sweep, the manual milestone | Ctrl-P Ctrl-Q never detached from a plain container: the terminal's flow control took Ctrl-Q; `exec`'s "not found" looked at the host's files |
| 8.8c | a whole-stage review by four fresh reviewers (security, robustness, test quality, docs), its findings fixed | a rootless `exec` could run the binary file its container could write; `exec` without `-t` passed the user's terminal on; a killed `attach` left the terminal raw; a full disk cost the oldest logs |

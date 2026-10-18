# Stage 10 notes: foundations and the Mac path

The engineering record of stage 10: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [Shipyard on a Mac](../user/macos.md),
[Containers](../user/containers.md) (the boot and stop units) and
[`contrib/systemd`](../../contrib/systemd/README.md).

## Outcome

Stage 10 asked for three things ([ROADMAP](../ROADMAP.md), [FEATURES
§10](../FEATURES.md#10-foundations-and-the-mac-path--stage-10)), decided when the direction
was reset on 2026-10-15 (D18–D31):

1. **A refactor with no behaviour change** (D23), checked against v0.2.0's binary: one
   `take`, one safe-file crate, `doctor`'s checks in the engine, attach's and logs' runtime
   halves in the runtime, `run/detached.rs` split, and stage 9's clean-code list (F48).
   Built as step 10.1.
2. **Stage 9's book chapter.** Moved out: skipped at the user's request (D10-close-4).
3. **A minimal macOS path, used daily** (D24, D29): Virtualization.framework from Rust, an
   Alpine guest running `shipyard`, every command forwarded over vsock, the home shared,
   published ports on the Mac's `localhost`, a VM that stops when idle. Built as step 10.3.

Between them, step 10.2 fixed the F-rows homed in stage 10 on the cleaned code, and the
close (part a) checked the engine's kernel needs inside the guest. From the hardening track
the stage took on H1 (`doctor` and the guest's answers behind the engine's typed API), H2
(every new file and wire format versioned from its first write; F43's rewrite rule), H3 (a
threat model in each plan, the Murphy review run against the plans of 10.2a, 10.3 and
10.3c1), H6 (the hold protocol's refusals as codes, F73) and H7 (`run -d` measured against
v0.2.0, F75; a cold and a warm command through the VM bounded).

It closed F37, F43–F46, F48–F50, F52–F58, F60, F61, F63–F66, F68, F70, F73–F75, F78,
F80, F82–F85 and F91 (F86 was recorded and withdrawn). A user can now:

- on an Apple Silicon Mac (macOS 13 or later), run `shipyard run --rm alpine echo hi`, and
  every other command, as on Linux: the first command boots the VM in about a second;
- `shipyard run -it alpine sh` on the Mac's terminal: raw, at the window's size, Ctrl-C
  reaching the container's job; a third Ctrl-C lets go;
- `shipyard run -d -p 8080:80 nginx:alpine` and open `http://localhost:8080` on the Mac;
- bind a project with `--volume "$PWD":/app` from anywhere under the home;
- leave the Mac alone: after 5 minutes with no command and no container up, the VM stops
  its containers in order and powers off; `shipyard vm status|start|stop` say and do the
  rest;
- run `shipyard doctor` on the Mac: the macOS version, the binary's entitlement, the
  guest, the VM, its ports, the shared home, and, with the VM running, the guest's own
  Linux checks;
- on Linux, have a paused container stop gracefully, a shutdown that lands during boot
  still stop containers (`shipyard-stop.service`), a full disk retried rather than
  turning restart policies off, and `doctor` warn of stop timeouts and stores the boot
  units can't see.

**10.1 changed nothing a user sees.** Each sub-step ran the differential
(`.claude/skills/shipyard-verify/scripts/differential.sh` with `differential-corpus.sh`,
new in 10.1a): v0.2.0 built from its tag, the working tree beside it, about 170 invocations
through both, stdout, stderr, exit codes and the files left behind compared after
normalising IDs, times and ports. At 10.1's close it was identical on macOS (100 cases),
Linux as `tester` (173) and as root (173). The mixed-binary check (root and tester, both
directions: a container started by v0.2.0 renamed, updated, connected and stopped by this
tree, and the reverse) kept every record at version 3, and the starter's `__cleanup`
recorded the end. From 10.2 on, the differential shows only the lines meant to change:
`shipyard-runtime create --help`'s `--hold-fd` (10.2c2), `system stop`'s help, twice
(10.2d1), and the new `vm` command in `--help` (10.3a).

**The Mac milestone** was done by hand on 2026-10-18 (step 10.3c2), with a signed copy
and the default 5-minute limit: `doctor` all ok; `nginx:alpine` serving a folder from the
home (`--volume …:ro`) at `localhost:18500`, an edit on the Mac seen at once; `vm status`
and `doctor` listing the forwarded port; `run -it` on a terminal (`/dev/pts/0`, 24×80);
after `rm -f`, the VM stopped by itself 310 s later ("idle for 300 s: stopping"), and the
next `run` booted it and ran in about a second. It found two everyday Docker habits that
fail (`-v` isn't accepted, only `--volume`; `ps`'s COMMAND is empty for an image's own
command): F101, stage 11. A Mac reboot with the VM's data kept across it is the user's
check.

**The Linux check by hand** (D10-close-3, 2026-10-18): the v0.2 milestone on Lima's
Ubuntu 24.04 arm64 (kernel 6.8.0-147, AppArmor's restriction on, Shipyard's profile), a
static musl build of this tree, both boot units enabled for root and the user (linger
on):
- the chain (`run -d` with a policy, logs, `exec`, `exec -it`, `attach` and detach, `cp`,
  `port`, a restart after `kill -9`, a health check, networks as root): 13/13 rootless,
  14/14 root;
- through a clean reboot (`shipyard-stop` stopped `always` and `unless-stopped`
  containers at the shutdown, `shipyard-recover` started them at the boot): 4/4
  rootless, 4/4 root;
- an upgrade in place: this build removed 32 containers left running for a day under the
  previous build's monitors (10.2d1's), every one cleanly.

| Requirement | Where | Tested by |
|---|---|---|
| The refactor changes nothing users see (D23, D10.1-2) | `.claude/skills/shipyard-verify/scripts/differential.sh`, `differential-corpus.sh` | the differential at every sub-step (identical through 10.1: mac 100, tester 173, root 173 cases); the mixed-binary check; the black-box suites (`cli_reference` pins every `--help`) |
| One "find, lock, read again"; one settle wait; one meaning of "up" (F48 L1, L3, L4) | `run/take.rs` (`take`, `take_or_leave`), `containers/locks.rs` (`lock_again`, `lock_found`, `settled`, `wait_settled`), `containers/state.rs` | `a_container_locked_again_is_read_again_by_its_id`, `a_settled_end_waits_for_the_running_file`, `every_meaning_of_up_is_as_it_was`, `a_reset_to_never_started_clears_start_end_and_exit` |
| One safe-file module, `rustix` beside `nix` (D10.1-4; closes F61's reads) | `crates/shipyard-sys` (`fs`, `lock`, `pidfd`, `fdpass`, `fd`; `relay` since 10.3c1) | `a_symlink_is_refused_not_followed`, `a_fifo_or_a_directory_is_refused_without_blocking`, `a_file_up_to_its_cap_is_read_and_one_byte_more_is_refused`, `a_planted_temporary_name_is_never_written_through_but_passed_over`, `each_durability_syncs_what_it_says`, `a_held_file_given_up_lasts_while_a_copy_is_open`, `a_real_time_signal_is_sent_by_number`, `an_inherited_descriptor_is_taken_as_it_is_or_made_close_on_exec` |
| Records rewritten at the version they were read at (F43, D10.1-1) | `containers/record.rs` (`written_version`, `to_json_at`, `to_json_new`), `containers/store.rs` (`rewrite`) | `a_record_is_written_again_at_the_version_it_was_read_at`, `a_rewrite_keeps_the_version_it_needs`, `a_rewrite_writes_at_the_version_it_read_though_less_is_needed`, `inspect_shows_a_record_as_it_always_has`; the mixed-binary check |
| `doctor`'s checks in the engine, the CLI only prints (H1) | `shipyard-engine/src/doctor/` (`checks`, `Report`, `linux.rs`, `probe.rs`), the CLI's `doctor.rs` | `the_verdict_counts_what_failed_and_what_is_limited`, `off_linux_nothing_is_checked_and_its_said_why`, `a_report_is_laid_out_in_its_columns`; `doctor` byte for byte against v0.2.0 (10.1e1) |
| Attach's and logs' halves in the runtime (D10.1-5; `cp`'s stays: F69) | `shipyard-runtime/src/attach/client.rs`, `logs/reader.rs` | `the_runtimes_attach_and_logs_errors_keep_their_kinds_and_words`, `a_sinks_epipe_is_found_in_either_error`; 18 manual cases against v0.2.0's binaries (10.1e4a) |
| The entry points listed once | `shipyard-engine/src/entry.rs` (`entry_points(Hooks)`) | nine mutants, one per entry dropped, each caught end to end (10.1a); `the_checks_find_what_they_look_for` |
| Modules split by job (F48 L2, L11, L13; F74) | `run/` (`take`, `create`, `start`, `foreground`, `files`, `remove`, `request`, `validate`, `plan`, `finish`, `prepare`), `containers/` (`store`, `lookup`, `locks`, `starts`), the CLI's modules and `run/`, `events/`, `image/index/`, `nft/ruleset/`, `lifecycle/attach/` | test names the same set before and after each split; `only_the_engines_tests_write_records_directly` |
| Fail closed, and what a local user can do (F65, F53, F60, F70) | `shipyard-network/src/store.rs` (`RoutingRecords`), `network/firewall.rs`, `ports.rs` (`hold`), `image::prepare_owned`, the CLI's `pipe.rs` | `routing_closed_fails_closed_on_a_record_it_cant_read`, `an_unread_routing_record_closes_the_forwarding_on_now`, `the_reservation_lock_is_the_callers_own`, `a_held_area_refuses_a_view_and_a_start`, `logs_end_as_their_readers_do_by_name`, `runtime_logs_end_as_their_readers_do` |
| State and crash consistency (F55, F64, F66, F56, F57, F44, F45) | `containers/starts.rs` (`settle_unstarted`, `previous-start.json` v2), `containers/store.rs` (`with_records_locked`), `run/connect.rs`, `paths.rs` (`store_overrides`), `run/cleanup.rs` (`retry_end`) | `a_failed_start_keeps_what_its_user_stopped`, `a_stop_of_a_dead_start_stays_stopped`, `running_is_let_go_at_once_unless_given_up`, `an_unreadable_record_keeps_the_volumes`, `prune_waits_for_the_container_store`, `a_cut_short_connect_is_finished_by_its_rerun`, `stores_the_boot_unit_wont_see_are_a_warning`, `an_end_the_store_refused_is_retried` |
| The runtime's descriptors and protocols (F54, F49, F73, F58) | `shipyard_sys::fdpass::receive`, the monitor's `exit_command_limit` and `set_descriptor_limit`, protocol v4 (`HoldRefusal`, `Frame::NotDone`), the DNS server's `serve_frames` | `a_message_cut_short_at_the_limit_leaves_nothing_open`, `holds_go_past_the_callers_limit_and_its_helpers_keep_it`, `hold_refusals_keep_their_numbers`, `a_refusal_says_why_from_either_version`, `holds_are_checked`, `a_connection_is_closed_at_its_lifetime` |
| The host's shutdown and small fixes (F46, F52, F50, F61) | `Cgroup::thaw` in the monitor's stop, `contrib/systemd/shipyard-stop.service` (and `user/`), `doctor`'s `stop_timeout_check`, `pasta::Features::cached`, `paths.rs` (`absolute`, `dns_env_of`) | `a_paused_container_stops_gracefully`, `the_boot_units_say_what_the_design_needs`, `a_stop_timeout_past_the_units_is_a_warning`, `what_pasta_can_do_is_asked_once_per_binary`, `a_relative_store_is_refused`, `a_checks_late_output_is_kept`, `a_monitor_has_joined_when_its_cgroup_lists_it`; four reboots on Lima's Ubuntu 24.04 (10.2d1) |
| Flakes with causes, and investigations (F63, F83, F84, F85, F68, F75) | `containers::create_record_avoiding`, `run/files.rs` (`never_joined`), `attach::client::Window`, the monitor's attach `handle` | `made_up_names_avoid_the_names_held_elsewhere`, `the_networks_a_plan_joins_are_named`, `a_client_too_far_behind_is_dropped_with_a_frame`, `a_window_size_is_sent_once_per_change`, `a_size_is_set_before_the_typing_after_it`; loops of 50 under load |
| The rows left after the split (F78, F80, F82, F37, F74) | `LogsOptions::reader_gone`, `cleanup.rs`'s `record_end`, `lifecycle.rs`'s `after_failure`, `process::ResizeWatch`, `child.rs`'s `Gaps`/`Merged`/`close_all_but`, `terminal::Size` | `following_ends_when_its_reader_goes`, `logs_follow_ends_when_its_reader_goes`, `a_follower_waiting_for_its_logs_ends_when_its_reader_goes`, `a_failed_start_keeps_what_its_last_start_left`, `a_monitor_that_outlives_its_failure_is_ended`, `a_resize_watch_catches_resizes_and_nothing_else`, `two_kept_lists_merge_and_repeats_count_once` |
| Virtualization.framework from Rust, in a VM host that starts on first use (D29) | `crates/shipyard-vm` (`machine.rs`, `host.rs`, `client.rs`, `paths.rs`, `peer.rs`), `contrib/macos/shipyard.entitlements`, `scripts/macos/sign.sh` | `the_guest_boots_runs_a_container_and_keeps_its_data`, `two_first_commands_start_one_vm`, `only_the_same_user_is_let_in`, `the_directory_must_be_the_users_own` |
| A minimal Alpine guest, built reproducibly (D10.3-2) | `scripts/guest/build.sh`, `alpine.lock`, `build-inside.sh`, `assemble.py`; `shipyard-vm/src/manifest.rs` (`Guest::find`, `verify`) | `a_manifest_of_version_1_is_read_and_a_newer_refused`, `a_guest_needs_its_manifest_and_its_own_files` |
| The guest's PID 1, its data disk and its agent | `shipyard-engine/src/guest/` (`init.rs`, `disk.rs`, `agent.rs`, `mod.rs`) | `a_disk_is_formatted_only_when_empty`, `a_disk_is_read_by_its_superblock`, `a_checked_disk_mounts_only_when_fixed`, `only_the_host_is_served`, `a_signal_is_128_plus_its_number` |
| Commands forwarded over vsock, stdio and signals (D10.3-5) | the CLI's `forward.rs` (`place`, `needs`, `within_home`, `guest_cwd`) and `run/forwarded.rs`, the runtime's `attach/forward.rs` (`Request` v2, `Frame::Signal`), `shipyard-vm/src/client.rs` | `commands_are_forwarded_as_on_linux`, `variables_and_input_cross_as_named`, `commands_go_where_they_mean_something`, `signals_go_by_their_linux_numbers`, `a_request_round_trips_and_its_limits_hold` |
| The terminal through the VM (D10.3-7) | the agent's pseudo-terminal (`host_pty`), `Welcomed::session_signalled`, the client's `Leaving` | `run_it_is_a_terminal_in_the_guest`, `a_ctrl_c_without_input_is_passed_on_as_a_signal`, `a_third_interrupt_lets_go_and_the_guest_hangs_up`, `a_hangup_on_the_mac_ends_the_command_in_the_guest`, `the_detach_keys_detach_in_the_guest`, `a_plain_command_on_a_terminal_gets_one` (`vm_terminal.rs`) |
| The project directory shared (virtiofs; the home, D10.3-3) | `host.rs` (the share, `shipyard.home=`), `guest/init.rs` (the mount, the hidden places) | `only_paths_in_the_home_are_forwarded`, `the_directory_is_the_callers_under_the_home`, `only_places_in_the_home_are_hidden`, `the_vms_own_files_are_hidden_from_a_home_bind` |
| Published ports on `localhost` (D10.3-10) | `shipyard-vm/src/forwarding.rs` (`bind_alone`, `Slots`), `ports.rs` (`wanted`), `containers::published`, `__guest-ports` | `published_ports_answer_on_the_macs_loopback`, `a_port_in_use_on_the_mac_is_said_and_left_alone`, `a_port_in_use_on_any_address_is_busy`, `a_port_in_time_wait_is_bound_again`, `ports_are_listened_on_the_macs_loopback_only` |
| The VM stops when idle (D20, D10.3-11) | `shipyard-vm/src/idle.rs` (`Clients`, `Idle::after`, `idle_limit`), the client's single retry | `an_idle_vm_stops_by_itself_and_starts_again`, `a_running_container_keeps_the_vm_up`, `commands_racing_the_idle_stop_all_run`, `a_command_sent_into_a_stop_runs_after_it`, `quiet_for_the_limit_stops_and_lets_no_one_in` |
| `doctor` on macOS | `shipyard-vm/src/doctor.rs`, the CLI's `doctor.rs` | `doctor_checks_the_vm`, `the_macos_floor_is_13_on_apple_silicon`, `the_entitlement_is_read_from_codesigns_xml`, `a_guest_is_needed_and_this_builds_is_best` |
| The engine's kernel needs checked in the guest (D10-close-1) | `__guest-doctor` (`guest::doctor_entry_point`), `doctor::GuestDoctor` v1, `shipyard_vm::client::ask` | `doctor_checks_the_vm` (cgroups, images, network, firewall all `ok`), `the_guests_checks_are_versioned`, `the_guests_checks_are_shown_and_counted`, `asking_never_starts_the_vm`; overlayfs, nf_tables, virtiofs and vsock exercised by the other VM tests ([Kernel checks](#kernel-checks)) |
| The native macOS path stays tested (D10.3-8) | `SHIPYARD_VM=off`, set by `.cargo/config.toml` for cargo's runs | `the_vm_is_turned_off_only_when_asked`; the macOS suite; the differential's Mac pass |
| Budgets (H7) | `tests/vm.rs`; F75 measured by hand | a warm `run --rm` through the VM under 10 s (`commands_are_forwarded_as_on_linux`); a cold `ps` after an idle stop under 10 s (1.05 s measured); `run -d` release medians against v0.2.0: root 33.8 → 32.4 ms, rootless 50.9 → 49.5 ms (10.2e2) |

**Moved out of stage 10:**

| Item | Now in | Why |
|---|---|---|
| Stage 9's book chapter (D22) | not planned | D10-close-4: skipped at the user's request |
| The record's split from `Config` and the API's types | stage 11 | D10.1d-1: `inspect` becomes Docker's there; 10.1d did F43's rule |
| `cp`'s runtime half; the `shipyard-runtime` binary without the engine | stage 13 (F69) | D10.1e-2: it would pull `shipyard-image` and `tar` into the runtime crate |
| F34 (joining namespaces by path), F62 (a default pids limit, `--isolate-host` for `bridge`) | stage 11 | D10.2-2: Compose needs F34; F62's defaults are decided beside Docker's |
| F59 (an identity of Shipyard's own instead of `nobody`) | stage 13 | D10.2-2: packaging can create a system user |
| F79, F87 (two flakes) | stage 15 | not reproduced in 50 runs under load each (10.2e2) |
| The 25 long modules F74 didn't name (F89) | stage 15 | D10.2-20: split each when it's next changed |
| F71 (one resize watch per process) | stage 11 | D10.2-21: the CLI never runs two sessions; the API may |
| The kernel probes (`scripts/probes/`) run in the guest | replaced | D10-close-1: the engine's own `doctor` checks run in the guest, the same as on a Linux host |

## Architecture

### The crates

```
shipyard-cli ──▶ shipyard-engine ──▶ shipyard-runtime, -network, -image, -storage
     │                 │                         │
     └──▶ shipyard-vm ─┴──▶ shipyard-runtime ────┴──▶ shipyard-sys (leaf: nix, libc, rustix)
```

- **`shipyard-sys`** (new, 10.1c): the plumbing every crate shares. `fs`
  (`write_atomically{,_at}` with a `Durability` of `Synced`, `FileSynced` or `Atomic`;
  `read_bounded{,_at,_from}`: no symlink, no FIFO wait, a regular file, a cap; lock-file
  opens), `lock` (`lock_until`, scoped, a nix `Flock`; `try_lock_until` and `HeldFile`,
  handed over and given up; `byte_range`), `pidfd` (Linux: rustix opens, one raw
  `pidfd_send_signal`), `fdpass` (`SCM_RIGHTS`; Linux receives through rustix's
  `recvmsg`, F54), `fd` (`take_inherited`), and `relay::both_ways` (10.3c1).
- **`shipyard-vm`** (new, 10.3a): macOS-only but for its places, manifest and checks.
  `machine.rs` (the configuration, every Virtualization.framework call on the VM's serial
  queue), `host.rs` (the `__shipyard-vm` process), `forwarding.rs` and `ports.rs` (ports),
  `idle.rs`, `client.rs` (the CLI's side), `doctor.rs`, `manifest.rs`, `paths.rs`,
  `peer.rs` (`getpeereid`).
- **`shipyard-engine`** gained `doctor/` (10.1e1) and `guest/` (10.3a: PID 1, the agent,
  the data disk, `__guest-ports`, `__guest-doctor`), and was split by job.
- **`shipyard-cli`** is split (10.1e3): `cli.rs`, `output.rs`, `reporter.rs`, a module per
  command group, `run/`, `forward.rs` (10.3b1), `vm.rs`; `lib.rs` holds what both
  binaries share (`limits`, `logs_output`, `pipe`).

### The Mac path

```
shipyard (Mac CLI)  place(): vm, doctor, --help on the Mac; system recover|stop refused;
   │                everything else forwarded (SHIPYARD_VM=off: run here, no VM)
   │ vm.sock (Unix, 0600 in a 0700 directory, getpeereid = the user)
   ▼
__shipyard-vm (the VM host: one per user, alive while it holds vm.lock)
   ├─ the VM, on its serial queue: 4 GiB, half the CPUs (at most 4)
   │    data.img (64 GiB sparse, SynchronizationMode::Full), virtiofs "home",
   │    vsock, NAT, entropy, console.log
   ├─ per client: a thread; vsock connect(1024); HelloRun + Request; relay both ways
   ├─ the poller: __guest-ports every 2 s and at each command's end
   │    → listeners on 127.0.0.1:P and [::1]:P (bind_alone, no SO_REUSEADDR)
   │    → each accepted connection: vsock __guest-forward IP PORT, then raw bytes
   └─ idle: Clients (count, stopping) + Idle::after → SIGTERM to itself
        → __guest-stop → requestStop, then stop after a bound
                     │ vsock (the host is CID 2)
                     ▼
guest: Alpine 3.24.2, linux-virt 6.18.57; the initramfs root copied to a tmpfs
   PID 1 = shipyard's guest init: mounts, modprobe, eth0 + DHCP, the data disk
     (e2fsck -p, or mkfs on an empty one) on /var/lib/shipyard, the home over
     virtiofs at its own path (the VM's own files there covered), the agent,
     `system recover`; on SIGTERM: sync, `system stop`, kill, reap, unmount, power off
   └─ __shipyard-agent (vsock 1024, CID 2 only, a process group of its own)
        └─ per connection: `shipyard ARGS` on pipes, or on a pseudo-terminal;
           __guest-ports and __guest-doctor as processes of their own
```

A command's round trip: the CLI parses its flags (clap refuses mistakes on the Mac),
checks every host path it names is under the canonical home, picks the variables it may
send (`TERM`, `LANG`, `LC_*`, `NO_COLOR`, and bare `-e KEY`s, never a reserved name),
starts the host if `vm.lock` is free, and sends a `HelloRun` with `forward::Request` v2.
The agent runs `shipyard ARGS` in the same directory and sends `Welcome`, the output
frames and `Exited`. The exit code becomes the CLI's (`u8::try_from(code).unwrap_or(125)`).
Signals travel as `Frame::Signal` in Linux numbers.

### Files and formats

| File or wire | Version | Written by | Notes |
|---|---|---|---|
| `record.json` | 3 (1 and 2 read) | the engine | rewritten at the version read unless a field set needs a newer one (F43, D10.1-1); a field the engine sets on a running container goes in a file of its own (F67's constraint) |
| `previous-start.json` | 2 (1 read, newer passed over) | a start, before it rewrites the record | the start, end, exit, boot, a user's stop, restart count, backoff, next start, ports (F55); bound 1 MiB |
| `pasta-features.json` | 1 | the first run to ask pasta | keyed by path, device, inode, size, mtime and ctime (F50) |
| `port-reservations.lock` | — | `ports::hold` | in the caller's runtime directory (F53); the abstract name only where none exists (F77) |
| The monitor's protocol | 4 | the monitor and its clients | `NotDone { reason, why }` (kind 24) to holders that agreed 4 or more; a v3 holder gets `Done { ok: false }` with the same words (F73). `HelloRun` (kind 11) and `Signal` (kind 12) are a client role only the guest's agent accepts |
| `manifest.json` (the guest) | 1 | `scripts/guest/assemble.py` | Alpine, the packages, the kernel, each file's sha256, the build's revision; written last |
| `forward::Request` | 2 (1 read) | the Mac CLI, the host | argv, cwd, named variables, input, terminal, time (10.3b1) |
| `PortsAnswer` | 1 | `__guest-ports` | active count, TCP ports, UDP count; read up to 8 MiB (`MAX_PORTS_ANSWER`) |
| `ports.json` (the Mac) | 1 | the host | forwarded, unbound with why, UDP count, the idle limit; removed when a host starts and ends |
| `GuestDoctor` | 1 | `__guest-doctor` | the kernel and the engine's checks |
| `data.img` / `data.img.new` | — | the host | a new disk is `data.img.new` until a guest has booted with it; excluded from Time Machine |

### Locks

The order of stage 9 holds (AGENTS §4). Two changes: `volume rm` and `prune` check users
and trash under the container store's `.lock` (F56), so volumes sit at 5 and `.lock` at 6;
a start's `running` lock is a `HeldFile` from `take` until the monitor holds it (F66).
On the Mac, `vm.lock` is the host's liveness; a waiter for a stop waits for *that* host's
PID to be gone, never for "stopped" (a new host may start at once).

## Decisions and why

**The direction (2026-10-15, D18–D31):** two users, macOS developers and Linux hosts, the
Linux host's needs first (D18); the product decides what's built, learning decides how,
and the owned surface is frozen (D19); "daemonless" means nothing runs while Shipyard
isn't used (D20); no release before v1.0 (D21); the refactor first (D23), then a Mac path
used daily, not a spike (D24); Docker's API, CLI, Compose and BuildKit for stages 11–12
(D25–D28); Virtualization.framework through `objc2-virtualization`, one shared Alpine VM,
Apple Silicon only (D29); CI off until before v1.0 (D31).

**10.1, the refactor (2026-10-15 and 16, asked):**

- **D10.1-1, F43's rule:** a record is rewritten at the version it was read at unless a
  field set in it needs a newer one. A v0.2.0 monitor still running must still read what
  a newer CLI rewrote. D10.1d-2 (after the Murphy review): F43 closes with the rule and a
  documented constraint; the residue is F67.
- **D10.1-2, strict "no behaviour change":** every message, exit code, `--help`,
  `inspect` JSON and record byte the same, proved by the differential. Durability, error
  kinds no caller reads, types and module paths could change.
- **D10.1-3:** the F-rows homed in stage 10 go to 10.2, on the cleaned code.
- **D10.1-4, `shipyard-sys`:** the copies (8 atomic writes, 7 lock loops, 3 pidfd copies,
  about 9 unbounded reads) become one leaf crate. A file over its cap is refused
  everywhere (five reads that truncated stopped truncating).
- **D10.1c-3, durability by file:** fsyncs on the runtime's files cost about 15 ms per
  `run -d` on the test container's overlayfs `/run`, and a reboot empties `/run` anyway.
  So `Durability::Atomic` (temporary and rename, no sync) for the runtime's files, as
  v0.2.0; `Synced` for the engine's records.
- **D10.1c-4, pidfds:** rustix opens them; sending stays one raw
  `SYS_pidfd_send_signal`, because rustix's `Signal` can't be 0 and its contract forbids
  sending libc's reserved real-time signals, both of which Shipyard sends.
- **D10.1d-1:** the record's split from `Config` moves to stage 11.
- **D10.1-5, D10.1e-2:** attach's and logs' by-runtime-ID halves move into the runtime;
  `cp`'s stays in the engine (F69).
- **D10.1e-7, D10.1e-8 (AGENTS §5):** exit codes are `u8` where a process's status
  becomes the CLI's, `i32` where a protocol or record carries one, and one way turns an
  `i32` into a `u8` (125 when it doesn't fit, never `as`); an item that exists everywhere
  but only Linux calls carries a reasoned `cfg_attr` allow rather than a `cfg`.

**10.2, the F-rows (2026-10-16 and 17, asked):**

- **D10.2-1:** five themed sub-steps (a fail closed, b state, c runtime and protocols,
  d host integration, e flakes), each with its own reviews, mutants and differential.
  D10.2-19 added f for the five rows left after the split.
- **D10.2-2:** F34 and F62 to stage 11, F59 to 13: each is bigger than a fix and changes
  what users see.
- **D10.2-3, F65:** an unreadable `.routing-closed` closes routing between the host's
  current interfaces and warns, rather than refusing the run: refusing left a flushed
  host routing for anyone. D10.2-5: that fallback leaves bridges and their ports out, so
  Docker's, libvirt's and CNI's keep working.
- **D10.2-4, F60:** closed with a test, no new lock: both paths that mount an overlay
  already hold the area's exclusive lock.
- **D10.2-12, F49:** the monitor raises its own descriptor limit; the companion gets the
  caller's back; the exit command gets room for its holds. The raised limit is what lets
  a container that connected past the caller's limit restart.
- **D10.2-20, D10.2-21:** F74 splits only the row's named modules (the rest F89); F71
  stays for stage 11.

**10.3, the Mac path (2026-10-17 and 18, asked):**

- **D10.3-1, D10.3-5, D10.3-9:** built in five parts: the guest and the host; forwarding
  without a terminal; terminals; ports; idle and `doctor`.
- **D10.3-2, the guest's root:** an immutable initramfs rebuilt per version, copied to a
  tmpfs at boot (`pivot_root` refuses the initramfs root); only `/var/lib/shipyard` is on
  a persistent disk, formatted on the first boot. An upgrade replaces two files.
- **D10.3-3, sharing:** the whole home at the same path, read-write: Virtualization.framework
  fixes shares at boot, so one share serves every project, and paths need no translation.
  Its cost: the guest can read and write the home, as Docker Desktop's sharing can. The
  close hides the VM's own files from it.
- **D10.3-4:** macOS 13 Ventura is the floor (what Rosetta in Virtualization.framework
  needs, so stage 13 won't move it).
- **D10.3-6, skew:** a wire the guest doesn't speak is refused ("rebuild the guest");
  another build of the same wire warns once per VM start.
- **D10.3-7:** a pseudo-terminal in the guest when the Mac's stdin and stdout are both
  terminals; pipes otherwise.
- **D10.3-8:** `SHIPYARD_VM=off` runs a Mac's command on the Mac itself. Found when the
  first full verification after 10.3b1 failed 68 macOS tests: the native path's tests
  had no guest. Cargo's runs set it; the VM tests unset it.
- **D10.3-10:** ports on the Mac's loopback only: nothing on the Mac's network reaches a
  container. Exposing one is a later opt-in.
- **D10.3-11:** idle is 5 minutes with no client and no container running, paused,
  stopping or waiting to restart; `SHIPYARD_VM_IDLE` overrides it. A guest that can't
  answer is never idle-stopped.
- **Kept, with why (10.3a's reviews):** modules loaded by `modprobe` rather than a load
  order in the manifest (the kernel loads on demand through the same `modprobe`); a state
  poll rather than a delegate class for the VM's end.

**The close (2026-10-18, asked):**

- **D10-close-1:** a Mac's `doctor` shows the guest's own checks when the VM runs, and
  never starts it to check.
- **D10-close-2:** the close in two parts: a (the gap, three whole-stage reviews, fixes),
  b (these notes, the docs sweep, fuzzing, verification, the Linux check by hand).
- **D10-close-3:** the Linux check by hand is the v0.2 milestone on Lima's Ubuntu 24.04
  arm64, rootless and root, through a reboot.
- **D10-close-4:** stage 9's book chapter is moved out.

## Kernel checks

**The guest's own checks** (D10-close-1): `shipyard doctor` on the Mac, with the VM
running, runs the engine's `doctor::checks` in the guest (`__guest-doctor`, the same
binary and the same probe helper as on a Linux host). On Alpine 3.24.2, kernel
`6.18.57-0-virt` (`linux-virt=6.18.57-r0`), every line is `ok`:

| Check | What it found | Pinned by |
|---|---|---|
| `cgroups` | cgroup v2 with the controllers the engine uses | `doctor_checks_the_vm` |
| `images` | overlayfs mounts where the image store is | `doctor_checks_the_vm` |
| `network` | bridge networks, with nf_tables | `doctor_checks_the_vm` |
| `firewall` | no other firewall in the way | `doctor_checks_the_vm` |
| `runtime state` | the runtime's state root | the same run (by hand) |
| mounts | a mount namespace and a mount in it (the probe; a line only if refused) | the absence of `fail` in `doctor_checks_the_vm` |

The guest runs commands as root, so `user namespaces` (a rootless line) isn't checked
there. It has no systemd, so a host's boot, linger and logout lines don't appear: no
filter was needed.

**Exercised by the VM tests:** overlayfs by every image run
(`the_guest_boots_runs_a_container_and_keeps_its_data`); nf_tables DNAT by every
forwarded port (`published_ports_answer_on_the_macs_loopback`: the agent dials the guest's
`127.0.0.1`, which DNAT reaches); virtiofs by binds from the home
(`commands_are_forwarded_as_on_linux`, `the_vms_own_files_are_hidden_from_a_home_bind`);
vsock by every command.

**Checked by hand while building (10.3a's spike, macOS 27 on Apple Silicon):**

| Question | Result |
|---|---|
| Can a Rust program boot Alpine's kernel with `objc2-virtualization` 0.3.2? | yes, with the raw `Image` unpacked from the zboot `vmlinuz-virt` (`ARMd` checked); the kernel is up in about 0.1 s |
| Does an unsigned process get anywhere? | no: even `validateWithError` refuses without `com.apple.security.virtualization`; test binaries are signed |
| Does the guest need its time on the command line? | no: the VM has a PL031 RTC and the kernel sets its clock from it. The skew after a Mac's sleep remains: each request carries the Mac's time, and the agent sets the clock when it's 2 s behind or 30 s ahead |
| Can the netboot files be pinned? | no per-file checksums; the guest is built from the minirootfs (published `.sha256`) with apk installing exact versions, signatures checked |
| `pivot_root` on the initramfs root | refused: the root is copied into a tmpfs, moved onto `/`, and the init re-executes |
| `panic=-1` | Virtualization.framework restarts the VM in place (four boots in 25 s): a panic isn't seen by the host (F90) |
| udhcpc on a link that's down | loops: the init brings `eth0` up first |

**The disk:** a boot to the agent in about 1 s; `run --rm alpine:3.20` with its pull in
about 5.5 s; an ordered stop in about 2.5 s; images kept across boots.

**On Lima's Ubuntu 24.04 arm64** (10.2d1, a static musl build, four reboots):

| Question | Result |
|---|---|
| A paused container's `stop -t 60` | 1 s, exit 7 (the thaw); 60 s before F46 |
| A plain reboot with both units | the recover unit stopped first, then the stop unit's `system stop` stopped the container (exit 7); back after the boot |
| A reboot while the recover unit is still activating (31 containers up) | systemd ended its control process, then the stop unit stopped all 31; all 126 `die` events of the hour exit 7 |
| A oneshot unit with no `ExecStart` | valid, active at once |
| The user manager's own stop timeout | **5 s on Ubuntu 24.04** (`user@.service.d/timeout.conf`), not upstream's 120 s: `doctor` compares against it |

**In the test container:** `recvmsg` at `EMFILE` installs some descriptors, sets
`MSG_CTRUNC` and still reports them; rustix's walk yields them, nix's `cmsgs()` doesn't
(F54). A monitor under a soft limit of 128 with 300 holds: the monitor at 2,417, its init
and companion at 128, the exit command at 367 (F49).

## macOS and Linux differences

- **What runs where on a Mac:** `vm`, `doctor`, `--help` and `--version` on the Mac;
  `system recover` and `system stop` are refused (the guest's PID 1 runs them at its start
  and stop); everything else in the guest. `SHIPYARD_VM=off` runs a command on the Mac
  itself, without isolation or images, as before stage 10: the macOS test suite and the
  differential's Mac pass use it.
- **Signal numbers differ above 15.** macOS's SIGUSR1 is 30, Linux's 10. Signals cross
  the wire in Linux numbers (`linux_number`: HUP, INT, QUIT, USR1, USR2, ALRM, TERM,
  WINCH); SIGINFO, with no Linux twin, is dropped.
- **No `SOCK_CLOEXEC` on macOS** (nor `O_CLOEXEC` for a pseudo-terminal): close-on-exec
  is set by `fcntl` right after. On Linux, `host_pty` opens both ends `O_CLOEXEC` from
  the start, so a command another connection spawns never inherits them.
- **`SO_REUSEADDR` shadows on BSD:** with it, a bind of `127.0.0.1:P` succeeds beside
  another program's `0.0.0.0:P` and takes its local connections. The host's listeners bind
  without it, so another program's port is "busy", never shadowed. The other order isn't
  refused by the Mac: a program started later with `SO_REUSEADDR` on every address is
  shadowed by the VM's listener (F98).
- **`TIME_WAIT` without `SO_REUSEADDR`:** a port held only by a closed connection can't be
  bound for about 30 s. `bind_alone` connects to tell the two apart: refused means
  `TIME_WAIT`, and it binds again with the option.
- **Accepted sockets inherit their listener's `O_NONBLOCK` on macOS** (Linux's `accept4`
  doesn't): the host sets them blocking before relaying.
- **`setsockopt(SO_RCVTIMEO)` on a Unix socket whose peer has closed fails with `EINVAL`
  on macOS** (Linux accepts it): `client::ask` doesn't fail on it, since the read ends at
  once anyway.
- **`sysctl` instead of `/proc`:** the macOS version comes from
  `sysctlbyname("kern.osproductversion")` (raw `libc`: nix has none).
- **Entitlements:** a Mac binary that starts a VM must be signed with
  `com.apple.security.virtualization` (`scripts/macos/sign.sh`; cargo's runner on a Mac,
  `scripts/macos/sign-and-run.sh`, signs each test binary). `doctor` checks the binary
  with `/usr/bin/codesign`, bounded at 5 s.
- **Linux only:** `shipyard-sys`'s `pidfd` and rustix's `recvmsg` (macOS keeps nix's);
  the guest's init, agent and disk; `Cgroup::thaw`; the boot and stop units. The
  `shipyard-vm` crate's VM, host and client are macOS only; its places, manifest, ports
  and idle rule build and are tested everywhere.

## Failure-mode register

| Case | Handling | Test |
|---|---|---|
| A v0.2.0 monitor still running after an upgrade, its record rewritten by a newer CLI (F43) | rewritten at the version read; a version with no writer refused, never mislabelled | `a_record_is_written_again_at_the_version_it_was_read_at`, `a_rewrite_keeps_the_version_it_needs`, `a_rewrite_writes_at_the_version_it_read_though_less_is_needed`; the mixed-binary check |
| A state file replaced by a symlink, a FIFO, or made huge by another user | refused by `read_bounded` (`O_NOFOLLOW`, `O_NONBLOCK`, regular files only, a cap checked by `fstat` and after reading) | `a_symlink_is_refused_not_followed`, `a_fifo_or_a_directory_is_refused_without_blocking`, `a_file_already_too_large_is_refused_unread`, `a_record_over_its_bound_is_refused_unread` |
| A temporary name planted before an atomic write | `O_EXCL`, passed over, up to 16 names; only a temporary this write made is unlinked | `a_planted_temporary_name_is_never_written_through_but_passed_over`, `a_symlink_planted_at_the_temporary_name_is_not_followed`, `a_write_gives_up_once_every_name_it_tries_is_taken` |
| `.routing-closed` past a 4 KiB line cap on a host of many interfaces (10.1c1's Murphy review, high): the firewall rebuilt with the host routing for anyone | its own 1 MiB bound | `routing_closed_on_a_host_of_many_interfaces_reads` (300 interfaces) |
| `.routing-closed` unreadable (a symlink, a directory, past its bound, not text) or this boot unknown (F65) | read as "unread", never "not closed": routing closed between the host's own links for each family forwarding now, bridges left out, and a warning | `routing_closed_fails_closed_on_a_record_it_cant_read`, `an_unread_routing_record_closes_the_forwarding_on_now` |
| Another local user holding the port-reservation lock (F53) | the lock is a file in the caller's 0700 runtime directory | `the_reservation_lock_is_the_callers_own` |
| A stopped container's `cp` view and a start mounting one upper directory (F60) | the area's exclusive lock, taken by both | `a_held_area_refuses_a_view_and_a_start` |
| `logs 2>&1 \| head` sees `EPIPE` on stderr; stderr closed while stdout is open (F70) | quiet only when `poll` says stdout's reader is gone, whichever stream failed | `only_stdouts_gone_reader_is_quiet`, `logs_end_as_their_readers_do_by_name`, `runtime_logs_end_as_their_readers_do` |
| A start killed after rewriting the record (F55); a user's `stop` of that dead start | `previous-start.json` v2 puts back all the start rewrote; a stop made since wins | `a_failed_start_keeps_what_its_user_stopped`, `a_stop_of_a_dead_start_stays_stopped`, `a_previous_start_puts_back_all_a_start_rewrote`, `a_version_1_previous_start_is_read` |
| A start that never ran, settled three ways (F64) | one `settle_unstarted` on every path: what the last start left, or never started | `a_reset_to_never_started_clears_start_end_and_exit`, `a_failed_start_keeps_what_its_last_start_left` |
| A failed start's `running` lock closed, not unlocked, while a copy was open (F66) | a `HeldFile`: unlocked on every failure, given up only once the monitor has it | `running_is_let_go_at_once_unless_given_up`, `running_is_free_only_once_its_end_is_settled` |
| `volume rm`/`prune` beside a create, an unreadable record, or a container store whose place isn't known (close-a, data loss: stopped containers' volumes deleted) | users checked and trashed under the store's `.lock`, every record read strictly; volumes kept, saying why | `an_unreadable_record_keeps_the_volumes`, `prune_waits_for_the_container_store`, `volumes_are_kept_when_the_container_store_isnt_found` |
| `network connect`/`disconnect` killed between the record and the join (F57) | the same command run again finishes it, with a notice; other settings still `Exists` | `a_cut_short_connect_is_finished_by_its_rerun` |
| A full disk refusing `__cleanup`'s end (F45) | `retry_end` with the policy's backoff, each refusal logged, the record read again under `op.lock` and `running` (10.2f1) | `an_end_the_store_refused_is_retried` |
| An end settled by a pass, its start's kept file left behind (close-a) | forgotten by `reconcile`, `settle_unrecorded_end`, and a failed keep | `an_end_settled_by_a_pass_forgets_the_start_kept_aside` |
| A `die` sent for a start that wrote no end (F80) | `record_end` says whether it wrote; only then `die` | `a_failed_start_keeps_what_its_last_start_left` (exactly one `die`) |
| A monitor that said `Failed` and lives on (F82) | ended after the lock's wait | `a_monitor_that_outlives_its_failure_is_ended` |
| A held message cut short at `EMFILE` (F54) | rustix's `recvmsg` yields what the kernel installed; a truncated message is dropped and its descriptors closed | `a_message_cut_short_at_the_limit_leaves_nothing_open`, `more_passed_than_room_for_is_said` |
| Holds past the caller's descriptor limit (F49) | the monitor raises its own; the companion and init keep the caller's; the exit command gets room | `holds_go_past_the_callers_limit_and_its_helpers_keep_it` |
| A v3 holder against a v4 monitor, and the reverse (F73); `--hold-fd` given a pipe | `NotDone` only to holders of 4 or more; a v3 `Done`'s words mapped; a pipe or a device refused | `what_a_holder_cant_do`, `a_refusal_says_why_from_either_version`, `holds_are_checked` |
| A DNS client keeping a TCP connection open (F58) | closed 30 s after it's served, plus one forward and one write at most | `a_connection_is_closed_at_its_lifetime` |
| A paused container at `stop` or at shutdown (F46) | the cgroup thawed right after the stop's signal, the SIGKILL deadline set first | `a_paused_container_stops_gracefully` |
| A shutdown while the boot unit's `system recover` runs (F52) | a separate `shipyard-stop.service` (`ExecStop` only), the recover unit `PartOf=` it | `the_boot_units_say_what_the_design_needs`; Lima, 31 containers |
| A stop timeout longer than the shutdown gives; stores the boot unit can't see (F44) | `doctor` warns, with the fix | `a_stop_timeout_past_the_units_is_a_warning`, `timespans_are_read_as_systemctl_prints_them`, `stores_the_boot_unit_wont_see_are_a_warning` |
| A failed `pasta --help` cached as "no features"; a relative store; a lease that isn't an object; a health check's late output; a monitor not in its cgroup (F50, F61) | kept only from a `--help` that exited 0; refused; refused; drained; warned | `what_pasta_can_do_is_asked_once_per_binary`, `a_relative_store_is_refused`, `a_lease_that_isnt_an_object_is_refused_by_a_rename`, `a_checks_late_output_is_kept`, `a_monitor_has_joined_when_its_cgroup_lists_it` |
| Two stores' made-up names colliding on the shared `bridge` (F63) | the names live leases hold on the plan's bridge networks avoided, `bridge` included | `made_up_names_avoid_the_names_held_elsewhere`, `the_networks_a_plan_joins_are_named` |
| A `cp` whose container ended between `op.lock` and the helper's join (F84) | retried through the stopped view | `a_start_waits_for_a_copy_into_one_that_ended` |
| Typing overtaking a resize under load (F68) | the client sends a changed size before each piece of typing; the monitor sets a size before typing in the same read | `a_window_size_is_sent_once_per_change`, `a_size_is_set_before_the_typing_after_it` |
| `logs -f` whose reader has gone (F78) | asked between polls, and while waiting for `log.json` | `following_ends_when_its_reader_goes`, `logs_follow_ends_when_its_reader_goes`, `a_follower_waiting_for_its_logs_ends_when_its_reader_goes` |
| A close-all-but gap starting below 3 (10.2f2's mutant) | `Gaps` skips below the next start | `two_kept_lists_merge_and_repeats_count_once` |
| Another Mac user reaching the VM host | the socket in a 0700 directory of the user's own; `getpeereid` | `only_the_same_user_is_let_in`, `a_peers_uid_is_the_kernels_word`, `the_directory_must_be_the_users_own` |
| A container reaching the agent (a root shell on the guest) | only peer CID 2 is served; the seccomp profile's `AF_VSOCK` refusal (7.5c) | `only_the_host_is_served` |
| A path outside the home, or a link out of it, or dangling | refused on the Mac before anything runs | `only_paths_in_the_home_are_forwarded`, `commands_are_forwarded_as_on_linux` |
| A container given the home rewriting the VM's data disk and boot files (close-a, security) | the host names its places under the home (`shipyard.hide=`), PID 1 covers each with an empty read-only tmpfs | `only_places_in_the_home_are_hidden`, `hidden_places_cross_the_command_line`, `the_vms_own_files_are_hidden_from_a_home_bind` |
| `-e PATH` or `-e SHIPYARD_…` steering the guest's `shipyard` | reserved names refused on the Mac and filtered by the agent | `the_guests_own_variables_are_reserved`, `variables_and_input_cross_as_named` |
| A signal to process group 0 or 1 after the session leader's exit (10.3b2, high: it stopped the VM); a signal after the command ended | `signal_target` never 0 or 1; the agent a group of its own; never after the end | `a_signal_never_goes_to_this_group_or_init`, `only_forwardable_signals_pass` |
| A pseudo-terminal inherited by another connection's command | both ends close-on-exec from the start | `a_pty_is_close_on_exec_and_paired` |
| The Mac's CLI lost mid-command | the agent hangs the command up (SIGHUP to its group) | `a_hangup_on_the_mac_ends_the_command_in_the_guest`, `a_third_interrupt_lets_go_and_the_guest_hangs_up` |
| A Mac program already on a published port; a port in `TIME_WAIT` | left to it and said, retried every 30 s; `TIME_WAIT` bound again | `a_port_in_use_on_any_address_is_busy`, `a_port_in_time_wait_is_bound_again`, `a_port_in_use_on_the_mac_is_said_and_left_alone` |
| A `-p` range of about 1,100 ports overflowing the ports answer (10.3c1, high: all forwarding stopped) | the answer travels in output frames, read up to 8 MiB | `a_wide_ranges_answer_fits` (2,000 ports) |
| A stopped listener accepting once more; a flood of connections | a wake pipe, joined on stop; 1,024 slots, 256 per address | `a_stopped_listener_frees_its_port_at_once`, `connections_are_bounded` |
| A command landing as the VM stops idle; a refusal before the hello is written (10.3c2, high) | refused before the agent, the answer read either way, sent once more after the stop, never a third time | `a_command_refused_as_the_vm_stops_is_sent_again_once`, `a_command_sent_into_a_stop_runs_after_it`, `commands_racing_the_idle_stop_all_run` |
| A guest that stops answering the poll | never idle-stopped; said in `host.log` | `a_guest_that_doesnt_answer_is_said_not_stopped` |
| `vm stop` while another command starts a new host | waits for its own host's PID to be gone | `a_stop_waits_for_its_own_host_only` |
| Ctrl-C while a command waits for the VM | `VmError::Interrupted(128 + N)` | `a_wait_is_ended_by_a_stopping_signal` |
| An empty, foreign or half-formatted data disk; an unclean one | formatted only when its first MiB is zeros; foreign refused; `e2fsck -p` before every mount | `a_disk_is_formatted_only_when_empty`, `a_disk_is_read_by_its_superblock`, `a_checked_disk_mounts_only_when_fixed` |
| A tampered guest; a newer manifest | each file's sha256 checked; a newer version refused | `a_guest_needs_its_manifest_and_its_own_files`, `a_manifest_of_version_1_is_read_and_a_newer_refused` |
| The guest's clock after the Mac's sleep | every request, the ports poll's too, carries the Mac's time | `a_clock_is_set_only_when_it_drifted`, `an_internal_request_carries_the_clock` |
| `doctor` booting a 4 GiB VM to check it | `client::ask` runs only on a running VM | `asking_never_starts_the_vm` |

## Fuzzing

Stage 10 added no fuzz target. `logs_dir` was re-pointed at the runtime's reader when it
moved (10.1e4a). `attach_frames` covers the new frames (`HelloRun`, `Signal`, `NotDone`)
through its generic round trip: any frame it decodes is encoded and decoded again.

- **10.1e4a:** all 14 targets, 60 s each; `logs_dir` (now the runtime's) 984,667 runs; no
  crash.
- **The new formats** (`forward::Request`, `PortsAnswer`, `GuestDoctor`, `manifest.json`,
  `ports.json`, `previous-start.json` v2, `pasta-features.json`) are serde JSON read with a
  bound, each with a version test. Their writers are the user's own processes or the
  guest; none comes from a container or a registry.
- **The close's run** (2026-10-18): all 14 targets, 300 s each, 223,833,704 runs in all;
  no crash. One "slow unit" was written for `process_json`: an input that isn't UTF-8,
  which the target doesn't parse (100 runs of it take 22 ms), timed at 649 s because the
  Mac slept meanwhile (Docker's VM, and the fuzzer's clock, paused): not a finding.

## Known limitations (by design, for stage 10)

- **Monitors an older binary started keep its behaviour** until they exit: nix's receive
  (F54's leak) and the caller's descriptor limit (F49). The records they write stay at
  version 3 (F43's rule).
- **The runtime's files aren't synced** (`Durability::Atomic`, D10.1c-3): a power loss can
  lose a write to `/run`, which a reboot empties anyway. No test can tell this apart but
  time.
- **A user's `stop` that failed on a full disk** may be overridden by `__cleanup`'s retry
  applying the policy; the user saw the error (10.2b2).
- **`doctor`'s lines come all at once**, after its probe (up to 15 s on a stuck host),
  where v0.2.0 printed the first ones before it (10.1e1).
- **An attach client's resize dedup:** two clients on terminals of different sizes,
  resizing A → C → A, can leave the other's size: Docker's "last resize wins" (10.2e2).
- **Two `PATH`s finding pasta in two places** replace each other's cached features record,
  as before F50.
- **The Mac path** is minimal (D24): TCP only on the Mac's loopback (F96, F97); a Mac
  program started after a port is forwarded, with `SO_REUSEADDR` on every address, is
  shadowed (F98); a change on the Mac isn't seen as it happens in a container (no file
  events over virtiofs: a dev server must poll); no login item, so policies come back only
  at the next command after a Mac reboot (F106); the guest is built from the source tree
  and the binary must be signed; the guest can read and write the whole home.
- **Input a command doesn't read can hold its signals back** (F94): the third Ctrl-C or
  the SIGTERM grace leaves, and the command in the guest is hung up on.
- **Survivors, each argued:** `watcher-no-flag` (10.3a: the ordered stop's flag; the main
  thread usually sees the guest stop first, so the race isn't lost in a test run);
  `gone-never-seen` (10.2f1: equivalent, the start-time check refuses the extra end); the
  health drain's zero poll (10.2d2: equivalent). Not tested: `boot_id` unreadable (`/proc`
  can't be made so); the `SUDO_USER` wiring (environment changes in a threaded test
  binary are unsound); the third Ctrl-C and SIGTERM grace of the pipes path end to end
  (checked by reading; the terminal path's are tested).

## Carried forward

- **Closed in stage 10:** F37 (10.2f2), F43 and F48 (10.1), F44, F45, F56, F57 (10.2b2),
  F46, F52 (10.2d1), F49, F54 (10.2c1), F50, F61 (10.2d2), F53, F60, F65, F70 (10.2a), F55,
  F64, F66 (10.2b1), F58, F73 (10.2c2), F63, F83, F84, F85 (10.2e1), F68, F75 (10.2e2),
  F74 (10.2f3), F78, F80, F82 (10.2f1), F91 (10.3b1: Docker's behaviour). F86 withdrawn
  (the cause was a test's own descriptors, now in AGENTS §10).
- **To stage 11** (the API and the drop-in CLI): F34, F62 (D10.2-2); F71; F72 (`inspect`'s
  `running` and wordings); F76 (seams in `shipyard-sys`'s lock errors, "settle", the run's
  end types); F81 (the policy after a failed user `start`); F88 (a `die` lost to a kill
  between the end's write and the event); F94 (signals behind input); F99 (the stopping
  refusal by its words); F101 (`-v`, `ps`'s COMMAND); F102 (a retry writing one start's
  exit over another's); F107 (helpers that grew a copy per step: the versioned read,
  "128 + signal", three doctor `Level`s, `Flock`s outside `shipyard_sys::lock`).
- **To stage 13** (macOS production): F59 (D10.2-2); F69 (`cp`'s runtime half); F77 (the
  port lock across users); F90 (a guest panic unseen); F92 (packages pinned by sha256);
  F93 (`vm stop`'s PID window); F96 (UDP); F97 (the guest's `::1`, port clashes); F98 (a
  later wildcard listener); F100 (`connect`'s one bound); F103 (the data disk only grows);
  F104 (`e2fsck -p` failing powers off for good); F105 (macOS privacy permissions,
  unconfirmed); F106 (login item, skew seen only at start, the host's first environment,
  the port poll's cost, `host.log`'s rotation).
- **To stage 15:** F67 (an engine-set field at a record v4); F79, F87 (flakes not
  reproduced); F89 (25 long modules); F95 (a raw terminal after a panic with
  `panic = "abort"`).
- **Still open from earlier:** F24, F29 (stage 11), F31, F47 (a known limitation), F51.

## Tests

Counted at the close's part a (`#[test]` attributes, or the names in a `harness = false`
file's list):

| File | Tests | Stage 10's |
|---|---|---|
| `shipyard-engine/tests/detached.rs` (`harness = false`) | 134 | 7 new (127 at v0.2.0): `a_held_area_refuses_a_view_and_a_start`, `a_failed_start_keeps_what_its_user_stopped`, `a_stop_of_a_dead_start_stays_stopped`, `a_cut_short_connect_is_finished_by_its_rerun`, `an_end_the_store_refused_is_retried`, `a_paused_container_stops_gracefully`, `an_end_settled_by_a_pass_forgets_the_start_kept_aside`; `a_failed_start_keeps_what_its_last_start_left` counts its `die`s; four wait for `running` rather than a `finished` |
| `shipyard-engine/tests/boundaries.rs` | 4 | 1: `only_the_engines_tests_write_records_directly`; test directories skipped as `tests.rs` files are (10.2f3) |
| `shipyard-cli/tests/vm.rs` | 12 | all: boot, two first commands, forwarding, variables and input, ports (2), idle (3), `doctor`, a command into a stop, the hidden files |
| `shipyard-cli/tests/vm_terminal.rs` (`harness = false`) | 6 | all: `run -it`, Ctrl-C as a signal, the third interrupt, a hang-up, the detach keys, a plain command on a terminal |
| `shipyard-cli/tests/containers.rs` | 20 | 2: `logs_follow_ends_when_its_reader_goes`, `logs_end_as_their_readers_do_by_name` |
| `shipyard-cli/tests/runtime.rs` | 189 | 3: `following_ends_when_its_reader_goes`, `runtime_logs_end_as_their_readers_do`, `holds_go_past_the_callers_limit_and_its_helpers_keep_it` |
| `shipyard-cli/tests/network.rs` | 20 | F85's `operstate` read replaced by `IFF_UP` |
| `shipyard-sys/src` | 44 | all: `fs` 23, `lock` 8, `fdpass` 5, `pidfd` 4, `fd` 2, `relay` 2 |
| `shipyard-vm/src` | 35 | all: `idle` 8, `client` 6, `doctor` 5, `forwarding` 4, `ports` 4, `paths` 3, `manifest` 2, `peer` 2, `host` 1 |
| `shipyard-engine/src` | 201 | 154 at v0.2.0: `containers/tests/` 47 (34 in one file at v0.2.0), `doctor/tests.rs` 20 (moved from the CLI and new), `guest/` 11, `paths.rs` 7, `volumes.rs` 5; the logs reader's 9 moved to the runtime |
| `shipyard-runtime/src` | 462 | 430 at v0.2.0: `attach/forward.rs` 9, `logs/reader.rs` 9 (moved), `lifecycle/attach/tests.rs` 7, `attach/client.rs` 2, and the protocol, signals, terminal and lifecycle units |
| `shipyard-network/src` | 263 | 254 at v0.2.0: `.routing-closed`, the lease store, pasta's cache, the reservation lock, the DNS lifetime |
| `shipyard-image/src` | 164 | 163 at v0.2.0 (the index's tests moved to `index/tests.rs`) |
| `shipyard-cli/src` | 88 | 87 at v0.2.0: `forward.rs` 7, `output.rs` 6, `doctor.rs` 4 (13 moved to the engine), `run/tests.rs` 36 (moved from `run.rs`) |

Totals (the close's verification, 2026-10-18, `verify.sh` with its new VM part):
macOS 1,164 passed, 0 failed, 3 ignored; Linux, both passes, 3,764 passed, 0 failed,
410 ignored; the macOS VM tests 18 passed, 0 failed; fmt, clippy (macOS, glibc, musl),
rustdoc and `cargo deny` clean. The differential against v0.2.0's binaries (mac, tester,
root): only the help's known lines (the `vm` command, 10.2's two texts). Mutants: every
step's, controls first (in the review history); the close's 10/10. Fuzzing: 14 targets,
300 s each, no crash.

**Mutants, hand-picked, controls first, per step:** 10.1a 9/9 (one per entry point),
10.1b 7/7, 10.1c1 14/14, 10.1c2 11/11, 10.1c3 7/7, 10.1d 6/6, 10.1e1 4/4, 10.1e2 1/1,
10.1e3 4/4, 10.1e4a 4/4 (10.1e4b: a visibility change, none); 10.2a 7/7, 10.2b1 5/5,
10.2b2 8/8, 10.2c1 6/6, 10.2c2 6/6, 10.2d1 7/7, 10.2d2 8/8 (one equivalent), 10.2e1 4/4,
10.2e2 2/2, 10.2f1 5 of 6 (one equivalent), 10.2f2 5/5; 10.3a 11 of 12 (one argued),
10.3b1 8/8, 10.3b2 11/11, 10.3c1 14/14, 10.3c2 11/11, the close's part a 10/10.

## Review history

| Step | What | A finding of its own reviews |
|---|---|---|
| 10.1a | `run/detached.rs` and `containers/mod.rs` split by job; one `entry_points`; the differential tool | the differential could pass vacuously (a failed pull fails every case alike): positive controls added; a `create` ⇄ `start` module cycle |
| 10.1b | one `take` (`take`, `take_or_leave`), `lock_again`, `settled`, one meaning of "up" | `Needs::Created` read as "never started" (now `Attachable`); 125 written three ways (`UNKNOWN_EXIT`) |
| 10.1c1 | `shipyard-sys::fs`: atomic writes, bounded reads, lock files; every site adopted | `.routing-closed` capped at 4 KiB, so a host of many interfaces got a firewall leaving it routing for anyone (high); F65 recorded |
| 10.1c2 | `shipyard-sys::lock`: `lock_until`, `try_lock_until`, `HeldFile` | a volume hold and an area taken but not yet wrapped, so a failure after locking kept them; F66 recorded |
| 10.1c3 | `pidfd`, `fdpass`, `fd::take_inherited` | `take_inherited` set its flag before owning the descriptor (a failure there would have leaked it); `send` with no descriptors sent a control message |
| 10.1d | F43's rewrite rule; `config/` split | two rustdoc links broken by the move (high); the rule's line had no failing test (a test-only hook added); F67 recorded |
| 10.1e1 | `doctor`'s checks into the engine | no logic finding; the AGENTS.md entry-point text and `checks`' doc (the binary must run `entry_points`) |
| 10.1e2 | the API narrowed, clashing names renamed | `containers::testing` ungated, so "only the engine's tests write records" held by convention: a boundary test |
| 10.1e3 | the CLI's `main.rs` and `run.rs` split; `lib.rs` | stuttering names (`images::images`); the rustdoc and musl runs predated the final tree |
| 10.1e4a | the attach client and logs reader into the runtime | F69's and F48's rows overstated the engine's removal; F70 and F71 carried |
| 10.1e4b | the last engine-only `pub` items narrowed; AGENTS §5's two rules | `restarts_held_off`'s untested macOS stub hidden by an allow (now `cfg`); rustdoc failed on private links |
| 10.1 close | whole-step reviews | a symlinked `.routing-closed` read as "not closed" (folded into F65); F72–F76 recorded |
| 10.2a | F65, F53, F60, F70 | the plan's Murphy review: refusing the run left a flushed host routing for anyone (close and warn instead); the build's: the fallback cut off Docker's and libvirt's bridges. A test hung 34 min on a `nix` pipe without `O_CLOEXEC` |
| 10.2b1 | F55, F64, F66 | a `stop` of a dead start undone by v2's restore (a regression; a stop since now wins) |
| 10.2b2 | F56, F57, F44, F45 | `Busy` not retried; a mutant survived because the test restored the directory before the first retry |
| 10.2c1 | F54, F49 | D10.2-12's stated reason was wrong (the raised limit is what lets a connected container restart); a latent `F_DUPFD` bug in a test helper |
| 10.2c2 | F73 (protocol v4), F58 | the v3 fallback tested only as a pure mapping (`answer` driven through a socket pair); `--hold-fd`'s message didn't say what it takes |
| 10.2d1 | F46, F52; `doctor`'s stop timeout | `systemctl stop shipyard-stop` left `.host-stopping` for good (`PartOf=`); rootless compared the wrong limit (Ubuntu's 5 s) |
| 10.2d2 | F50, F61's five | a failed `pasta --help` kept for the session as "no features"; `ctime` missing from the key; a pasta test made a ports test fail 2 in 5 |
| 10.2e1 | F63, F83, F84, F85 | F63's fix missed the default network, the case the row is about (both reviews); F83's relaxation left `Dropped` untested anywhere |
| 10.2e2 | F68 found and fixed; F75 measured; F79, F87 looped | the monitor wrote typing before a size asked in the same read |
| 10.2f1 | F78, F80, F82 | `retry_end` checked the record before taking the locks (a second `die`, or an older end over a newer one); F88 recorded |
| 10.2f2 | F37: `ResizeWatch`, one close-all-but, `terminal::Size` | `spawn.rs` still read `size.0` (Linux only: the Linux build would have failed); the watch's drop drained every pending flag |
| 10.2f3 | F74's modules split by seven agents | the engine's boundary test skipped only files named `tests.rs`, not a `tests/` directory |
| 10.3a | the guest, its PID 1 and agent; `shipyard-vm`; `vm start\|status\|stop` | the plan's Murphy review: `pivot_root` refuses the initramfs root (critical); the build's: an interrupted first format refused forever, the stop answered before PID 1 was signalled |
| 10.3b1 | every command forwarded without a terminal; `SHIPYARD_VM=off` | signals sent in macOS numbers (both reviews); dangling links let through; stderr's `EPIPE` taken for stdout's |
| 10.3b2 | terminals through the VM | `killpg(0)` after the leader's exit signalled PID 1's group, stopping the VM (high); pty descriptors briefly without `O_CLOEXEC` |
| 10.3c1 | ports on the Mac's loopback | the ports answer's 32 KiB cap: a `-p` range of about 1,100 ports stopped all forwarding (high); a stopped listener could accept once more |
| 10.3c2 | the idle stop; `doctor` on macOS; the Mac milestone | a host refusing and closing before the hello was written gave `EPIPE` and no retry (high); Ctrl-C did nothing while waiting for the VM |
| close, part a | the guest's checks in `doctor`; three whole-stage reviews | an unknown container store's place read as "no records", so `volume rm`/`prune` deleted stopped containers' volumes (data loss); a home bind could rewrite the VM's own disk (security); F102–F107 recorded |

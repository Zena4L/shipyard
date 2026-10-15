# Stage 9 notes: engine state

The engineering record of stage 9: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [Containers](../user/containers.md), [Running
commands](../user/run.md), [Events](../user/events.md), [Networking](../user/networking.md),
[The OCI runtime](../user/runtime.md) and [Exit codes](../user/exit-codes.md).

## Outcome

Stage 9 asked for one state model shared by every `shipyard` invocation, without a daemon:
records on disk, locking, crash recovery, IDs and names, restart policies, health checks,
an `events` stream, commands across processes, and the networks of named containers moved
from stage 7 ([ROADMAP](../ROADMAP.md), [FEATURES
§9](../FEATURES.md#9-engine-state-daemonless--stage-9)). From the hardening track it took
on the engine crate (H1), one versioned state model (H2), errors as a designed surface
(H6) and more timing budgets (H7); 9.8b built H5's mutants and scheduled fuzzing. It also
closed F10, F30, F36, F38, F39, F40, F41 and F42. A user can now:

- `shipyard run -d --name web --restart on-failure:3 alpine …` and get the prompt back:
  the container runs on under its monitor, holds its network, ports, layer and volumes
  without the CLI, and comes back when it fails;
- reach it by name, ID, ISO 6346 serial (`SHPU 304812 2`) or ID prefix from any shell:
  `ps`, `inspect`, `logs [-f]`, `attach`, `exec [-it]`, `cp` (running or not), `stop`,
  `kill`, `wait`, `restart`, `rename`, `update --restart`, `port [--sockets]`, `rm`;
- run in the foreground as before: an image or `--rootfs` run is a container too (kept in
  `ps -a` unless `--rm`), `-it` detaches with Ctrl-P Ctrl-Q;
- have containers settled after a crash, started again after a reboot
  (`shipyard system recover` from `contrib/systemd`) and stopped gracefully at shutdown
  (`shipyard system stop`);
- read what happened (`shipyard events`), see health checks in `ps` and `inspect`, and
  connect a running container to another network.

**The milestone** (v0.2: containers run detached and survive the CLI exiting; `logs`,
`exec`, restart policies) is proved through the `shipyard` binary by
`the_v0_2_milestone_a_named_container_end_to_end` (`crates/shipyard-cli/tests/containers.rs`,
root or rootless as the suite runs): `run -d` returns while the container runs on; `logs -f`
follows it; `exec`, `attach` (typing reaches it, the keys detach) and `cp` reach it; its
process killed from outside, its restart policy starts it again (`die` then `start` in the
events, its logs showing both starts); `system recover` leaves a running one as it is;
`stop` and `rm` leave nothing. A reboot is the engine's test
(`boot_starts_what_the_policies_say`, a simulated reboot) and the manual check's.

It was also run by hand on 2026-10-15 (step 9.9b), rootless and as root, with static (musl)
builds, on three Lima VMs: Ubuntu 24.04 arm64 (vz) with AppArmor's unprivileged
user-namespace restriction on and Shipyard's profile installed; Fedora 44 arm64 (Linux
6.19) with SELinux enforcing and firewalld running; and Ubuntu 24.04 x86-64 under QEMU
11.1.2 (emulated; the binaries built in an amd64 container), stage 9's first run on x86-64.
A script drove a pseudo-terminal (`target/scratch/vm/milestone.py`, not committed) through:

- `doctor`, `pull`, `run -d` back at the prompt, `ps`, `logs`, `exec` and `exec -it`;
- `attach` (typing reaches it; Ctrl-P Ctrl-Q, then Enter for a container without a
  terminal), `cp` in and out, `port`;
- `kill -9` of its process from outside, and the policy's restart (`events`: `die`, then
  the restart's `start`); a health check;
- `network connect` and `disconnect` (root), then the clean-up.

13 of 13 checks passed rootless and 14 of 14 as root, on each host. Then the boot units
(`contrib/systemd`, `systemctl enable --now`, linger for the user's) and a reboot: the
`always` and `unless-stopped` containers came back, the one its user stopped stayed down,
and `doctor` had no `restarts` warning: 4 of 4 rootless and 4 of 4 as root, on each host.

The runs found one real bug, fixed in 9.9b (see the [register](#failure-mode-register)):
root's containers made from a login session (`sudo shipyard run -d` over SSH) had their
monitor in the session's scope, which systemd ended at shutdown before the unit's `system
stop` reached them. Each container's monitor now has a cgroup of its own; the reboot was
run again with the fix on all three hosts. They also found three gaps in the docs,
fixed: `contrib/systemd/README.md` said `enable`, not `enable --now` (the first shutdown
then isn't graceful); `loginctl enable-linger` needs `sudo` on Fedora (polkit), now said in
the README, `containers.md` and `doctor`'s hints; and `containers.md` didn't say that a
container without a terminal sees the detach keys only once Enter sends the line.

**A forced logout**, observed and recorded as it is (asked): `loginctl terminate-user` is
harsher than a logout. It stops even a lingering user's manager and empties `/run/user/UID`,
so the user's containers end; the user unit (linger) brought them back.

**The upgrade check** (9.9a, H2). Stage 8 made no engine records, but its
`shipyard-runtime` containers share today's state root. Stage 8's real binaries (built from
`main`, `906f1fe`, in the test container) made and started a container with logs and its
stdin open; stage 9's binaries then drove it: `shipyard-runtime` `state`, `logs`, `exec`,
`attach` (typing into it), `cp`, `pause`/`resume`, `update`; `shipyard ps`, `system
recover`, `system stop` and `system recover` again (each leaving it alone); `kill`, the
stage 8 monitor's own exit file ("killed by SIGTERM (exit code 143)"), `delete`, and the
monitor gone. 21 of 21 checks passed. The permanent test is
`a_container_stage_8s_runtime_made_is_left_to_it` (stand-ins for the monitor and init, as
stage 8 recorded them); `a_container_an_earlier_version_recorded_is_managed` covers a
version 1 engine record.

The stage was built in thirty-four steps up to 9.9a, each planned with the user's
decisions asked first, built, reviewed (clean code, then what could go wrong; one combined
review for the smaller steps), mutation-checked and run by hand on Linux, then reviewed as
a whole (see [Review history](#review-history)).

| Requirement | Where | Tested by |
|---|---|---|
| The engine crate (H1): the CLI's logic behind typed APIs; it never prints and reads the environment only in `Paths` | `crates/shipyard-engine` (`run/`, `image/`, `network/`, `volumes.rs`, `report.rs`, `paths.rs`), `deny.toml` | `the_engine_never_prints`, `only_paths_reads_the_environment`, `the_checks_find_what_they_look_for` (`boundaries.rs`); `a_run_ends_as_its_command_does` (`launch.rs`); `the_engines_events_print_as_the_clis_lines`; 9.1's differential corpus (90 cases, the previous binary against the new, identical on macOS, Linux tester and root) |
| Errors with kinds (H6), warnings with kinds | `error.rs` (`ErrorKind`, the other crates' `From` maps), `report.rs` (`WarningKind`) | `other_crates_errors_have_their_kinds`, `bail_returns_an_error_of_its_kind`, `a_requests_mistakes_have_kinds`, `context_keeps_the_kind_and_finds_the_foreign_error` |
| On-disk state: records in `/var/lib/shipyard` (rootless `~/.local/share/shipyard`), runtime data under `/run` | `containers/` (`mod.rs`, `record.rs`), `paths.rs` (`containers`, `events`), runtime `container/versions.rs`, `plan.rs` | `a_container_is_made_found_and_removed`, `version_1_reads_as_written`, `version_2_reads_as_written`, `version_2_and_1_read_as_written`, `a_newer_state_json_is_refused_by_kind`, `a_plan_reads_as_written`, `a_store_others_could_change_is_refused` |
| Locking: concurrent commands never corrupt state | the store's `.lock`; a container's `op.lock`, `running`, `restarting` | `concurrent_creates_keep_names_and_serials_unique`, `a_reader_never_sees_half_a_container`, `start_and_rm_never_interleave`, `running_is_free_only_once_its_end_is_settled`, `rm_beside_a_listing_of_the_areas` |
| IDs (64 hex), ISO 6346 serials, names, labels; lookup by any | `containers/id.rs`, `names.rs`, `labels.rs`, `find` | `check_digits_are_iso_6346s`, `serials_are_read_as_written`, `names_follow_the_rule`, `labels_are_bounded`, `lookup_goes_id_name_serial_prefix`, `an_ambiguous_prefix_names_the_matches`, `a_container_is_found_by_name_serial_and_prefix` |
| Docker's spelling, `run IMAGE [COMMAND]` (v0.1's carried item) | the CLI's `run.rs` (`image_and_command`), `--no-image` | `the_image_comes_first`, `flags_that_need_an_image`, `a_bare_word_on_the_path_is_a_host_command` |
| Containers that outlive the CLI: holds handed to the monitor, released by `__cleanup` | `run/detached.rs`, `run/cleanup.rs`, `run/holds.rs`; runtime `create_planned`, `--hold-fd`, `--exit-command`; each holder's `give_up` | `a_detached_container_outlives_its_caller_and_is_cleaned_up`, `a_hold_outlives_create_and_is_the_exit_commands`, `holds_go_with_the_container_or_its_monitor`, `an_upgraded_binary_still_cleans_up`, `a_lease_handed_over_outlives_its_holder`, `an_area_handed_over_outlives_its_holder`, `a_run_dir_handed_over_outlives_its_holder` |
| A detached container's pasta follows its init, not its caller | `shipyard-network/src/pasta.rs` (`Follow::Init`), `network/pasta.rs`, runtime `init_process` | `a_pasta_container_follows_its_init`, `a_pasta_whose_launcher_was_killed_ends_with_the_clean_up`, `a_pasta_launcher_pins_no_host_mount`, `a_created_containers_init_can_be_followed` |
| `create`, `start`, `run -d`, `ps`, `inspect`, `rm` by name; a container's own writable layer, its image pinned | `run/detached.rs`, `containers/view.rs`; `shipyard-image` `containers.rs` (`ctr-<id>`), `index.rs` (pins); the CLI's `containers.rs` | `run_d_prints_the_id_and_rm_goes_on`, `a_container_starts_again_with_its_files`, `the_image_stays_with_its_container`, `rmi_refuses_a_containers_image`, `two_container_stores_share_an_image_store`, `run_d_rm_leaves_nothing` |
| Commands across processes: `stop`, `kill`, `wait`, `restart`, `rename` | `run/stop.rs`, `run/rename.rs`; the runtime's control protocol v2 (`lifecycle/control.rs`, `lifecycle/attach.rs`) | `stop_restart_kill_and_wait_by_name`, `stop_ends_it_and_it_starts_again`, `stop_kills_one_that_ignores_its_signal`, `a_stop_killed_meanwhile_still_ends_it`, `kill_reaches_one_a_stop_waits_on`, `wait_gives_the_exit_code`, `rename_gives_a_container_a_new_name`, `a_renamed_container_answers_to_its_new_name` |
| `logs`, `attach`, `exec`, `cp` by name (F38); `cp` of a stopped container | `logs.rs` (`of_container`), `attach.rs` (`to_container`), `exec.rs`, `cp/`; runtime `RootView`, `start_helper_in_view` | `logs_exec_cp_and_attach_by_name`, `logs_cover_every_start`, `logs_follow_ends_when_it_stops`, `attach_gets_its_output_and_its_end`, `start_a_misses_nothing`, `exec_runs_as_a_user_by_name`, `cp_reaches_a_stopped_container`, `cp_into_a_created_container`, `a_stopped_cp_keeps_its_owners`; `attach_by_name_to_a_terminal_container`, `exec_it_by_name` (`terminal.rs`) |
| A foreground image or `--rootfs` run is a container (D9.4c-2); `-t` through the attach client | `run/detached.rs` (`attached`, `Forwarder`), `attach.rs` (`session_with`), runtime `SignalWatch`, `HelloHeld` | `a_foreground_run_is_a_container`, `a_foreground_run_is_a_container_by_name`, `a_slow_run_misses_nothing`, `a_forwarded_signal_ends_it_with_its_code`, `the_container_outlives_its_run`; `a_terminal_container_run_is_raw_and_detaches`, `a_hangup_ends_a_terminal_container_run` (`terminal.rs`) |
| Crash recovery: recorded state against what runs | `run/recover.rs` (`recover_others`, `settle_unrecorded_end`), `detached.rs` (`reconcile`) | `a_killed_monitors_container_is_settled_by_the_next_create`, `a_crashed_rm_container_is_removed`, `a_reboot_is_told_by_the_boot_id`, `recovery_leaves_a_busy_container_alone`, `a_dead_start_is_settled` |
| Restart policies, enforced by the monitor's exit command rather than a daemon | `run/restart_policy.rs`, `run/cleanup.rs`, `run/update.rs` | `policies_decide_as_dockers`, `the_backoff_doubles_resets_and_stops_at_a_minute`, `on_failure_restarts_up_to_its_max`, `always_restarts_until_its_user_stops_it`, `a_stop_while_restarting_ends_the_restarts`, `update_to_no_ends_the_restarts`, `a_killed_restart_waiter_is_settled`; the milestone |
| *Optional:* systemd units so containers start at boot; a graceful host stop | `run/recover.rs` (`boot`, `stop_for_the_host`), `contrib/systemd/`, the CLI's `doctor.rs`; the monitor's own cgroup (runtime `cgroup::MONITOR_PARENT`, `create_from`) | `a_monitor_lives_in_a_cgroup_of_its_own`, `what_starts_at_boot_is_dockers`, `boot_starts_what_the_policies_say`, `the_host_stop_is_graceful_and_not_a_users`, `a_restart_waiting_for_its_lock_sees_the_host_stopping`, `linger_off_is_a_warning`, `kill_user_processes_is_read_as_logind_reads_it`; the 9.5c VM checks |
| Health checks, shown in `ps` and `inspect` | runtime companion (`CreateOptions.companion`, `monitor.rs`), `health.rs`, `run/health.rs`, `shipyard-image` `manifest.rs` (`RawHealthcheck`) | `the_status_follows_dockers_rules`, `a_check_merges_as_dockers`, `a_passing_check_is_healthy`, `failing_checks_make_it_unhealthy`, `a_check_out_of_time_fails_and_leaves_nothing`, `the_images_check_is_used`, `a_companion_runs_beside_the_container_and_ends_with_it`, `health_shows_in_ps_and_inspect` |
| The `events` stream | `events.rs`, the CLI's `events_output.rs` | `a_line_reads_back_and_version_1_is_pinned`, `concurrent_appends_stay_whole_across_a_rotation`, `a_follower_reads_across_a_rotation`, `events_tell_a_containers_life`, `an_oom_kill_says_oom_then_die`, `past_events_are_listed_and_it_ends`, `filters_narrow_it`, `new_events_are_followed` |
| `network connect` / `disconnect` on a running container | `run/connect.rs`, `network/live.rs`; runtime `lifecycle/holds.rs`, the control protocol v3 | `a_running_container_joins_and_leaves_a_network`, `the_record_keeps_it_and_the_way_out_needs_force`, `a_stopped_containers_network_is_kept`, `an_interfaces_settings_are_set_once_it_is_made`, `holds_are_added_and_let_go_of_while_it_runs`, `a_holder_holds_and_releases` |
| `port` and a view of a container's sockets; names for everything (DNS by short ID, `network inspect`'s `Container`, in-use messages naming containers) | `containers/view.rs` (`ports`, `sockets`), `shipyard-network` `store.rs` (`names`), `observe.rs`, `network/commands.rs` | `port_shows_what_a_container_publishes`, `port_sockets_shows_its_listeners_and_peers`, `a_containers_short_id_is_one_of_its_names`, `in_use_names_its_containers` |
| The DNS servers in a cgroup of their own, restarted if killed | `network/dns.rs`, `network/dns_server.rs` (the watchdog), runtime `DNS_PARENT` | `a_server_has_a_cgroup_of_its_own_till_its_network_goes`, `a_killed_servers_worker_is_started_again`, `backoff_doubles_resets_after_a_minute_and_gives_up` |
| A warning when a container's pasta dies | `shipyard-network/src/pasta.rs` (`Lost`), `network/lost.rs` | `a_dead_pasta_is_said`, `a_pasta_that_ends_is_said_with_its_code`, `a_pasta_ending_with_its_init_isnt_a_loss`, `a_lost_file_reads_back_and_version_1_is_pinned` |
| F10: a per-container conntrack limit; F30: `--isolate-host`, per-lease DNS TCP slots | `nft/ruleset.rs` (ruleset v7, v8), `nft/expr.rs`, `dns/server.rs` | `a_limited_network_caps_each_containers_connections`, `an_isolated_network_reaches_the_host_only_for_its_dns` (`firewall.rs`); `a_limited_network_counts_each_containers_new_connections`, `a_dynset_nests_its_count_whole`, `a_reject_says_its_kind_and_one_byte_code` |
| F36 (mounts steered into `/proc`, `/sys`, `/dev`), F40 (the attach decoder, resizes) | `process/rootfs.rs` (`steered`), `attach.rs` (`Decoder`), `lifecycle/attach.rs` | `a_mount_a_symlink_leads_into_a_later_standard_mount_is_refused`, `a_rootless_mount_a_symlink_leads_into_sys_is_refused`, `a_mount_led_into_dev_is_refused_and_one_written_there_isnt`, `a_mount_steered_into_a_standard_directory_is_told`, `a_flood_of_small_frames_is_read_in_one_pass`, `a_flood_of_sizes_is_one_change` |
| F42: a port in `TIME_WAIT` held again | `shipyard-network/src/ports.rs` | `a_port_closed_connections_wait_on_is_held`, `its_pastas_ports_are_free_once_its_end_is_settled` |
| Upgrades (H2): every record read in every version written; stage 8's containers left to its runtime | `containers/record.rs`, runtime `versions.rs` | `a_container_an_earlier_version_recorded_is_managed`, `a_container_stage_8s_runtime_made_is_left_to_it`, `stage_8s_reader_finds_the_code_in_a_version_2_record`; the upgrade check above |
| Budgets (H7) | `tests/budgets.rs`, and beside what they time | `run_d_returns_quickly` (under 2 s), `ps_over_a_hundred_containers_is_quick` (under 200 ms; a `create` among 100 records within 100 ms of one among none), `stop_takes_its_timeout_and_no_more`, `a_terminal_adds_little_to_a_container_run`, `fifty_thousand_events_are_read_in_linear_time`, `an_emit_is_cheap`, `a_passing_check_is_healthy` (a check under 1 s, an idle companion under 2 MiB) |
| Mutants and fuzzing in CI (H5) | `.github/workflows/quality.yml`, `.cargo/mutants.toml`, `scripts/fuzz-targets.sh`, `scripts/test-linux.sh --mutants` | actionlint clean; the first runs found a wrong fuzz expectation and an untested truncation (see [Fuzzing](#fuzzing)) |
| The v0.2 milestone | all of the above | `the_v0_2_milestone_a_named_container_end_to_end`; the manual milestone (above) |

**Moved out of stage 9:**

| Item | Now in | Why |
|---|---|---|
| The libseccomp and bpftool differential tests | stage 14 | D9.9-5: they belong with the seccomp and device tooling |
| The stage's closing book chapters, the glossary pass and the recheck of earlier chapters; chapter 52 (step 9.7) | not planned | D9.9-1 (asked); chapter 52 skipped at the user's request. Chapters 36 to 51 were written with their steps |
| The `Quality` workflow on its schedule | when Actions billing is back | D9.8-2: `workflow_dispatch` only, the schedule written and commented out |
| F43 (records across a version bump), F44 (the boot unit's environment), F45 (a full disk), F46 (a paused container at stop), F48 (the clean-code refactor) | stage 10 | 9.9a's whole-stage reviews; F48 is stage 10's first step |
| F37's rest: attach's own `Winch`, the after-fork close-all-but sweeps merged, `Size` as a type of its own | stage 10 | signal-disposition code, quoted by the book; left with F48's refactor |
| A device node steered by a symlink checked by place, as mounts are | later (F31) | 9.8a's review: the place check covers mounts only |
| `stats`, `top`, `pause`/`unpause` and `update` (beyond `--restart`) by name; `tag`, image `inspect`, `history`, `save`/`load`; `--mount` | stage 10's first steps | the stage outline moved them out (not needed for v0.2); placed there when asked in 9.9b |
| F49 (the monitor's descriptor limit for live holds), F50 (`pasta --help` in the journal) | stage 10 | 9.9a's review and 9.9b's manual milestone |
| F51 (a rootless monitor made from an SSH login stays in the session's scope) | later | 9.9b's manual milestone: no delegated cgroup tree there |
| `--image`'s removal (now a hidden alias of the positional image) | the next release | 9.4a0: kept one release |

## Architecture

```
shipyard run -d | create + start | run (foreground) | restart     the CLI: exits when done
  recovery pass (try-locks only), then op.lock, running free, the record re-read
  the plan from the record (image by digest), then:
  container::create_planned(ROOT/<id>): plan.json, labelled holds, exit command, companion
  bridges: the veth made from outside, in /proc/<init>/ns/net, between create and start
  └─ shipyard-monitor      double-forked, a session of its own; /proc/self/exe is the
       │                   binary that made the container; serves attach.sock (v3);
       │                   cgroup <base>/shipyard/monitors/<id> (9.9b), joined in
       │                   spawn_monitor's pre_exec; its children inherit it
       ├─ init → the command                       the container (cgroup shipyard/runtime/<id>)
       ├─ __health  (the companion, with a check)  started with the container, waits
       │    │                                      for it to run; killed (killpg)
       │    │                                      before the end is recorded
       │    └─ __health-check (one per check)      exec into the container; SIGKILLed
       │                                           at the timeout
       └─ at the end: exit record (v2), then the exit command, not waited for
          __cleanup --id …   setsid; its holds at 3.. (SHIPYARD_HOLDS, SHIPYARD_HOLD_LABELS)
            waits for the monitor's PID to go; releases each hold (leases under their
            network's lock, ports, run directory, volumes, layer); records the end;
            deletes the runtime state; closes `running` last
            then, if the policy says so: takes `restarting`, waits out the backoff,
            tries op.lock and `running`, and starts the container again (a new monitor,
            born in the same monitors/<id> cgroup, a new __cleanup)

  pasta (rootless): __shipyard-pasta, double-forked, a session of its own, in the
       monitor's cgroup (monitors/<id>, 9.9c); follows the init's pidfd; holds a copy of `running` until pasta is reaped;
       pasta ending while the init lives → the engine's hook: network-lost, an event

shipyard stop | wait | network connect | disconnect     a control client on attach.sock
shipyard kill                                            runtime::kill: no lock taken

__shipyard-dns (root, one per bridge network with DNS)   cgroup shipyard/dns/<net>-<hex>
  the watchdog: sockets bound, root given up, holds dns.lock
  └─ the worker: serves; started again after a backoff when killed or failing

contrib/systemd: shipyard-recover.service   ExecStart: system recover   ExecStop: system stop
```

**The processes.** Every one is Shipyard's own binary started again with a hidden first
argument (`__shipyard-monitor`, `__cleanup`, `__health`, `__health-check`,
`__shipyard-pasta`, `__shipyard-dns`, `__shipyard-storage`), so `shipyard`'s `main` calls
each entry point first, in CLAUDE.md's order. The monitor and pasta's launcher double-fork:
no caller keeps a child (or a zombie) of theirs, and a caller that is a subreaper is warned
about. The exit command is `/proc/self/exe __cleanup …` in the monitor: the binary that made
the container, even after an upgrade or a delete on disk, so `__cleanup`'s arguments never
cross versions. Every child Shipyard starts gets an empty signal mask (9.3b2a's finding).

**The monitor's own cgroup** (9.9b, found by the manual milestone). Each container's
monitor runs in a leaf of Shipyard's own, `<base>/shipyard/monitors/<id>`: `<base>` is
`cgroup::Tree::find()`'s, `/` for root, the delegated `user@UID.service` subtree for a
rootless user who has one. `create` makes the leaf (or reuses it: a policy restart's new
monitor is started from inside the old one's), and the monitor's first process joins it in
`spawn_monitor`'s `pre_exec` (one async-signal-safe `write` of `0` to its `cgroup.procs`, as
the DNS server's start does). Its health companion and its `__cleanup` inherit it; the
container's init is still born into `shipyard/runtime/<id>`. A rootless container's pasta
launcher joins it too (9.9c), first thing, so a logout or a stopped unit doesn't take the
container's network. An empty leaf that's there is removed and made again rather than
reused (another `create`'s sweep could remove it between its opening and the join); one
with a process in it is kept. Outside every login session's
scope and every unit's cgroup, the monitor is ended at shutdown only after the boot unit's
`system stop`. `MONITOR_PARENT` (and `DNS_PARENT`) are among the parents `is_container_cgroup` refuses,
so a bundle's `cgroupsPath` can't name a cgroup below them: refused at `create` (9.9c). `rm` and `shipyard-runtime delete` remove the leaf
(`remove_monitor_cgroup`); one left from inside (a `--rm` container's own `__cleanup`) is
swept by a later `create`, which removes empty leaves older than 60 s
(`cgroup::remove_empty_left_over`, shared with the DNS servers). Where there's no tree (a
rootless user logged in over SSH, with no delegation: F51), or the leaf can't be made, the
container starts anyway, and `create` warns that its monitor stays in the caller's cgroup.

**Records and their versions.** The engine's store (`/var/lib/shipyard/containers`,
rootless `~/.local/share/shipyard/containers`, `SHIPYARD_CONTAINER_DIR` for tests) holds one
0700 directory per container, named by ID: `record.json` (0600: it holds the environment),
`logs/<N>` per start (numbered from `0000000001`, so they sort by start whatever the clock
does; every start keeps only the latest 10, `prune_before_a_start`, 9.9c), `cleanup.log`,
`health.log`, `previous-start.json` while a start is in flight (what the last start left:
`started`, `finished`, `exit`, kept aside before the record says it started, so a start
killed half way is put back by the next recovery pass, 9.9c), and the lock files. A record appears whole (written
under `.new-<id>`, synced, renamed into place) and goes whole (renamed to `.gone-<id>`);
updates replace `record.json` in place (temporary file, `fsync`, `rename`, `fsync` of the
directory). Within a version the shape is fixed: a new field means a new version, and the
reader keeps the old ones (H2).

- **Record v1** (9.2b): ID, serial, name, created, the request as asked (`Config`, its own
  wire types), labels, started, finished, exit.
- **v2** (9.4a): the image's digest, `--rm`, the ports bound at the last start.
- **v3** (9.5a): `stopped_by_user`, `restart_count`, `boot_id`, `next_start`; amended while
  unreleased with `restart_delay_ms` (9.5b). `Config.restart` and `Config.health` are the
  two `#[serde(default)]` exceptions, documented.
- **The runtime's files:** `state.json` v1 (stage 8's read as v0), `exit` JSON v1 (9.2a),
  v2 with `started` (9.4a; stage 8's text read as v0), `plan.json` v1 (read only by the
  monitor its own `create` starts), `exec.json` and `log.json` v1; markers whose format is
  their existence (`killed`, `gate-opened`, `.host-stopping`, and `exit-command-failed`,
9.9c: the monitor couldn't start its exit command after a few tries, so the recovery pass
warns that the end was settled late and the policy not applied).
- **Per start:** `health.json` v1 (the status, the last five results), `network-lost` v1.
- **The image store:** a container's own writable layer `ctr-<id>` (its store's `owner`,
  removed only by `rm` or `--rm`), and a pin `container:<owner>:<id>` that `rmi` and
  garbage collection respect.

**The locks, and their order** (`containers/mod.rs`): a container's `op.lock` (taken by
`start`, `rm`, `stop`, `rename`, `update`, `connect`, `cp`), then its `running`
(held by a start's monitor and its `__cleanup` until all of that start is let go), then the
image store's lock, then the container store's `.lock` (briefly). `__cleanup` takes neither
of a container's. `restarting` is outside the order: its holder only tries the others, and a
holder of the others only tries it. `rename` adds the networks' locks, in name order, between
`op.lock` and the store's. Readers (`ps`, `inspect`, `list`, `find`) take no lock: a record
is whole or absent, and the runtime's status is peeked without its lock. The recovery pass
and the restart waiter only *try* locks, in the same order, so they never wait on a command.

**The monitor's control protocol** (`attach.sock`, `SO_PEERCRED`: the user or root):

- **v1** (stage 8): attach clients, 16 × 256 KiB.
- **v2** (9.4b1): control clients that never get output: waiters (told `Exited`) and
  stoppers (one `Stop { signal, timeout }`), each capped at 16 apart, so waiters never keep
  a stop out. The monitor owns the escalation: the signal, then SIGKILL at the earliest
  deadline, so a stop outlives its caller. A foreground run's own client says
  `HelloHeld` (9.4c3a): it is never dropped; while it's behind, the monitor stops reading the
  container's output instead.
- **v3** (9.7b1): holders (`HelloHolder`, then one `Hold { label }` with a descriptor over
  `SCM_RIGHTS`, or `Release { label }`, answered by `Done { ok, why }`). Every client is read
  with `recvmsg(MSG_CMSG_CLOEXEC)` with room for 253 descriptors, so control data is never
  cut and every installed descriptor is seen; one from a non-holder drops the client.
  Every hold has a label (`lease:app`, `port`, `run-dir`, …, from `create` or later), at
  most `MOST_HOLDS` (2,100 since 9.9a: a whole 1,024-port range on both families; the
  caller's descriptor limit, which the monitor inherits, is the other bound), and
  labels of at most 96 KiB in all (`MOST_HOLD_LABEL_BYTES`). A run is refused before
  anything is made unless every hold it will give fits both bounds (`check_holds`, 9.9c):
  its published ports' sockets (two per port on every address, `-P`'s too), a lease per
  network, a hold per volume and four of its own (the run directory's two, the writable
  area, `running`), within `MOST_HOLDS` and the soft descriptor limit less 64;
  `__cleanup` gets them all with `SHIPYARD_HOLD_LABELS`. An older monitor refuses a holder:
  "restart it to connect networks".

**The events journal** (9.6a): `events.log` and one rotated `events.log.1` beside the
store (`/var/lib/shipyard/events`, `~/.local/share/shipyard/events`, or
`SHIPYARD_EVENTS_DIR`). Each event is one versioned JSON line (v1, at most 4 KiB, Docker's
types and actions), appended with one `O_APPEND` write under a shared `flock` on
`events.lock`; past 1 MiB, an appender that gets the exclusive lock without waiting
rotates. Events carry their own time (a die is dated by its exit record), so readers filter
by time and keep write order. Writing never fails a command: it is warned about once.

**Health** (9.6b): the runtime gained a generic companion, an OCI-free counterpart of the
exit command, started by the monitor once the container is created and killed (its process
group) before the end is recorded. The engine's companion is `__health`: it reads its check
from the record by ID, waits for the start, and every interval runs one `__health-check`
(its own process group, a pipe for the first 4 KiB of output, killed at the timeout).
Docker's rules decide `starting`, `healthy` and `unhealthy`; each change is a
`health_status` event. An image's `HEALTHCHECK` (and `SHELL`) is merged with the flags at
create, so the record holds one settled check.

**Live networks** (9.7). `network connect` takes the container's `op.lock`, writes the
record first, then: the network's lock, an address and the next free `eth{n}`, the veth made
in the running container's netns (`network_namespace_live`, start time checked), its
interface sysctls, and the lease's descriptor handed to the monitor (`Hold`). `disconnect`
releases the lease under the network's lock, removes its file, then `Release`; the network
that carries the default route (read from the netns) needs `--force`. The firewall's table
went from ruleset v6 to v7 (an `input` chain for `--isolate-host`, the metadata addresses
dropped) and v8 (the per-container connection limit: dynamic sets `conns4`/`conns6` of at
most 65,536 sources, `dynset` with `connlimit`). A table of a newer version is never rebuilt
by an older binary (`Ruleset::Newer`).

**Recovery, boot and `system stop`** (9.5). A quick pass at the start of `create`, `run`,
`start`, `restart` and `rm` settles what nothing watched: a runtime state ended or dead, or
a start never finished with no runtime state (dated by the boot time when `boot_id`
differs), a crashed `--rm` container removed, a dead restart waiter's `next_start` cleared.
`system recover` (the boot unit's `ExecStart`) removes the `.host-stopping` marker, runs the
pass once, then starts what the policies say. `system stop` (`ExecStop`) writes the marker
in the runtime root first, then stops every up container in parallel without marking it
stopped by its user, so `unless-stopped` comes back; while the marker exists, no policy
fires, and `doctor` says so.

## Decisions and why

**Made with the user, before building each step (asked, 2026-10-12 to 2026-10-15):**

- **D9.1-1 to D9.1-4, the extraction** (2026-10-12): split by layer (images and volumes,
  then network, then run); warnings through a `Reporter` sink the caller passes; one engine
  `Error` with a kind, the sources kept in the chain; `--json` later, with the record. No
  behaviour change, proved by the black-box suites and a differential corpus against the
  previous binary.
- **D9.1c-1 to D9.1c-3, D9.1c2-1:** run split into planning and launching; `RunRequest` a
  flat mirror of the flags; a supervise failure reported when it happens, the engine owning
  the cleanup end to end, so stderr's order stays.
- **D9.2-1 to D9.2-4, the state model:** the runtime's files first, then the engine's
  record; one JSON file per container, atomically replaced, a store-wide `flock` for writers
  and none for readers (not SQLite: it fits daemonless and today's code); 64-hex IDs, a
  random ISO 6346 serial drawn under the lock; Docker's name rule, and generated ship names.
- **D9.2a-1 (asked after the Murphy review):** OCI `state` prints `stopped` during the
  stopping window. OCI's `stopped` is "the process has exited", and Podman and containerd may
  reject a word they don't know; `stopping` stays internal.
- **D9.2b-1 to D9.2b-3:** the record stores the request as asked, re-resolved at every
  start (Docker's Config and HostConfig), not the resolved bundle; lookup in order: full ID,
  name, serial, unique prefix; a library first, commands in 9.4.
- **D9.3-1 to D9.3-4, holds:** the network set up between `create` and `start`, through
  the init's `/proc/<pid>/ns/net` (no namespace files, no pause process: rootless can't
  join those); cleanup by an exit command, Podman's model, the existing sweeps the backstop;
  pasta in a session of its own, following the init's pidfd; the runtime first, then the
  engine.
- **D9.3b-1 to D9.3b-3:** a Shipyard plan file (`create_planned`), since OCI can't say an
  image overlay root, user names or Shipyard's defaults; `__cleanup` runs the existing
  sweeps, with what they need in its arguments.
- **D9.3b1-4:** `exec` resolves a named user in the container's own `/etc`, through
  `/proc/<init>/root` with the start time checked (pulled forward from 9.4c).
- **D9.3b2-1 to D9.3b2-3:** the exit command is `/proc/self/exe __cleanup …` (never a
  path: the same binary at the end as at the start); logs beside the record
  (`<containers>/<id>/logs`, as Docker's); pasta in a step of its own.
- **D9.3b2b-1:** a double fork for pasta's launcher (and, asked during the build, the
  monitor), so no caller keeps a zombie, stage 11's API server included; readiness over a
  pipe.
- **D9.4-1 to D9.4-5** (2026-10-13): Docker's spelling first, as its own step (9.4a0),
  `--image` a hidden alias for one release; a container's writable layer kept until `rm`;
  every container run makes a record, kept in `ps -a` unless `--rm`; `--bundle` and
  `--no-isolation` runs make none (`shipyard-runtime` is the bundle tool).
- **F42 (the author's choice):** a port in `TIME_WAIT` is bound again with `SO_REUSEADDR`,
  under a lock, only when every socket on it is in `TIME_WAIT`.
- **D9.4b-1, D9.4b-2:** the monitor's control channel with `stop`, `kill`, `wait` and
  `restart`, then `rename`; renaming a running container changes its DNS name at once
  (Docker's).
- **D9.4c-1, D9.4c-2:** `logs`, `attach`, `exec`, `cp` by name first; foreground image and
  `--rootfs` runs go through the monitor (Docker's model), while `--no-image`, `--bundle`,
  `--no-isolation` and macOS runs stay in-process with no record.
- **D9.4c2-1, D9.4c2-2:** a stopped container's volumes are mounted for `cp`; a `start`
  during a copy waits on the operation lock.
- **D9.4c3-1, D9.4c3-2:** a foreground run's container is kept unless `--rm`; `-t` in a
  step of its own.
- **D9.4c3b-1, D9.4c3b-2** (2026-10-14): Docker's detach keys for `run -it`; of F37, only
  the one resize watch then, the rest at the stage's close.
- **D9.5-1 to D9.5-4:** the exit command restarts (conmon and `podman container cleanup`'s
  model), since a restart redoes engine work; one boot unit; a lock-safe recovery pass at
  the start of container-changing commands; split crash recovery, policies, reboots.
- **D9.5b-1, D9.5b-2:** a policy's restarts keep the latest 10 starts' logs (a user's
  starts keep all; since 9.9c every start keeps the latest 10); `update --restart` included.
- **D9.5c-1, D9.5c-2:** checked on a Lima Ubuntu VM (a real reboot and logouts); a
  graceful `system stop` in the units, which doesn't mark a user's stop.
- **D9.6-1 to D9.6-3:** health checks run by a monitor companion (the runtime stays
  OCI-generic); events in a journal per store; split journal, health, carry-overs.
- **D9.6b-1 to D9.6b-3:** a process per check, killed at its timeout; `unhealthy` does
  nothing but show (Docker's); the image's check in a step of its own.
- **D9.6c-1 to D9.6c-3:** a DNS server gets its own watchdog in its network's cgroup
  (replacing D9.6-4, the companions re-ensuring servers: only containers with a check have a
  companion); pasta's death is said, never restarted (Podman's).
- **D9.7-1 to D9.7-4:** a later lease is held by the monitor (protocol v3); `port` plus
  `port --sockets`; host isolation and the connection limit opt-in per network, today's
  behaviour the default (Docker's).
- **D9.7b-1, D9.7b-2:** `resolv.conf` and `/etc/hosts` are never touched by a connect: a
  container's resolver is the one its start chose; labelled holds first, then the commands.
- **D9.7c-1, D9.7c-2, D9.7c2-1 to D9.7c2-3** (2026-10-15): the connection limit on or off,
  one fixed limit (8,192), so the table stays fixed-rule; over it, a reject (ICMP
  admin-prohibited) so programs fail at once; the test uses real flows, no hidden knob;
  every connection a container opens counts, to the host too.
- **D9.8-1 to D9.8-4:** F36 refuses any mount whose resolved place is in `/proc`, `/sys` or
  `/dev` while its destination as written isn't; the `Quality` workflow manual with its
  schedule written; cargo-mutants on a branch's diff, as a report that never fails the run.
- **D9.9-1 to D9.9-5:** no closing book chapters, glossary pass or recheck of earlier
  chapters for stage 9 (chapters 36 to 51 were written with their steps; in 9.9b chapter
  48 gained a section on the monitor's own cgroup and 51 was updated); x86-64 in a Lima VM
  under QEMU; the close split a/b/c; F37's rework before the reviews; the differentials to
  stage 14.
- **9.9a's review decisions:** the holds' cap raised (to about 2,200: built as 2,100, a
  whole 1,024-port range on both families) with a refusal before anything is made; one
  recovery pass at boot and `TimeoutStartSec=infinity`; M4 recorded for stage 10 (F43) with
  the upgrade rule written down; the clean-code review's D1–D10 now, L1–L13 a stage 10
  refactor (F48).
- **The monitor's descriptor limit (asked in 9.9b): recorded, not built.** 9.9a decided to
  raise the monitor's soft `NOFILE` to its hard limit, but the published ports' sockets are
  bound by the caller, so the caller's soft limit is what bounds them, and 9.9a's early
  refusal checks exactly that, with a `ulimit -n` hint. A raised limit would help only
  holds that `network connect` adds later, and the init, the companion and a restart's
  monitor would inherit it unless it were lowered around each: F49, stage 10.
- **D4 (asked):** a reader gone (a closed pipe) ends any command quietly, with 0.
- **D9.9-6:** the release is v0.2.0: the CHANGELOG's `## Unreleased` became `## 0.2.0 -
  unreleased`, matching 0.1.0's heading, and the workspace version 0.2.0; tagging is the
  user's.
- **D9.9-7:** the manual milestone on all three VMs (Ubuntu arm64, Fedora arm64, Ubuntu
  x86-64 under QEMU); QEMU and `lima-additional-guestagents` installed by Homebrew (kept),
  every VM deleted after.
- **D9.9-8:** no `--mutants` sweep of 9.9a: stage 9 is uncommitted, so there is no diff of
  9.9a alone, and every 9.9a behaviour change has hand-picked mutants instead.
- **The monitor's own cgroup (asked in 9.9b): fixed now,** not carried: root's containers
  made from a login session were ended at shutdown without their stop signal.
- **The resource and image commands by name (asked in 9.9b):** stage 10's first steps.
- **The forced logout (asked in 9.9b):** accepted and recorded as observed (`loginctl
  terminate-user` ends even a lingering user's containers; the user unit brings them back).

**Made in the steps:**

- **Paths are explicit** (9.1): `Paths::from_env` is the only reader of the stores'
  variables; the clap types stay in the CLI, the engine takes plain types.
- **Another crate's error is found with `Error::find`** (9.1a's finding: a foreign type is
  hidden from `downcast_ref`), so the two type checks that relied on it moved first.
- **Holders unlock on drop unless given up** (9.3b2a): nix's `Flock` would release the
  monitor's copy at drop (`LOCK_UN` on any copy releases the open file description), and
  std's close-only locks let a stray fork's copy keep an area alive. The hand-over is the
  only path that skips the unlock.
- **`__cleanup` waits for the monitor's PID** (`SHIPYARD_MONITOR_PID`) to go before it
  releases anything: the monitor keeps its copies until it exits.
- **No `logs` field in record v1** (its shape is fixed): start directories are numbered.
- **A failed start leaves a `Created` record** (Docker's `run -d`); a failure before
  `create_planned` removes it (a guard); SIGINT, SIGTERM, SIGHUP and SIGQUIT are held off
  from the record's making to the start.
- **The runtime ID is the container's 64-hex ID; the run ID stays random per start**
  (plan review): one derived from the container would reuse a veth name, and a late sweep
  could delete the live one.
- **Pasta's launcher holds a copy of `running`** (9.4a, found by the rootless manual
  check), so a container's end means its ports are free too.
- **The op-lock re-check stays** (9.4a): a mutant showed a `start` without it opens
  `running` in a directory an `rm` removed.
- **Dropped from 9.4b1's plan: the engine's own escalation** for an unreachable monitor: a
  killed monitor takes its init (parent-death signal).
- **Leases know their container** (9.4b2) and are rewritten in place under an OFD
  byte-range lock every lease reader takes; the run's `flock` on the inode stays its
  liveness.
- **`logs -f` follows the newest start until the container stops** (9.4c1), not into its
  next start: that worked only within the restart's window.
- **A copy into a `--read-only` root fails with the kernel's `EROFS`** (9.4c2); names in
  `cp -a`'s headers dropped (numeric, as `tar --numeric-owner`).
- **A foreground run's SIGTERM is the container's stop** (9.4c3a), its other signals go
  raw; the CLI exits with the recorded code and dies by a forwarded SIGINT, so shell loops
  stop.
- **`--rm` with a restart policy is refused; Docker's backoff** (100 ms doubling to a
  minute, reset after 10 s of running) **with an `Instant` deadline**, so a clock set back
  doesn't stretch it.
- **The image's health check is merged at create**, not per start (Docker's): the image is
  pinned by digest, so the answer is the same, and the companion reads one settled value.
- **DNS restarts go to `dns.log`, not the journal** (9.6c2): the watchdog runs as nobody
  and can't write root's journal.
- **A separate `HelloHolder`** (9.7b1) rather than a role byte: a v2 monitor drops an
  unknown kind cleanly.
- **`ct status ! confirmed`, not `ct state new`** (9.7c2): only a flow's first packet is
  counted; TCP over the limit gets a reset, others ICMP.
- **D1 rejected** (9.9a): marking a container stopped by its user only after `kill`
  succeeds would let a SIGKILLed container's `__cleanup` restart what the user killed. Kept
  as it is; the narrow race left is F47.
- **D3, narrower than the review asked:** `WrongStatus` with the container stopped or
  stopping is `NotRunning`; running when it mustn't be stays `Runtime` (mapping all of it to
  `NotRunning` would mislabel `delete` and `start`). `MonitorTooOld` is `Unsupported`,
  `BadHoldLabel` `InvalidRequest`, attach's `Refused` `Busy`.

## Kernel checks

`scripts/probes/network.c` (9.7c2, as root on the test container, Linux 7.2):

| | Question | Result |
|---|---|---|
| N10 | A dynamic set bounded by `NFTA_SET_DESC`; `dynset` with a nested `connlimit`, as one expression (`NFTA_DYNSET_EXPR`) and as a list (`NFTA_DYNSET_EXPRESSIONS`); `reject with icmpx admin-prohibited` | all load; Shipyard sends the single form, which kernels before 5.11 take too |

**Checked by hand while building** (no probe files: 9.4a's `SO_REUSEADDR` checks were done by hand too):

| | Question | Result |
|---|---|---|
| 9.2a | how long the stopping window lasts | too brief to catch by polling `state`; the end-to-end test makes one by hand |
| 9.3b2a | a signal mask across `fork` and `exec` by std's `Command` | kept: the monitor and `__cleanup` had `SigBlk` `0000000000004003` (HUP, INT, TERM) from the CLI's hold; a child Shipyard starts now gets an empty mask |
| 9.4a | a socket bound with `SO_REUSEADDR`, then cleared | as exclusive as a plain one: every later bind or listen on the port refused, on any address. Bound-only sockets aren't in `/proc/net/tcp*`; one bound with reuse beside the held port can't listen |
| 9.6c1 | a namespace's init, killed | can read as running while it ends the rest: pasta's loss is said only after a 200 ms grace |


**On a Lima Ubuntu 24.04 VM** (9.5c, D9.5c-1; deleted after):

| | Question | Result |
|---|---|---|
| V1 | a reboot, root's boot unit enabled | `always` and a running `unless-stopped` back; a stopped `unless-stopped` and a `no` not |
| V2 | a reboot, a rootless user with linger and the user unit | the same |
| V3 | where the monitors run | in 9.5c, the unit's cgroup: `system.slice/shipyard-recover.service`; `user@501.service/app.slice/shipyard-recover.service`. Since 9.9b, `shipyard/monitors/<id>` below the tree's base, whoever started them (checked on all three hosts) |
| V4 | a container that can't start at boot (its bind source removed) | the failure in the journal; with `SuccessExitStatus=125` the unit stays active and the others run |
| V5 | `systemctl reboot`, `systemctl stop` of the unit | `ExecStop` ran: a TERM trap wrote its file, `Exited (0)`/`(143)`, both back at boot. `limactl stop` powers off without a clean shutdown (no `ExecStop`) |
| V6 | a rootless logout over SSH, `KillUserProcesses=no` (Ubuntu's) | the container runs on: the session stays "closing", `/run/user/UID` and the user manager kept, linger or not |
| V7 | `KillUserProcesses=yes` | killed even with linger (the monitor in `session-N.scope`); one started under `systemd-run --user --scope` (linger) survives. Since 9.9b, a rootless monitor made from an SSH login is still in the session's scope (no delegated tree: F51) |
| V8 | root through `sudo` from a user's SSH session | in 9.5c, in that session's scope: killed by `yes` (`KillExcludeUsers=root` spares only root's own sessions), and at shutdown before `system stop` (found in 9.9b); `sudo systemd-run --scope` survives. Since 9.9b, in `/shipyard/monitors/<id>`: stopped gracefully at a reboot, no "nothing recorded its end" warning |
| V9 | logind's setting, live | `busctl get-property … KillUserProcesses` gave `b false` / `b true` |

**Sizes** (9.6b1, 9.6c2): an idle health companion's anonymous memory is under 2 MiB
(`smaps_rollup`, the budget in `a_passing_check_is_healthy`). A DNS server's cgroup gets
`memory.max` 64 MiB and `pids.max` 136 (`MOST_TASKS` + 5), checked by hand.

## macOS and Linux differences

- The container store, records, IDs, names, labels, the events journal (and `shipyard
  events`), health's status machine, the restart decision, `steered` and the attach
  decoder are `cfg(unix)` and tested on macOS. Containers (`create`, `run -d`, `start`,
  `stop`, recovery, `boot`, `connect`, the health runner) are Linux's; elsewhere the
  engine fails with `Unsupported` ("containers need Linux", `error::linux_only`).
- On macOS a foreground run stays in-process, with no record (D9.4c-2), as `--no-image`,
  `--bundle` and `--no-isolation` runs do on Linux.
- `doctor`'s `linger`, `logout`, `restart at boot` and `restarts` lines appear only where
  systemd runs; `contrib/systemd` is Linux's.
- The `Quality` workflow's `mutants` job runs on Ubuntu and macOS; `fuzz` on Ubuntu.
- On Apple silicon a quadratic attach decoder got through 9.8a's first time budget (1.84 s
  for 1 MiB of small frames, under 2 s): the test now asserts that the bytes taken stay
  until the next push, and floods 4 MiB, rather than relying on time alone.

## Failure-mode register

| Case | Handling | Test |
|---|---|---|
| A helper that answered and exited between a `poll` timeout and `try_wait` had its answer taken for none (9.1b, a race from stages 3, 6 and 7, found by a flaky full run) | once the process is gone, its pipe is read to its end; the exit status kept in "ended without answering" | `an_answer_a_gone_helper_left_in_its_pipe_is_read`, `what_a_gone_server_said_is_read`, `an_answer_a_gone_monitor_left_in_its_pipe_is_read` |
| An API caller's request with an image and no isolation ran on the host; `cap_drop` without isolation ignored (9.1c1's clean-code review) | `RunRequest::validate` mirrors clap's rules, every field destructured | `a_request_that_cant_be_carried_out_is_refused` |
| A request's `Debug` printing the environment's values (9.1c1's Murphy review) | values redacted | `a_request_shows_no_environment_values` |
| `exec`, `cp` and `kill` called a finishing container `running` (F39) | `Stopping`, derived from the init's and the monitor's liveness, never stored | `status_tells_stopping_and_none`, `a_stopping_container_is_said_to_be` |
| A newer `state.json`, `exit` or record misread (9.2a, 9.2b) | refused by kind (`UnknownVersion`, `Storage`); an unknown `exit` version still says stopped, without a made-up code | `a_newer_state_json_is_refused_by_kind`, `a_newer_state_record_is_refused`, `a_newer_exit_record_gives_no_code` |
| A downgrade to stage 8 reading a v1 or v2 `exit` (9.2a's Murphy review) | the code kept in `description`'s text | `stage_8s_reader_finds_the_code_in_a_version_2_record` |
| `killed` made by path with the umask's mode (9.2a) | made through the locked directory, `O_NOFOLLOW`, 0600 | `the_killed_marker_follows_no_symlink` |
| `shipyard-runtime create runs` colliding with `shipyard run`'s directories (9.2a) | `ROOT/.runs`: a name no ID can be | `runs_in_the_runtime_root_are_no_container` |
| `create` treating an unreadable record's name and serial as free (9.2b, both reviews) | writers read strictly and stop; readers skip and warn | `an_unreadable_record_stops_a_create`, `an_unreadable_record_is_left_out_and_said` |
| Two creates at once; a reader during writes; a writer killed half way (9.2b) | the store's lock; publish by `rename`; `.new-`/`.gone-` swept | `concurrent_creates_keep_names_and_serials_unique`, `a_reader_never_sees_half_a_container`, `leftovers_are_never_listed_and_are_swept` |
| A name or serial two records share (9.2b's review) | `find` refuses, naming at most 10 | `a_name_or_serial_two_records_have_is_ambiguous` |
| `F_DUPFD`'s lowest free numbers taken as consecutive: with `--log-dir` the wrong descriptors reached 3; the shuffle after the fork could hit std's exec-error pipe (9.3a, both reviews, high) | the descriptors arranged before the fork; `pre_exec` only `setsid`s and clears close-on-exec | `holds_reach_the_exit_command_in_order`, `a_hold_outlives_create_and_is_the_exit_commands` |
| A hold leaking into the container, or kept by a killed monitor; an exit command after a failed `create` (9.3a) | holds close-on-exec in the monitor; a SIGKILLed monitor's go with it; no exit command before `Created` | `holds_go_with_the_container_or_its_monitor`, `the_exit_command_runs_after_delete_and_not_after_a_failed_create`, `bad_holds_and_exit_commands_are_refused` |
| `exec` into a plan with user names always failed (9.3b1's Murphy review, high) | names resolved in the container's own `/etc` (D9.3b1-4) | `exec_runs_as_a_planned_user_by_name` |
| A plan without a root ran on the host's files; a FIFO at `plan.json` stalled the monitor (9.3b1's reviews, the book agent) | `PlanError::NoRoot`; `plan.json` opened `O_NONBLOCK\|O_NOFOLLOW`, regular files only | `a_plan_is_checked_as_a_bundle_is`, `a_plan_behind_a_symlink_is_not_read` |
| nix's `Flock` releasing the monitor's copy at drop; a parallel thread's fork keeping a dropped area alive (9.3b2a, found by the image crate's test) | std locks that unlock on drop unless given up | `a_lease_handed_over_outlives_its_holder`, `an_area_handed_over_outlives_its_holder`, `a_dropped_area_is_stale_despite_a_stray_copy`, `a_dropped_lease_is_free_despite_a_stray_copy` |
| `__cleanup` racing its monitor's copies of the holds; then waiting its whole 10 s because `getppid()` was already the reaper (9.3b2a, a flaky e2e run, then the manual check) | the monitor passes its PID; `__cleanup` waits only while its parent is that PID | `a_detached_container_outlives_its_caller_and_is_cleaned_up` (the end recorded within 5 s) |
| The monitor and `__cleanup` deaf to SIGHUP, SIGINT and SIGTERM, their mask inherited from the CLI's hold (9.3b2a, found by the book agent) | every child Shipyard starts gets an empty mask | `a_detached_container_outlives_its_caller_and_is_cleaned_up` (`SigBlk` 0) |
| A Ctrl-C between `create` and `start` stranding a created container with its lease and ports (9.3b2a's plan review) | signals held off from the record to the start; one pending before `create` undoes everything | `a_ctrl_c_before_create_undoes_everything`, `a_ctrl_c_between_create_and_start_waits` |
| The binary deleted or upgraded under a running container (9.3b2a) | the exit command is `/proc/self/exe` | `an_upgraded_binary_still_cleans_up` |
| A monitor killed -9 (9.3b2a) | its holds go with it; the next run's sweeps reclaim; the next `start` or `rm` reconciles | `a_monitor_killed_leaves_what_the_next_run_sweeps`, `a_start_after_its_monitor_was_killed_works` |
| A hold that isn't what its slot says; a run directory swapped (9.3b2a) | refused alone, closed and named; removed only if still the one locked | `holds_are_sorted_and_checked`, `a_kind_fits_only_its_file_type`, `the_run_dir_goes_only_if_it_is_the_one_held` |
| Pasta's launcher's private mount copy pinning host filesystems for the container's life (9.3b2b's Murphy review) | its mounts are slaves, its working directory `/` | `a_pasta_launcher_pins_no_host_mount` |
| Pasta's launcher killed under SELinux (`pasta_t` clears the parent-death signal) (9.3b2b) | no `--no-netns-quit`: pasta quits when `__cleanup` removes the netns file | `a_pasta_whose_launcher_was_killed_ends_with_the_clean_up`, `a_detached_pasta_quits_with_its_namespace_file` |
| The init dying during pasta's start-up; a PID reused before the pidfd (9.3b2b) | one poll loop from pasta's fork, every exit kills pasta first; the start time checked after the pidfd is open | `the_init_killed_while_pasta_starts_ends_it`, `a_detached_pasta_for_an_ended_init_is_refused`, `a_created_containers_init_can_be_followed` |
| A monitor's PID unpinned after the double fork (9.3b2b's Murphy review) | the monitor sends its own start time; a PID alone is refused | `a_monitor_without_its_start_time_is_refused` |
| A `start` that opened `running` in a directory an `rm` had removed (9.4a, found by a mutant) | the record re-found by ID under the op lock | `start_and_rm_never_interleave` |
| A rootless container's end with pasta still holding its ports (9.4a's manual check) | the launcher holds a copy of `running` until pasta is reaped | `its_pastas_ports_are_free_once_its_end_is_settled` |
| `start` refused for about 60 s after a client used a published TCP port (F42, 9.4a's manual check) | bound again with `SO_REUSEADDR` when every socket on it is in `TIME_WAIT`, under a lock | `a_port_closed_connections_wait_on_is_held`, `its_pastas_ports_are_free_once_its_end_is_settled` |
| A CLI killed between `started` and `start`, read as `Exited (137)` (9.3b2a's Murphy F5) | the exit record v2 says whether the gate opened | `an_end_before_the_gate_records_no_exit` |
| Two container stores on one image store sweeping each other's layers (9.4a) | areas carry their store's `owner` | `two_container_stores_share_an_image_store` |
| An image removed under a stopped container (9.4a) | a pin per container; `rmi` refuses, `rmi -f` untags | `the_image_stays_with_its_container`, `rmi_refuses_a_containers_image` |
| `rm` dropping its op lock early (a `..` pattern, found by the book agent, 9.4a) | kept to its end | reviewed; its mutant survives, masked by `running` (defence in depth) |
| A `stop` whose CLI is killed mid-wait (9.4b1) | the monitor owns the escalation | `a_stop_killed_meanwhile_still_ends_it` |
| Real-time stop signals never sent (nix's `Signal` enum) (9.4b1's clean-code review) | sent by number | `a_real_time_stop_signal_arrives` |
| Waiters keeping a stop out; a long stop's op lock blocking a shorter one (9.4b1's Murphy review) | waiters and stoppers capped apart; a stop asks the monitor before waiting for the op lock | `waiters_never_keep_a_stop_out`, `a_shorter_stop_goes_past_a_longer_one` |
| An attach client or a waiter sending `Stop` (9.4b1) | not the protocol: dropped | `an_attach_client_cant_stop_it`, `a_control_client_sees_no_output` |
| The monitor serving no client while it waits for its lock at the end (F41) | it serves between tries | `the_monitor_serves_while_it_waits_at_the_end` |
| A lease rename read torn by the DNS server or a firewall rebuild (9.4b2's Murphy review) | written in place under an OFD write lock that readers take shared | `a_lease_is_renamed_in_place`, `a_lease_being_written_unsettles_a_read` |
| `rename` accepting dots and 128 characters, not a DNS label (9.4b2, found by the book agent) | `--name`'s rule | `rename_gives_a_container_a_new_name` |
| A whole stderr line hiding an open stdout line between starts (9.4c1, found by the book agent) | one open slot per stream | `a_start_never_runs_on_into_the_next` |
| `start -a` attaching after the gate (9.4c1, a surviving mutant) | attached before the gate; a late attach is warned about (`AttachedLate`) | `start_a_misses_nothing` |
| A live copy into a container that ends, started again under the helper (9.4c2's Murphy review, high) | the live path holds the op lock for the whole copy | `a_start_waits_for_a_copy_into_one_that_ended` |
| A start mounting a layer a stopped copy has mounted (9.4c2) | the op lock and the layer's lock held for the copy | `a_start_waits_for_a_copy` |
| The helper's user-namespace handshake deadlocking (9.4c2's reviews) | a 10 s wait, then the helper killed | `a_byte_is_waited_for_and_never_forever` |
| A removed volume made again by a copy; a created container's volume never made (9.4c2's Murphy review, the book agent) | existing volumes only, except for a never-started container, as its first start would | `cp_reaches_a_stopped_containers_volumes`, `cp_into_a_created_container` |
| `cp -a` right after a short container's start: "kept starting and ending" (9.4c2's manual check) | the live branch waits for the end to settle | `cp_as_a_container_ends` (a briefer window can't be held open: its mutant survives) |
| A stopped copy's mounts reaching the host; `userxattr` dropped (9.4c2) | `MS_PRIVATE`; the start's own overlay options | `cp_reaches_a_stopped_container` (mountinfo), `a_views_overlay_is_mounted_as_its_starts` |
| A foreground run's client dropped: output lost (9.4c3a's Murphy review) | `HelloHeld`: never dropped; the monitor stops reading the container's output while it's behind | `a_slow_run_misses_nothing`, `input_ends_at_your_end_of_file` |
| A foreground run's stdout closed (9.4c3a) | SIGPIPE to the container | `a_closed_output_ends_it_as_a_pipe_would` |
| Ctrl-C swallowed while the run is blocked; a shell loop not stopped by it (9.4c3a) | a forwarding thread; the CLI dies by the forwarded SIGINT | `a_forwarded_signal_ends_it_with_its_code` |
| The CLI killed or its terminal gone (9.4c3a) | the container runs on, as a detached one | `the_container_outlives_its_run` |
| The signal mask left blocked after the watch (9.4c3a's reviews) | saved before, restored after | reviewed; its mutant has no test |
| `-it` without a terminal accepted (9.4c3b's manual check) | refused before anything is made | `a_terminal_foreground_run_needs_a_terminal_to_type_on` |
| A container ignoring SIGHUP leaving `run -it` lingering after a hang-up (9.4c3b's Murphy review) | `run` exits 129; the container runs on | `a_hangup_ends_a_terminal_container_run`, `a_hangup_leaves_a_container_ignoring_it_running` |
| A second SIGHUP from the ending session leader, after the watch was dropped (9.4c3b, the second full run) | SIGHUP ignored while still held off | `a_hangup_ends_a_terminal_container_run` (8 of 8 repeated runs) |
| Keys typed after `bg` then `fg` read cooked under real bash (9.4c3b, found by the book's Try-its) | no stdin reads while raw mode is due; SIGCONT wakes the session | `a_terminal_container_run_brought_back_after_bg_goes_raw`; the bash case only by hand (the fake shell keeps the terminal canonical) |
| `start` or `restart` of a crashed `--rm` container bringing it back (9.5a's reviews) | recovered first, its own record included | `a_crashed_rm_container_isnt_started_again` |
| A concurrent `start` wiping a user's stop mark (9.5a) | marked before asking the monitor and again under the op lock | `a_users_stop_is_recorded_and_a_start_forgets_it` |
| Recovery touching a container a command or monitor is on (9.5a) | try-locks only, op lock then `running` | `recovery_leaves_a_busy_container_alone` |
| A reboot told from a killed monitor (9.5a) | `boot_id`; dated by `/proc/stat`'s `btime` | `a_reboot_is_told_by_the_boot_id`, `the_boot_time_is_proc_stats_btime` |
| A restart waiter clearing another restart's schedule; a start while restarting (9.5b's reviews) | it clears only its own `next_start` | `a_start_while_restarting_takes_over` |
| A dead restart waiter making `rm` refuse (9.5b) | a free `restarting` lock means its `next_start` is stale | `a_killed_restart_waiter_is_settled`, `rm_refuses_a_restarting_container` |
| `update` honoured only for `no` (9.5b) | the waiter decides again each step | `update_to_no_ends_the_restarts` |
| A clock set back stretching a backoff (9.5b) | an `Instant` deadline | reviewed; no test sets the clock |
| A oneshot unit whose `ExecStart` fails having systemd kill every container it started (9.5c's Murphy H1) | `SuccessExitStatus=125` | the 9.5c VM (V4); no automated test |
| A shutdown restarting what it stops, or marking it the user's (9.5c's Murphy H2) | `system stop`, `WhenUp::HostStop`, the `.host-stopping` marker | `the_host_stop_is_graceful_and_not_a_users` |
| `reconcile` settling an end without saying it (9.6a's Murphy H1) | it emits the die | `an_end_its_clean_up_missed_is_said_once` |
| `rm -f` writing no `die`; `rm` of an end unwatched writing none (9.6a, the book agent and the manual check) | a kill the monitor accepted is left to it; `settle_unrecorded_end` shared | `rm_f_says_kill_die_destroy`, `rm_of_an_end_unwatched_says_its_die` |
| `events \| head -n1` waiting for the next event (9.6a) | the reader asked between looks; stdout polled | `new_events_are_followed`, `a_reader_gone_ends_any_command_quietly` |
| Reading the past quadratic (9.6a) | an offset, one drain | `fifty_thousand_events_are_read_in_linear_time` |
| A follower missing a file rotated in and out between looks; appends torn (9.6a) | the move noted before the drain, the middle file read; one `O_APPEND` write under a shared lock | `a_follower_reads_a_file_rotated_twice_between_looks`, `concurrent_appends_stay_whole_across_a_rotation` |
| A filter matching every type by ID prefix (9.6a) | filters by type | `filters_narrow_as_dockers` |
| A pause during a check ending checks for good (9.6b1's Murphy H1) | paused containers skipped | `a_paused_container_isnt_checked` |
| A check in flight escaping the companion's end; a zombie init read as running (9.6b1's Murphy review) | the companion killed (its group) before the reap | `a_check_under_way_at_the_end_counts_for_nothing`, `a_companion_runs_beside_the_container_and_ends_with_it` |
| A check's leftover holding the pipe (9.6b1's Murphy M1, found real by the book's Try-its) | wait for the exit, then drain at most 100 ms | `a_check_that_leaves_a_process_still_answers` |
| A timed-out `sh -c` check's children surviving (9.6b1) | a process group of its own, killed whole | `a_check_out_of_time_fails_and_leaves_nothing` |
| An image's `SHELL` ignored; a malformed image check making the image unrunnable (9.6b2's review) | the shell used; a bad image check left out with a warning | `an_images_none_or_bad_check_and_its_shell` |
| A killed session's init read as alive after pasta was reaped (9.6c1's review) | a 200 ms grace before a loss is said | `a_pasta_ending_with_its_init_isnt_a_loss` |
| A DNS server killed at logout (`KillUserProcesses`) or by the boot unit's stop (9.6c2) | its own cgroup, `shipyard/dns/<net>-<hex>` | `a_server_has_a_cgroup_of_its_own_till_its_network_goes` |
| A killed DNS worker; an orphaned worker after its watchdog's death; a failure that never ends (9.6c2's review) | the watchdog restarts it with a backoff; the worker's parent-death signal; given up after 20 quick ends | `a_killed_servers_worker_is_started_again`, `backoff_doubles_resets_after_a_minute_and_gives_up` |
| An inherited ignored SIGCHLD making the watchdog loop; a worker dying with the network's lock held (9.6c2's review) | SIGCHLD reset to the default; the lock released after a worker's end | reviewed; neither window can be reached on purpose |
| A descriptor leak on `MSG_CTRUNC` exhausting the monitor's descriptors, so `__cleanup` never ran (9.7b1's Murphy review) | room for 253; a descriptor from a non-holder drops the client | `a_descriptor_from_anyone_else_is_refused` |
| `__cleanup` refusing every hold on one unknown label, closing `running` first (9.7b1's Murphy review) | an unknown label refuses that hold alone; `running` last | `kinds_come_from_labels_or_arguments` |
| A Ctrl-C or a failed record write leaving a connected container live but unrecorded (9.7b2's review) | the record first, put back if the live step fails; signals held off | `the_record_keeps_it_and_the_way_out_needs_force` |
| A disconnect unlinking the lease before undoing the veth (a live veth and a reusable address) (9.7b2's review) | released first, then unlinked | `a_running_container_joins_and_leaves_a_network` |
| Interface sysctls breaking later starts after a disconnect (9.7b2's review) | an unmade interface's keys skipped with a warning at start, refused at create | `an_interfaces_settings_are_set_once_it_is_made`, `interface_sysctls_need_their_interface` |
| A newer table rebuilt by an older binary (9.7c1's review) | `Ruleset::Newer`: only elements added | `only_this_versions_whole_table_is_current` |
| An isolated container reaching other clouds' metadata addresses (9.7c1's review) | `169.254.0.0/16`, `100.100.100.200`, `fd00:ec2::254`, `fd00:ec2::23`, `fd20:ce::254` dropped | `an_isolated_network_reaches_the_host_only_for_its_dns` |
| `ct state new` counting every unanswered packet; IPv6 link-local spoofing filling `conns6`; ICMP rate limits (9.7c2's review) | first packets only (`! confirmed`); link-local new connections to the host refused; TCP reset | `a_limited_network_counts_each_containers_new_connections`, `a_limited_network_caps_each_containers_connections` |
| A mount steered by an image's symlink into `/proc`, `/sys` or `/dev`, or a nested place like `/dev/ptmx` (F36, 9.8a's review) | refused by its resolved place, read from `/proc/self/fd` | `a_mount_a_symlink_leads_into_a_later_standard_mount_is_refused`, `a_rootless_mount_a_symlink_leads_into_sys_is_refused`, `a_mount_led_into_dev_is_refused_and_one_written_there_isnt`, `a_mount_steered_into_a_standard_directory_is_told` |
| A flood of small frames costing the monitor quadratic work; a resize storm (F40) | decoded from an offset; one size per pass | `a_flood_of_small_frames_is_read_in_one_pass`, `a_flood_of_sizes_is_one_change` |
| `Done.why` truncation untested (9.8b, found by the first `--mutants` sweep) | a test | `a_long_reason_is_cut_to_one_frame` |
| A container publishing more ports than a monitor holds (about 29 on a dual-stack host), refused only after everything was made; foreground runs too (9.9a's Murphy H1) | `MOST_HOLDS` 2,100; refused before anything is made (`check_port_holds`), bounded by the caller's soft descriptor limit too | `a_range_of_ports_is_published_and_too_many_are_refused_first` |
| A late start failure erasing the container's history, so it read `Created` and never came back at boot (9.9a's Murphy H2) | `unstarted` restores the last start's `started`, `finished` and `exit` | `a_failed_start_keeps_what_its_last_start_left` |
| The boot unit's 5-minute start timeout killing every container it had started (9.9a's Murphy M1) | one recovery pass in `boot`; `TimeoutStartSec=infinity` | `boot_starts_what_the_policies_say`; the timeout reviewed |
| `network rm` and `prune` removing a network a stopped, restarting or waiting container still names (9.9a's Murphy M2) | refused (`InUse`) while any record joins it; unreadable records refuse | `a_stopped_containers_network_is_kept`, `a_record_says_which_networks_it_joins` |
| `system stop` racing a restart already starting (9.9a's Murphy M3) | `host_stopping` checked under both locks; a restarting container stopped too | `a_restart_waiting_for_its_lock_sees_the_host_stopping` |
| Every health check reading the whole store, warning about each unreadable record into every `health.log` (9.9a's Murphy L1) | `__health` and `__health-check` read their record by ID | `a_check_reads_its_record_alone` |
| A `system stop` run by hand silently turning every restart policy off (9.9a's Murphy L4) | `doctor`'s `restarts` warning while the marker exists | `restarts_held_off_is_a_warning`, `a_stop_that_holds_restarts_off_is_said` |
| `system stop` with no state root (rootless, nothing run since login: the user unit's `ExecStop` at every shutdown) failing "holding restart policies off" (9.9a, found by L4's test) | nothing to do: success | `a_stop_that_holds_restarts_off_is_said` |
| `cleanup.log` rotated a second time while `__cleanup` had it open (9.9a's clean-code D2) | rotated once, as each clean-up opens it | `a_restart_leaves_the_open_cleanup_log` |
| One situation, several error kinds (9.9a's clean-code D3) | one mapping per situation | `other_crates_errors_have_their_kinds` |
| A closed pipe: two commands exit 0, the rest 125 (9.9a's clean-code D4) | any command ends quietly with 0 (D4, asked) | `a_reader_gone_ends_any_command_quietly` |
| A stage 8 container running across the upgrade (9.9a, H2) | left to `shipyard-runtime`, which reads v0 files | `a_container_stage_8s_runtime_made_is_left_to_it` |
| `network rm`'s new record check (M2) seeing other tests' same-named networks: tests shared root's container store (9.9b, found on the way to the monitor's cgroup) | test-side: `NetworkDir` gives each test a private container store | `a_stopped_containers_network_is_kept`, and the `network.rs`, `networks.rs`, `dns.rs` and `connect.rs` suites |
| Root's containers made from a login session (`sudo shipyard run -d` over SSH): their monitor in the session's scope, ended by systemd at shutdown before the boot unit's `system stop`, so they died without their stop signal (9.9b's manual milestone, Ubuntu arm64) | each monitor in `shipyard/monitors/<id>`, joined in `spawn_monitor`'s `pre_exec`; a restart's monitor born in it; removed by `rm` and `delete`, empty leftovers swept at `create` | `a_monitor_lives_in_a_cgroup_of_its_own` (mutants 3/3); the reboot on all three hosts with the fix: no "nothing recorded its end" for root's |
| The boot unit enabled without `--now`, so the first shutdown wasn't graceful; `loginctl enable-linger` refused without `sudo` on Fedora (polkit); a container without a terminal seeing the detach keys only after Enter, unsaid (9.9b's manual milestone) | the docs: `contrib/systemd/README.md`, `containers.md`, `doctor`'s hints | the VM runs (no automated test: docs) |
| Tests fixed in 9.9a: one expected no end after H2's change; one read before the first clean-up was done; one asked for a 1,100-port range (the limit is 1,024) | the last start's end expected (no 137 still asserted); wait for `running` free; two ranges | `an_end_before_the_gate_records_no_exit`, `a_failed_start_keeps_what_its_last_start_left`, `a_range_of_ports_is_published_and_too_many_are_refused_first` |
| A command acting per item (`stop`, `rm`, `volume rm`, `rmi`, …) saying "Broken pipe" with 125 when its reader went away (9.9c's review) | `each` stops at a broken pipe, quietly; an earlier real failure still gives 125 | `a_reader_gone_ends_a_command_per_item_quietly` |
| A container's name, command or label (or an event's attributes) sending escape sequences or bidirectional overrides to the terminal through `ps`, the other tables or `events` (9.9c's review) | `printable`: C0 and C1 controls and the bidi and format characters `is_control` misses escaped (`\u{…}`) | `printable_escapes_what_a_terminal_acts_on` |
| Published ports that fit a monitor's holds only without its other holds (a lease per network, a hold per volume, the run directory, the writable area, `running`), `-P`'s not counted (9.9c's review) | `check_holds` counts every hold, `-P`'s ports included, against `MOST_HOLDS` and the soft descriptor limit less 64, before anything is made | `ports_that_leave_no_room_for_the_other_holds_are_refused_first` |
| `system stop` passing over a start in flight (its runtime state `created` or `creating`, its record never started): read as created, it was left out of the stop (9.9c's review, R1) | every container with runtime state that hasn't ended is stopped, and a restart waiting | `the_hosts_stop_takes_a_start_in_flight` |
| A start killed after recording itself started, its container made and its gate never opened: read as never started, so its policy didn't bring it back at boot (9.9c's review, R2) | the last start's history kept aside first (`previous-start.json`), put back by the next recovery pass (`undo_a_dead_start`) | `a_start_killed_after_recording_itself_keeps_its_history` |
| A user's starts keeping every start's logs, so a container started again and again filled the disk (9.9c's review, R4) | `prune_before_a_start` on every start, a policy's or a user's: the latest 10 kept | `a_users_starts_keep_the_latest_ten_starts_logs` |
| A monitor that couldn't start its exit command (the process limit, memory): the end settled later, silently, its policy not applied (9.9c's review, R5) | the monitor leaves `exit-command-failed` in the runtime state; the recovery pass reads it before `reconcile` and warns | `an_exit_command_that_never_started_is_said` |
| A container's lease found by its name: a run with no record (`--no-image`) of the same name could be taken for it by `rename` and `disconnect` (9.9c's review) | by the container's ID first; by name only when the caller asks (leases made before leases knew their container), and only one that says no container | `a_lease_by_id_comes_first_and_by_name_only_when_asked` |
| `volume rm` and `volume prune` removing a stopped container's volume (they looked only at holds), which its next start made again empty; `system df` counting it reclaimable (9.9c's review) | kept while any record mounts it, running or not; `rm` names the containers; `df` counts as `prune` does | `a_volume_a_record_mounts_is_kept`, `a_stopped_containers_volume_is_kept_until_it_is_removed` |
| A monitor's empty leaf reused: another `create`'s sweep (empty leaves older than a minute) could remove it between its opening and the monitor's join, leaving the monitor in the caller's cgroup (9.9c's review) | an empty one removed and made again, new (spared for a minute); one with a process in it kept | `a_monitors_empty_cgroup_is_made_again_and_a_live_one_kept` |
| A rootless container's pasta in the caller's cgroup (a login session's, a unit's): a logout or `systemctl stop` ended it, the container left with no network (9.9c's review) | the launcher joins the monitor's cgroup (`shipyard/monitors/<id>`) first thing, by writing `0` to its `cgroup.procs`; a failure said in pasta's log | `a_detached_launcher_joins_the_cgroup_it_is_given` |
| A bundle's `linux.cgroupsPath` naming another container's monitor's cgroup or a DNS server's, which its `delete` would kill (9.9c's review) | refused at `create` (`Cgroup::container`: nothing below `shipyard/monitors` or `shipyard/dns`) | `only_a_containers_own_cgroup_is_acted_on`, `a_users_containers_are_below_what_was_delegated` |
| An image's health check with a duration under 1 ms (not zero), kept in milliseconds as zero (the default) or rounded (9.9c's review) | the image's check left out with a warning, as Docker refuses it | `an_image_check_under_a_millisecond_is_left_out` |
| `health.json` read through a symlink, waiting on a FIFO, or whole whatever its size (9.9c's review) | `O_NOFOLLOW \| O_NONBLOCK`, regular files only, at most 1 MiB | `a_health_file_that_isnt_plain_or_is_too_big_is_not_read` |
| `doctor`'s failed boot unit hint naming `journalctl -u` for a rootless user's unit (9.9c's review) | `journalctl --user -u shipyard-recover` for a user unit | `a_restart_policy_without_the_boot_unit_is_a_warning` |

## Fuzzing

The fourteen cargo-fuzz targets of stage 8 (one list now, `scripts/fuzz-targets.sh`, which
reads `fuzz/Cargo.toml`'s `[[bin]]` names); stage 9 added none. `attach_frames` learned the
new frames (`HelloControl`, `Stop`, `HelloHeld`, the holder's) with their protocol versions.

- **The first `--fuzz 30` of 9.8b crashed `attach_frames`:** an empty `Stdout` frame
  decodes, but empty output is never encoded, so the target's round-trip check was wrong.
  The target now skips empty output frames. All 14 then ran clean.
- **The first `--mutants` sweep** (9.8b, 40 mutants of the runtime's `attach.rs` changes)
  missed two: `Done.why`'s cut to one frame, untested since 9.7b1. With
  `a_long_reason_is_cut_to_one_frame`, the rerun had 33 caught, 0 missed, 4 unviable and 3
  timeouts.
- **The final run (9.9a):** `scripts/test-linux.sh --fuzz 300`, all 14 targets 300 seconds
  each, no crash.

## Known limitations (by design, for stage 9)

- **`kill` and a container ending by itself (F47):** `kill` with SIGKILL or the stop signal
  marks the container stopped by its user before signalling, so its `__cleanup` never
  restarts what its user killed. One that ends on its own in the instant between `kill`'s
  status check and its signal stays down. The user asked it to stop.
- **The `m3-stop-only-running` mutant survives** (9.9a): holding a restart's start open in a
  test needs a hook in production code; the lock order in `wait_for_turn`'s documentation
  argues it. Other survivors, each argued in its step: `rm-drops-op-lock-early` (masked by
  `running`), `end-not-waited-for` (a window that can't be held open), `input-not-held` (no
  data lost without the gate; it bounds memory), `raw-not-retried` (SIGCONT usually comes
  first), `called-on-the-inits-end` (the launcher usually sees the init first).
- **The end-to-end test of a `system stop` that holds restarts off runs rootless only:**
  root's state root is shared with the other tests, so it returns early as root.
- **Restarts run the binary that made the container, with its maker's environment:** after
  an upgrade, `rm` and `run` again. The boot unit has systemd's environment (F44).
- **`wait` and `logs -f` stop at the first end;** Docker follows across a policy's
  restarts. A trapped stop signal still counts as a user's stop. `cleanup.log` lines have no
  timestamps.
- **A SIGKILL between `create` and `start`** leaves a created container holding its
  resources until `rm`. The signal hold is per thread: a multi-threaded caller's other
  threads take SIGINT.
- **A published port is free for a moment** while pasta binds it (as in a foreground run).
  One inotify instance per detached pasta counts against `fs.inotify.max_user_instances`
  (128 by default): not checked yet.
- **Unreleased formats changed in place:** record v3 was amended (`restart_delay_ms`) and
  `Config` gained `restart` and `health` without a version, because no release wrote them; a
  9.4 binary refuses v3 records. A stage 8 `runs/` left in `/run/shipyard` blocks `create
  runs` until a reboot.
- **Recovery after a reboot with many `--rm` leftovers** removes their layers one at a time,
  with no time budget.
- **`exec -u` with an unknown user exits 125** (Docker's may be 126). `attach` without `-i`
  reads no input, so its detach keys can't reach it; Ctrl-C ends the client.
- **Under a parent without job control,** `run -it … &` waits for an `fg` that never comes.
  A SIGTERM forwarded as a stop blocks the forwarding thread for up to the stop timeout.
  Ctrl-C with `-t` and no `-i` goes to the init (Docker's).
- **A stopped container's `/etc/hostname`, `/etc/hosts` and `/etc/resolv.conf`** are its
  image's: those exist only while it runs. Owners in `cp -a`'s archives are numeric.
- **Health:** `unhealthy` does nothing but show (Docker's); `--health-interval 1ms` is
  accepted; `health.log` is bounded only as it's opened.
- **DNS restarts are in `dns.log`, not the journal;** a journal that can't be written is
  warned about once per process; a follower missing three rotations between looks loses the
  oldest.
- **Networks:** `network connect` is root's (bridges), and never changes the resolver or
  `/etc/hosts` (D9.7b-1). The connection limit fails open past 65,536 sources, a table
  rebuild empties the counts, IPv6's ICMP errors read `EACCES`, and traffic within a network
  is counted only with `br_netfilter`. `--isolate-host` isn't offered on the default
  network (it has no DNS server).
- **A device node steered by a symlink** isn't checked by place (F31); a refused mount's
  mount point is left in the image.
- **Labels are kept and bounded in the record,** but no flag sets them yet.
- **The `Quality` workflow hasn't run** (billing is off): its jobs were checked by
  actionlint and by the same commands locally.

- **A rootless container made from an SSH login** (no delegated cgroup tree) keeps its
  monitor in the session's scope: a shutdown ends it before the user unit's `system stop`
  (F51). It comes back at boot, its end settled, but without its stop signal; `systemd-run
  --user --scope shipyard run -d …` avoids it.
- **The monitor's descriptor limit is its caller's** (F49): holds `network connect` adds
  to a container near it can be refused though the hard limit would allow them.
- **`pasta --help`**, how a rootless start learns what this pasta can do, also goes to the
  system log: about 60 lines in the journal per container start (F50).
- **A forced logout** (`loginctl terminate-user`) ends even a lingering user's containers;
  the user unit brings them back.

## Carried forward

- **Open from earlier stages:** F24 (an OCI bind source re-resolved at start, later), F29
  (the library's subreaper and SIGPIPE, stage 11), F31 (stages 7.5 and 7.6's residuals,
  later; its mount part closed in 9.8a), F34 (joining namespaces by path, stage 10), F18
  (CI: the `Quality` workflow manual while billing is off).
- **Closed in stage 9:** F10 (9.7c2), F30 (9.7c1), F36 and F40 (9.8a), F38 (9.4c1), F39
  (9.2a and 9.4b1), F41 (9.4b1), F42 (9.4a).
- **Stage 9's findings:** F37's rest, F43 (records across a version bump: the upgrade rule
  before any bump), F44 (the boot unit's environment), F45 (a full disk turns policies off),
  F46 (a paused container at stop: thaw first), F48 (the clean-code refactor, L1–L13: one
  meaning of "up", `run/detached.rs` split, one find-lock-re-read, one settle wait, a reason
  code in the hold protocol, the API narrowed, entry points listed once) to stage 10. F47 is
  a known limitation; a fix needs the monitor to deliver the signal and the mark together.
- **Found in 9.9b:** F49 (the monitor's descriptor limit for live holds) and F50 (`pasta
  --help` in the journal) to stage 10; F51 (a rootless monitor made from an SSH login
  stays in the session's scope) later. Fixed in 9.9b: hand-made root monitors in session
  scopes, ended before `system stop` (the monitor's own cgroup).
- **Found in 9.9c:** F52 (a shutdown during the boot unit's `system recover` skips its
  `ExecStop`: a separate stop unit) and F53 (the `TIME_WAIT` re-hold lock is an abstract
  socket name any local user can take: a lock file in the caller's runtime directory),
  both to stage 10.
- **Stage 10's first steps:** the resource and image commands by name (`stats`, `top`,
  `pause`/`unpause`, `update`; `tag`, image `inspect`, `history`, `save`/`load`;
  `--mount`), asked in 9.9b.
- **Stage 14:** the libseccomp and bpftool differentials (D9.9-5).
- **Later:**
  - waiting for a route at boot (9.5c's Murphy M2: a failed start is said, the others run);
    a rootless `systemd-run --user` default (or a `doctor` hint) where
    `KillUserProcesses=yes`; encrypted homes with linger;
  - `logs -f` and `wait` across a policy's restarts; timestamps in `cleanup.log`;
    `health.log` growth within one long start; an interval floor;
  - Docker's `exec`, `attach` and `pause` events; `--format` templates;
  - a non-blocking stop in the foreground run's forwarding thread; `attach` and `start -a`
    through the signal watch (`--sig-proxy` for them);
  - a bridge-network budget for a foreground container run; a container-path cgroup CLI
    test; a test of the input gate's memory bound; one settled-code function;
  - the `HOST_INSTEAD` hint tested (it needs root without `CAP_NET_ADMIN`);
  - the AppArmor CI job gaining a rootless stopped-`cp` line; a warning on copies into the
    `/etc` network paths; the `Busy` message naming the copy;
  - a pre-container lease whose rewrite failed after the record was renamed, repaired
    without a restart;
  - `HeldPort::also_ipv6` inferring IPv6 from the socket count; `parse_port` accepting
    `+80`; an IPv6 end-to-end test of the connection limit; tests of a monitor killed after a
    connect, a paused connect, racing connects;
  - `exec`'s `read_snapshot` reading `exec.json` by path; `run`/`create` gaining `-v`.

## Tests

Counted at 9.9b (`#[test]`, or the names in a `harness = false` file's lists):

| File | Tests | Stage 9's |
|---|---|---|
| `shipyard-engine/tests/detached.rs` (`harness = false`) | 121 | all: detached containers, pasta, recovery, restart policies, boot and host stop, stop/kill/wait, logs, attach, exec, `cp`, foreground runs, events, health, pasta's loss |
| `shipyard-cli/tests/runtime.rs` | 186 | 12: holds and the exit command (5), the companion (2), `stopping` and a newer `state.json` (2), F36 (2), labelled holds while it runs (1) |
| `shipyard-cli/tests/terminal.rs` (`harness = false`) | 81 | 9 in `ISOLATED_TESTS` (now 62; `TESTS` 19): `attach` and `exec -it` by name, seven terminal container runs |
| `shipyard-cli/tests/containers.rs` | 16 | all: by name, the milestone, the upgrades, port ranges, health, rename, the monitor's own cgroup |
| `shipyard-cli/tests/connect.rs` | 5 | all: connect, disconnect, `port --sockets`, interface sysctls |
| `shipyard-cli/tests/events.rs` | 5 | all |
| `shipyard-cli/tests/budgets.rs` | 8 | 4: `run -d`, `ps` (and `create` among 100), `stop`, a terminal container run |
| `shipyard-cli/tests/dns.rs` | 12 | 3: a renamed container, the server's cgroup, its worker restarted |
| `shipyard-cli/tests/firewall.rs` | 19 | 2: `--isolate-host`, `--conntrack-limit` |
| `shipyard-cli/tests/image.rs` | 22 | 2: pull's progress, a killed run's container |
| `shipyard-cli/tests/doctor.rs`, `bundle.rs`, `run.rs` | 8, 25, 36 | 1 each: restarts held off, F36 into `/dev`, a bare `--env` name |
| `shipyard-runtime/tests/containers.rs` (`harness = false`) | 22 as root, 6 rootless | 5: planned containers and a created init followed |
| `shipyard-engine/tests/boundaries.rs`, `launch.rs` | 3, 1 | all (9.1) |
| `shipyard-engine/src` | 148 | 123 (25 before: streams, logs, `cp`); `containers/tests.rs` 33, `run/tests.rs` 18 |
| `shipyard-runtime/src` | 427 | 35 (392 before): versions, plan, holds, control, the monitor's server |
| `shipyard-network/src` | 252 | 20 (232 before): leases, pasta, rulesets v7 and v8, expressions, DNS |
| `shipyard-image/src`, `shipyard-storage/src` | 163, 12 | 6 and 3: owned layers, the image's health check, volume holds |
| `shipyard-cli/src` | 82 | 83 before: some moved to the engine with their code; new: `containers.rs` 7, `doctor.rs` 4 |

**At the close (9.9c):** macOS 1,029 passed. Linux, both passes of `test-linux.sh`, now with
`--no-fail-fast`: 3,452 passed, 0 failed (406 root-only tests skipped in the normal user's pass,
run in root's); 3,407 at 9.9b. (9.9a's 1,652 was incomplete: the root pass stopped at the first
failing test binary.) Clippy for
macOS, glibc and musl, and rustdoc with CI's flags, clean; the conformance gate as expected;
fuzzing clean. Mutants, hand-picked, controls first, per step: 9.1a 12/12, 9.1b 9/10 (one
argued), 9.1c1 11/11, 9.1c2 5/5 (two not run: one equivalent, one needing root without
`CAP_NET_ADMIN`), 9.2a 11/11, 9.2b 15/15, 9.3a 10/10, 9.3b1 15/15, 9.3b2a 27/30 (three
argued), 9.3b2b 13/13, 9.4a0 8/8, 9.4a 13/14 and F42 4/4, 9.4b1 13/13, 9.4b2 12/12, 9.4c1
17/17, 9.4c2 12 caught (one equivalent, one window), 9.4c3a 10/11, 9.4c3b 10/11, 9.5a
11/11, 9.5b 11/11, 9.5c 12/12, 9.6a 20/20, 9.6b1 9/10 (one equivalent), 9.6b2 7/7, 9.6c1
3/4 (one unreachable), 9.6c2 10/10, 9.7a 7/7, 9.7b1 8/8, 9.7b2 7/7, 9.7c1 10/10, 9.7c2
9/9, 9.8a 6/6, 9.8b's sweep 33 caught after one test, 9.9a 18 caught and one argued
survivor, 9.9b (the monitor's cgroup) 3/3: no join, the parent unlisted, `rm` leaving the
leaf; 9.9c 24/24, each caught at its own assertion (the review's fixes and the rollback the
tests now reach). No `--mutants` sweep of 9.9a alone (D9.9-8).

## Review history

| Step | What | A finding of its own reviews |
|---|---|---|
| 9.1a | the engine's `Error`, `Reporter` and `Paths`; images, volumes and the storage helper moved | another crate's error hidden from `downcast_ref` inside the engine's (`Error::find`); helper pipe failures given kinds |
| 9.1b | network and the DNS server moved | none of its own; the full run found a race from stages 3, 6 and 7: a helper's answer lost between a poll and its exit |
| 9.1c1 | planning a run (`RunRequest`, `check`, `finish`) | an API's image run without isolation ran on the host (`validate`); `Debug` printed the environment |
| 9.1c2 | launching a run (`Outcome`, one guard for its resources) | a broken doc link that would have failed CI's rustdoc; launch failures released volumes before the network |
| 9.2a | versions on the runtime's files, `stopping`, `.runs` | OCI callers may refuse `stopping` (D9.2a-1); the downgrade claim corrected |
| 9.2b | the engine's container record, IDs, serials, names | `create` took an unreadable record's name and serial as free |
| 9.3a | holds and the exit command | wrong descriptors at 3 (`F_DUPFD` numbers not consecutive); the shuffle could hit std's exec-error pipe |
| 9.3b1 | `create_planned` | `exec` into a plan with user names always failed; a plan without a root ran on the host's files |
| 9.3b2a | `run::detached` and `__cleanup` | `__cleanup` raced its monitor's copies; the monitor and `__cleanup` deaf to SIGTERM (an inherited mask, found by the book agent) |
| 9.3b2b | pasta following the init; the monitor double-forked | pasta's launcher pinned host mounts for the container's life |
| 9.4a0 | Docker's spelling, `run IMAGE [COMMAND]` | two root-only tests built their commands outside the sweep |
| 9.4a | `create`, `start`, `run -d`, `ps`, `inspect`, `rm`; own layers, pins | the op lock's re-check was needed (a mutant); F42, a port in `TIME_WAIT` |
| 9.4b1 | `stop`, `kill`, `wait`, `restart`; protocol v2; F41 | real-time stop signals never sent; waiters could keep a stop out |
| 9.4b2 | `rename`, live DNS names | torn lease reads during a rename; `rename` allowed dots (the book agent) |
| 9.4c1 | `logs`, `attach`, `exec`, `cp` by name; F38 | `-f` into the next start worked only within a window; an open stdout line hidden by stderr |
| 9.4c2 | `cp` of a stopped container | a live copy into a container that ends could be started under its helper |
| 9.4c3a | foreground runs as containers, without `-t` | output lost when the run's own client fell behind; the signal mask left blocked |
| 9.4c3b | `-t` through the attach client, detach keys | a container ignoring SIGHUP left `run -it` lingering; cooked keys after `bg`/`fg` under real bash |
| 9.5a | record v3, the recovery pass | a crashed `--rm` container brought back by its own `start`; a concurrent start wiped a user's stop |
| 9.5b | restart policies, `update --restart` | a waiter cleared another restart's schedule; a dead waiter made `rm` refuse |
| 9.5c | `system recover`, `system stop`, units, `doctor`'s lines; the Lima VM | a failing `ExecStart` would have systemd kill every container it started; a shutdown marked stops as the user's |
| 9.6a | the events journal, `shipyard events` | `reconcile` settled ends silently; `rm -f` wrote no `die`; reading the past was quadratic |
| 9.6b1 | the companion, `__health`, `--health-*` | a pause mid-check ended checks for good; a check in flight escaped the companion's end |
| 9.6b2 | the image's `HEALTHCHECK` and `SHELL` | the image's `SHELL` ignored; a malformed image check made the image unrunnable |
| 9.6c1 | pasta's death said | a killed session's init read as alive after pasta's reap |
| 9.6c2 | DNS servers in their own cgroup, a watchdog | an inherited ignored SIGCHLD looped restarts; `pids.max` below the server's tasks |
| 9.7a | DNS by short ID, `port`, `network inspect` by container | none in the logic; docs (`volumes.md`'s message) and a flaky volume test |
| 9.7b1 | labelled holds, protocol v3 | a descriptor leak on `MSG_CTRUNC` that could skip `__cleanup`; one unknown label refused every hold |
| 9.7b2 | `network connect`, `disconnect` | a Ctrl-C could leave a container connected but unrecorded; a veth left live by the unlink order |
| 9.7c1 | `port --sockets`, F30, `--isolate-host` (ruleset v7) | a newer table rebuilt by an older binary; other clouds' metadata addresses open |
| 9.7c2 | `--conntrack-limit` (ruleset v8), N10 | `ct state new` counted every unanswered packet; link-local spoofing could fill the set |
| 9.8a | F36, F40 | nested standard places (`/dev/ptmx` in `pts`) passed |
| 9.8b | the `Quality` workflow, `--mutants`, one fuzz list | (no review of its own) its first runs: a wrong `attach_frames` expectation, an untested truncation |
| 9.9a | F37's rework, the milestone test, the gap tests, whole-stage reviews, fuzzing | ports past about 29 refused after all was made; a late start failure erased the history boot relies on; `system stop` failed with no state root |
| 9.9b | notes, docs sweep, v0.2.0, the manual milestone on three hosts | the monitor's cgroup at shutdown (fixed); docs gaps (`enable --now`, linger's `sudo`, detach's Enter) |
| 9.9c | four fresh reviewers (security, robustness, test quality, docs), every claim then traced by a fifth: 41 items, 26 confirmed, 11 partly, 1 wrong as worded | fixed: per-item commands on a broken pipe; the holds bound (`-P` and the other holds); `system stop` and a start in flight; logs pruned on every start; a killed start's history; a stopped container's volumes (Docker's rule); pasta in the monitor's cgroup; a reused monitor leaf; leases by ID first; terminal escapes; sub-millisecond image checks; the health file and `cgroupsPath` hardening; the exit command's failure said. Tests: the record v3 pin, connect's rollback, error kinds, no sleeps for synchronisation. 14 docs fixes. Recorded: F52, F53 |
| v0.2 review | five fresh reviewers (runtime, engine, network/image/storage, CLI/tests/release, security), the main claims then checked against the code; a hardening pass after it | fixed: a broken pipe anywhere in an error taken for stdout's (`pull` exited 0 on a storage helper's); errors, warnings and hints printed terminal escapes from registries and layers; an unreadable record blocked every `create` with no way to remove it (`rm -f ID`); a failed restart settled after the operation lock was let go; a monitor that said it failed ran anyway; `rustls` advisories. One claim wrong: the bridge has no anti-spoofing (it has, `nft/spoofing.rs`). Recorded: F54–F62 |

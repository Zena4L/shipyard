# Stage 13 notes: macOS production

The engineering record of stage 13: what was built, why it's built that way, what the
framework and the guest's kernel were checked to do, what can go wrong and how that's
handled, and what was left for later stages. For how to *use* it, see [Shipyard on a
Mac](../user/macos.md) (the VM's size, clock and disk, Rosetta, your files, signals,
ports, Docker's CLI, the login start) and [The Docker API](../user/api.md) (the "mounts
denied" refusal).

## Outcome

Stage 13 asked for the Mac path made production-grade ([ROADMAP](../ROADMAP.md), [FEATURES
§13](../FEATURES.md#13-macos-production--stage-13)), Apple Silicon only (D29), macOS 13 the
floor (D10.3-4); its milestone is v0.5, internal (D21). Its outline was decided on
2026-10-26 (D13-1 to D13-4) and built in steps:

- **13.0** the stage-12 build tests owed since #20 (blocked by this network until 13.5a),
  and FEATURES and ROADMAP for D13-1 to D13-4;
- **13.1** the probes: Rosetta, the balloon, TRIM, virtiofs, the clock (no product code);
- **13.2** the VM: its size, its clock, its disk, its liveness;
- **13.3** Rosetta and platforms (D13-2);
- **13.4** file sharing: Docker Desktop's places, "mounts denied";
- **13.5** ports and the guest's network (a: UDP and dialling the container; b: the
  agent's port held by PID 1, the guest's firewall, F98's warning);
- **13.6** the forwarded wire and the lifecycle (a: the control connection; b1: the
  VM's start and stop; b2: its sockets and its start at login);
- **13.7** signing and the floor (D13-3);
- **13.8** F132 and the close (a: the VM tests bounded, the crash rate measured; b: the
  close, these notes).

From the hardening track the stage took on H1 (the bind rule, the platforms, what comes
back after a boot: the engine's; the Mac's crate only carries them), H2 (every new file
and wire versioned from its first write: `vm.json` v1, `booted.json` v1,
`comes-back.json` v1, `network.json` v2, `ports.json` v2, the ports answer v2 then v3,
attach protocol 6, the ruleset v9; each older version read or refused by a test), H3 (a
threat model in every plan; a Murphy review of each plan before it was built, 13.4 to
13.6b2), H6 (`Refusal::Newer`, `VmError::Size`, `PullError::is_other_platform`), H7 (two
budgets: 2,000 small files through a share under 2 s, a UDP round trip under 1 s).

It closed F90, F93, F94, F96, F97, F98, F100, F103, F105, F106, F108, F109, F138, F140,
F143, F144, F147, F152, F155 and F167, and, in the close, F92 (every package the guest
installs pinned: D13.8b-1). It moved F59, F69, F77 (D13-4), F113 (D13.2-4) and F132
(D13.8-2) to stage 15, and opened F168–F181, all homed in stage 15. A user can now:

- `shipyard vm set --memory 8g --cpus 6` (or `default`), and see in `vm status` what
  the next start takes; by default the VM gets a quarter of the Mac's memory (4 to
  16 GiB) and half its cores (at most 8);
- leave the Mac asleep and come back to a guest whose clock is right within seconds;
  get the disk space a prune freed back at the VM's next stop;
- `shipyard run --platform linux/amd64 …` (and `pull`, `create`, `build`), or run an
  image that has no arm64 build at all, through Rosetta;
- `shipyard run -v /tmp/proj:/w …`, `-v /Volumes/Drive/x:/x`, or the same through
  `docker` and `docker compose`: the home, `/Volumes`, `/tmp` and `/var/folders` are
  shared at their own paths; anything else is refused ("mounts denied"), as Docker
  Desktop refuses it;
- `shipyard run -p 5353:53/udp …` and reach it on the Mac's `127.0.0.1` and `::1`;
  see in `vm status` when another Mac program shadows a forwarded port;
- Ctrl-C, `kill`, resize or hang up a forwarded command that has stopped reading its
  input (`yes | shipyard run -i … sleep 600`);
- `shipyard vm stop` during a start, and have it succeed;
- `shipyard vm install --at-login`, so containers with a restart policy come back
  after a login without a command run first;
- `scripts/macos/make-signing-identity.sh` once, so macOS asks its privacy questions
  once, not after every rebuild; `doctor` says how `shipyard` is signed.

| Requirement | Where | Tested by |
|---|---|---|
| Rosetta: the share and its registration at boot (D13-2) | `shipyard-vm`'s `machine.rs` (`rosetta`, `rosetta_share`), `host.rs`'s command line (`forward::ROSETTA_WORD`); the guest's `init.rs` (`rosetta`, `register_rosetta`: Apple's magic and mask, flags `CF`) | `an_amd64_image_runs_through_rosetta` (VM: `uname -m` says `x86_64` under the default seccomp profile); units `rosetta_s_registration_is_apple_s`, `rosetta_is_said_where_it_is_given` |
| What runs here: the host's platform, then amd64 where Shipyard's own Rosetta entry is registered | the engine's `image::platforms_here`; `import.rs` (`named_in`, `platform_here`); `shipyard-image`'s `PlatformChoice`, `AnyPlatform`, `runs_on_any`, `Index::select_among` | `what_runs_here_is_the_host_s_then_rosetta_s`, `only_shipyard_s_rosetta_counts` (a Lima VM's entry isn't taken), `the_first_choice_the_index_has_is_taken` |
| `--platform` on `pull`, `run`, `create`, `build`; the API's `platform`; an image with no arm64 build pulled as amd64, warned | `pull_with` and `from_store*` over `&dyn PlatformChoice`; `said_if_other` (`WarningKind::OtherPlatform`); `build/args.rs` (`--opt platform=`); the API's `images.rs`, `create.rs` | `an_image_is_pulled_for_the_first_platform_that_runs_here`, `another_platform_is_told_from_other_failures` (the fixture registry, no Docker Hub), `a_platform_that_doesnt_run_here_is_refused_first` (Linux: 125 for `run`/`create`, 1 for `pull`) |
| Memory: the VM's size from the Mac, `vm set` (D13.2-1); the balloon **not built** (D13.2-2) | `shipyard-vm`'s `settings.rs` (`vm.json` v1: `defaults`, `bounds`, `resolve`, `VmSettings::change`, `this_mac`), `sysctl.rs`, `machine::max_cpus`; the CLI's `vm.rs` | `the_vm_takes_its_size_from_vm_set` (VM); units `the_default_size_follows_the_mac`, `a_set_size_is_taken_within_bounds`, `vm_set_keeps_what_it_is_asked`, `a_newer_or_unknown_vm_json_is_refused`, `a_size_is_shown_plainly` |
| The clock after a sleep | no new channel: the ports poll is a `Request` stamped with the Mac's time, which the agent follows (`agent.rs`'s `follow_clock`), every 2 s while the VM runs | `a_wrong_clock_is_set_right_by_the_host` (VM: a request an hour behind sets the clock back, the control; within 10 s with no command it's the Mac's again) |
| The disk: freed space back at each stop (D13.2-3, F103); `doctor`'s size and the Mac's free space | the guest's `init.rs` (`trim_data`: `FITRIM` after the containers end, before the unmount); `shipyard-vm`'s `doctor.rs` (`disk`) | `a_stop_gives_freed_space_back` (VM: the image grows with a 512 MiB file, the control, then shrinks); unit `the_disk_is_said_with_the_macs_free_space` |
| Liveness: a guest that rebooted in place or hangs is stopped (F90); the console capped | `liveness.rs` (`Liveness`, `Heard`, `Verdict`: 10 s, 6 misses), `host.rs` (`keep_alive_checked`, `keep_console_small`, `end_host`), `forward::BOOT_REQUEST`, `BOOT_UNREADABLE`, `forwarding.rs` (`heard`); `panic=5` on the command line | units `another_boot_id_is_a_reboot`, `only_misses_in_a_row_are_a_hang`, `an_older_guest_is_left_unasked`, `a_boot_answer_is_heard_for_what_it_is`, `a_full_console_is_moved_aside_and_written_on`; end to end: none (no unprivileged trigger) |
| A copied VM directory gets its own network address (F138) | `network.rs` (`network.json` v2, `DirId`, `Card`; a v1 file adopted) | `a_copied_directory_gets_its_own_address` |
| Bind mounts of macOS paths that just work: the home, `/Volumes`, `/private/tmp`, `/private/var/folders` (D13.4-1) | `forward::{MAC_SHARES, MAC_ALIASES, SHARES_KEY, GUARDED_FOLDERS}`; `shipyard-vm`'s `shares.rs`, a virtiofs device a share; the guest's `init.rs` (`shares`, `mount_share`, `bind_alias`, `hidden_in`; `TMPDIR=/run/tmp`) | `a_bind_of_tmp_is_the_macs` (VM, both ways); units `the_shares_are_real_directories_beside_the_home`, `the_macs_share_tags_are_told`, `the_shares_are_mounted_nosuid_and_nodev`, `only_places_in_a_share_are_hidden` |
| "Mounts denied" for a bind outside the shares, at both doors (D13.4-3, F143) | the engine's `guest/shares.rs` (`Shared::here`, `check`, `check_again`, `check_binds`, `privacy_hint`, `on_a_macs_share`); called by the API's create and the CLI's `run`/`create`, again at each start; the CLI's `forward.rs` (`within_shares`, `guest_cwd`) | `a_bind_outside_the_shares_is_refused` (VM: the API's 400s; a link a container swapped in refused at the next start), `the_vms_own_places_stay_hidden`; units `a_bind_is_let_through_only_from_a_share`, `a_guests_shares_are_read_from_its_command_line`, `the_macs_shares_are_told_by_their_mount`, `only_paths_in_the_shares_are_forwarded`, `the_directory_is_the_callers_in_a_share` |
| The privacy folders said by name (F105, F140's doctor and refusal) | `privacy_hint`; the guest's `doctor.rs` (every guarded folder tried at once, 5 s); the agent's directory refusal | `a_guarded_folder_names_the_permission`, `the_privacy_line_names_what_macos_holds_back` |
| File sharing fast enough: virtiofs **not tuned** (D13.4-2: nothing to tune), a budget (H7) | — | `small_files_on_a_share_stay_within_a_bound` (2,000 files under 2 s); F172 the bar not met |
| UDP published ports (F96) | `forward::{FORWARD_UDP_REQUEST, write_datagram, read_datagram, UDP_IDLE, MAX_UDP_PEERS}`; `shipyard-vm`'s `udp.rs` (`Peers`), `forwarding.rs` (`bind_udp`), `ports.rs` (`wanted`, `ports.json` v2); the guest's `forwarded.rs` (`relay_udp`) | `a_udp_port_answers_on_the_macs_loopback` (VM, numbered datagrams, H7 under 1 s); units `datagrams_are_framed`, `each_peer_gets_its_own_replies_over_one_stream`, `peers_are_remembered_while_heard`, `a_broken_stream_is_joined_again`, `datagrams_reach_the_container_and_come_back`, `a_udp_port_held_on_every_address_is_busy` |
| A forwarded port dialled at its container's own address (F97) | the guest's `forwarded.rs` (`target_of`, `find_target`); the agent's `published_or_refuse`; the guest's DNAT only as a fallback, said | `a_tcp_port_is_reached_at_the_containers_own_address` (VM, both loopbacks; the console never says the fallback ran); unit `the_container_is_found_by_its_lease` |
| The agent's port held by PID 1 from boot (F108) | the guest's `init.rs` (`AGENT_LISTENER`, `start_agent`), `agent.rs` (`at_descriptor`, `is_agent_socket`, `waited_too_long`), `forward::AGENT_FD`; the host's `wait_for_agent` (asks the boot ID) | units `the_agents_port_is_said_held_or_handed_on`, `a_request_that_waited_too_long_is_refused`; doctor's `agent port` line in `doctor_checks_the_vm`; the restart itself untested end to end (F176) |
| Published ports refused from the guest's NAT link (F108) | `shipyard-network`'s ruleset v9 (`set::OUTSIDE`, `iifname @outside ct status dnat drop` before the DNAT accept); the engine's `network/firewall.rs` (`outside_links`, `guest::booted_by_shipyard`), `forward::GUEST_LINK` | `a_published_port_doesnt_answer_on_the_vms_own_address` (VM; controls: `localhost`, and a host-network container on the same address), `a_link_outside_reaches_no_published_port` (root, Linux); units `only_dnated_traffic_reaches_a_bridge_and_internal_networks_are_closed`, `the_table_is_built_in_order_and_every_item_encodes` |
| A port another Mac program shadows, said (F98, D13.5-1) | `ports.rs` (`held_on_every_address`, `shadowed`); `vm status`'s `shadowed:` lines; `doctor`'s clause | `a_mac_program_on_a_forwarded_port_is_said` (VM); unit `a_program_on_every_address_is_found` (macOS only) |
| Signals, resizes and hang-ups on a channel of their own (D13-1, F94) | `Frame::Session(u128)` (kind 26, attach `VERSION` 6), `forward::CONTROL_REQUEST`; the guest's `control.rs` (`Controlled`, `SESSIONS`, `Registered`, `new_token`, `serve`); the Mac's `client/session.rs` (`open_control`); the runtime's `Welcomed::{session_token, with_control}` | `a_signal_reaches_a_command_that_reads_no_input` (VM, piped), `a_signal_passes_a_paste_the_command_never_reads` (VM, a terminal); units `a_session_token_follows_a_version_6_welcome`, `a_control_connection_takes_the_signals_and_resizes`, `a_command_is_found_by_its_token_while_it_runs`, `an_ended_command_is_signalled_no_more`, `a_session_token_is_32_hex_digits`, `the_hosts_own_requests_are_internal` |
| An older guest's refusal said plainly (F167), another build's VM said (F173b) | `Decline::Newer` (code 3), `Refusal::Newer`, `forward::ANOTHER_BUILD`; `shipyard-vm`'s `booted.rs` (`booted.json` v1, `running_elsewhere`, `another_build`) | `another_builds_vm_is_said` (VM); units `another_build_is_told_by_its_revision_or_its_binary`, `another_home_or_guest_is_said`, `the_booted_file_is_versioned` |
| An API hang-up names its connection by a token (F155) | the agent's `Named`; `forwarding.rs` (`joined_from`, `hang_up`) | `an_api_connection_is_named_by_its_port_and_its_token` |
| A relay's linger from its last byte (F109); a session's end by the grace not a 143 | `shipyard_sys::relay` (`copy_stamping`, `cut_at`, `LINGER_BOUND` 10 min); the Mac's session (`over` under a mutex) | `a_side_still_sending_is_not_cut`, `the_cut_is_a_linger_after_the_last_byte_within_a_bound` |
| `vm stop` asks the host, never a PID (F93); a stop during a start succeeds (F106's part) | `shipyard-vm`'s `control.rs` (`Stops`, `Phase`, `serve`), `phase.rs` (`HostPhase`, `PhaseAnswer`, `stop_wait`, `at_deadline`, `STOP_BOUND`, `STOPPED_BEFORE_SERVING`), `host.rs` (`stop_in_order`, `LONGEST_STOP`), `client.rs` (`ask_host`, `STOP_MOST`); `forward::{HOST_PHASE_REQUEST, HOST_STOP_REQUEST}` | `a_stop_during_a_start_succeeds` (VM), `a_logout_stops_the_vm_in_order` (launchd's `bootout`, during a boot and once up); units `the_host_socket_answers_its_asks`, `a_stop_is_asked_once_and_wakes_its_waiter`, `a_phase_moves_on_only_from_where_it_is`, `a_phase_is_said_and_read`, `a_stop_waits_by_the_hosts_phase` |
| A start's bound per phase, never a signal (F100); a launchd start's failure seen with its runs unknown (F144) | `client.rs` (`START_MOST`, `at_deadline`); `client/host.rs` (`ended_in_launchd`, the `marked` start) | `a_start_never_stops_a_healthy_host`, `a_launchd_start_ends_by_launchds_word` |
| The host keeps its own log short (F106's part) | `control.rs` (`keep_log_small`: at its start and hourly, past 1 MiB) | `a_big_log_is_moved_aside` |
| Every accepted connection blocking (F147); one budget for the host's sockets (F152c) | `serving.rs` (`accepted`, `Budget`: 1,024 in all, `PORTS_SHARE` 768) | `an_accepted_connection_is_blocking`, `the_budget_keeps_room_for_the_sockets` |
| An installed but unloaded agent said (F152a); `system service` on a Mac points at `docker.sock` (F152d) | `launchd.rs` (`AgentState`, `state`, `NOT_LOADED`); the CLI's `vm.rs`, `forward.rs`; `doctor.rs` | by hand in 13.6b2's checks; `doctor`'s line unit-tested with the state given |
| The start at login for containers that come back (D13.6-2, F106) | `forward::PORTS_COMES_BACK` (ports answer v3); the engine's `run/recover.rs` (`policy_restarts`, `starts_at_boot`, `comes_back`, `HOST_STOP_EXIT`); `shipyard-vm`'s `comes_back.rs` (`comes-back.json` v1, `AT_LOGIN_VAR`, `would_come_back`), `forwarding.rs` (`count_to_keep`, `last_look`), `host.rs` (`asked`, `taken_fresh`), `launchd::install(paths, at_login)`, `starts_at_login` | `the_vm_starts_at_login_for_containers_that_come_back` (VM: `bootout` and `bootstrap`; the control without `--at-login`); units `what_comes_back_is_what_a_boot_starts`, `the_count_is_versioned`, `the_login_start_is_asked_by_one`, `an_older_answer_keeps_the_count`, `the_answer_is_at_the_version_asked`, `an_older_host_is_answered_at_v1` |
| The poll backs off while nothing runs (F106) | `forwarding.rs` (`quiet_wait`, `QUIET_POLL` 10 s, `SETTLING` 30 s; never past the idle limit) | `a_quiet_poll_backs_off_to_its_bound`; `commands_racing_the_idle_stop_all_run` (VM, idle 1 s) |
| The guest found beside a checkout's binary (D13.6b-2) | `paths.rs` (`guest_dir`: only a guest that, and whose directory, are the user's alone) | `the_guest_is_found_beside_the_binary` |
| A stable signing identity (D13-3, D13.7-1, F140) | `scripts/macos/make-signing-identity.sh`, `sign.sh` (`SHIPYARD_SIGN_IDENTITY`, else the local identity by its SHA-1, else ad hoc); `doctor.rs` (`signer`) | `scripts/macos/test-sign.sh` in `verify.sh --mac` (two builds, one designated requirement; the ad-hoc control's two differ); unit `the_signer_is_codesigns_first_authority`; the kept permission across a rebuild by hand (the milestone) |
| Developer ID signing and notarization scripted (D13-3) | `scripts/macos/notarize.sh` | not run: a release's (stage 15) |
| The floor: macOS 13 (D10.3-4) | `.cargo/config.toml` (`MACOSX_DEPLOYMENT_TARGET` 13.0, forced); `doctor.rs` (`MACOS_FLOOR`, `floor_refusal`, `this_macs_floor_refusal`: the client's start, the host's boot, `doctor`) | `an_older_macos_is_refused_by_name`; `test-sign.sh` checks `minos` 13.0 |
| The VM tests bounded (F132's part) | `tests/vm_support` (`COMMAND_LIMIT` 300 s, `finish_within`, `BoundedOutput`, `AT_ONCE` 2) | `a_child_past_its_limit_is_killed`; the whole VM suite |
| Ports and DNS from macOS; the VM's start on first use and idle stop | stage 10 (10.3) and 11 (`docker.sock`); stage 13 made the ports UDP and IPv6's loopback, and the lifecycle's edges sound (13.6b) | the VM suite's port, idle and stop tests |
| **The v0.5 milestone:** an amd64 image, a compose project in the home with a bind and a UDP port, a sleep and wake, the idle stop, a login, memory after a build | the whole of the above | Mine, on this Mac's real VM directory (2026-10-27): an amd64 image (`uname -m` x86_64), a compose project in the home (nginx on a bind, a change on the Mac seen at once; a UDP echo on `localhost:17777`), `docker run` and `docker version` through `docker.sock`, the idle stop (20 s), the VM's memory 987 MiB before a 1 GiB build and after it (F168: held, as said): 13/14, the build from `$TMPDIR` refused (F181; from the home it built). The user's part: `scripts/macos/milestone-13.sh` (`identity`, `install`, `after-login`, `documents`, `before-sleep`, `after-wake`): **pending** at the merge (2026-10-27, the user's choice: to be run while stage 14's UI is tested; its result recorded here then) |

**Moved out of stage 13:**

| Item | Now in | Why |
|---|---|---|
| The memory balloon (F168) | stage 15 | D13.2-2: three runs, nothing given back to macOS while it had memory to spare; no device attached, nothing claims memory is returned; the size (`vm set`) is the lever |
| F132's KASAN guest kernel and an upstream report | stage 15 | D13.8-2: 13.8a didn't reproduce the crash (0 in 353 counted boots), so there's no trigger to name; the bounds are done |
| An install path for a Mac (the guest beside an installed binary, `vm install` copying it) (F178) | stage 15 | D13.6b-2: a checkout's guest is found beside its binary; packaging is stage 15's |
| F113's copy-out of `lost+found` | stage 15 | D13.2-4: the guest's `doctor` says its count |
| F59, F69, F77 | stage 15 | D13-4: not Mac work (F59 needs packaging's system user) |
| FEATURES' Mac CLI "over the API" (D11.5b-1) | dropped | D13-1: one code path on both platforms beats a second CLI backend made from Docker's JSON; the wire was fixed instead |
| virtiofs "its caching tuned" | not built | D13.4-2: the framework exposes no cache mode or DAX, the guest's virtiofs no cache options; OrbStack's numbers recorded as a bar not met (F172) |
| `vm set --share`, shares chosen by the user | not built | D13.4-1: Docker Desktop's fixed set |
| Rosetta's translation cache (F169) | stage 15 | 13.3: ~3 ms an exec; `VZLinuxRosettaCachingOptions` and `rosettad` measured first |
| A bind of Docker's socket (Ryuk, Docker-in-Docker) (F171) | stage 15 | 13.4: refused by the new rule; Docker Desktop gives the VM's own socket |
| An old VM's `/tmp`; Time Machine's snapshots; `doctor`'s waits (F173b–d; the bind by descriptor, F173a, done in 13.8c) | stage 15 | 13.4's plan Murphy review |
| The readiness review's lows (F182–F188) | stage 15 | 13.8c: the Mac host's launchd edges, the boot's unbounded queue wait, UDP peer churn, the guest's clock and case folding, platform edges, a second's window at the first boot |
| The guest's input rule for its NAT link (F175) | stage 15 | 13.5b: only published ports refused from `eth0` |
| The agent's restart tested end to end (F176) | stage 15 | no unprivileged way to kill the agent from a test |
| A bound on the agent's connections (F177) | stage 15 | 13.6a's plan review: a cap without room for the host's own asks would lock them out |
| `ImageRecord.platform` | not built | 13.3: a record already holds its platform's manifest digest |
| A Linux host's qemu `binfmt_misc` handler | not taken | 13.3: another platform stays refused on Linux; stated in the docs |
| Developer ID signing and notarization, run | stage 15 | D13-3: they need a paid Apple Developer account |

## Architecture

### The crates

No crate is new. What each gained:

- **`shipyard-vm`:** `settings.rs` and `settings/` (`vm.json`), `sysctl.rs`,
  `liveness.rs`, `shares.rs`, `udp.rs`, `booted.rs`, `phase.rs`, `control.rs` (the
  host's stops, its phase, `host.sock`, its log), `comes_back.rs`; `machine.rs`'s
  Rosetta share and one virtiofs device a share; `network.rs`'s v2 card;
  `forwarding.rs`'s UDP listeners and the quiet poll; `serving.rs`'s budget;
  `launchd.rs`'s `--at-login` and agent state; `doctor.rs`'s `disk`, `rosetta`,
  `shares`, the floor and the signer.
- **`shipyard-engine`:** `guest/shares.rs` (the bind rule), `guest/forwarded.rs` (a
  forwarded port's target, UDP's relay), `guest/control.rs` (the control connection);
  `guest/init.rs`'s agent listener, shares, Rosetta and `FITRIM`; `image::platforms_here`
  and the platform rule; `run/recover.rs`'s `comes_back`; `network/firewall.rs`'s
  `outside_links`.
- **`shipyard-runtime`:** the attach protocol's `Frame::Session` and `VERSION` 6;
  `attach/forward.rs`'s requests (boot, UDP, control, the host's phase and stop), the
  ports answer v2 and v3, `Decline::Newer`, the shares' keys; `Welcomed::with_control`.
- **`shipyard-image`:** `PlatformChoice`, `AnyPlatform`, `runs_on_any`,
  `Index::select_among`, `PullError::is_other_platform`.
- **`shipyard-network`:** the ruleset v9 (`outside`).
- **`shipyard-sys`:** `relay`'s `copy_stamping` and `cut_at`.
- **`shipyard-cli`:** `vm set`, `vm install --at-login`, `vm status`'s size, build,
  shadowed, agent and at-login lines; `--platform` on `pull`, `run`, `create`;
  `forward.rs`'s `within_shares`.
- **`shipyard-test-support`:** `pty::Session::type_until_stuck`.
- **Scripts:** `scripts/probes/vz-probe/` (13.1), `scripts/macos/`
  (`make-signing-identity.sh`, `test-sign.sh`, `notarize.sh`, `milestone-13.sh`),
  `guest-crash-rate.sh` brought up to date (13.8a).

### The host process

```
shipyard (the Mac's CLI) ── spawns, or launchctl kickstart ──▶ __shipyard-vm (the host)
                                                                │
 main thread   Stops::watched: SIGTERM, SIGINT, SIGHUP blocked before any other thread
   │           launchd's docker.sock checked in (if launchd started it); asked? (at load:
   │           only a CLI's start-asked mark, or --at-login with comes-back.json > 0,
   │           never vm install's own just-installed run)
   │           vm.lock taken → host.sock bound (0600) → its PID written → host.log kept
   │           short (past 1 MiB, then hourly) → docker.sock bound (unless launchd's)
   │           the floor checked (macOS 13) → boot: preparing → booting → checking → up
   │           stops.wait() … then stop_in_order: the phase stopping, the last look at
   │           the ports, the guest's stop (90 s; a disk check never cut), forced at the end
   ├─ stops      a sigwait thread: a signal is a stop asked (the first reason kept)
   ├─ host.sock  __host-phase → "PHASE SECONDS-LEFT"; __host-stop → the stop asked, then
   │             the phase (no PID signalled); the uid checked, 16 at once
   ├─ vm.sock    the CLI's commands, joined to the guest's agent over vsock
   ├─ docker.sock  Docker's API, relayed raw to the guest's service
   │             vm.sock + docker.sock + the forwarded ports: one budget of 1,024
   │             open, the ports at most 768 of it; every accepted connection blocking
   ├─ the poll   __guest-ports 3 every 2 s (10 s while nothing runs, after 30 s settled,
   │             never past the idle limit); listeners per (protocol, address);
   │             comes-back.json on change, and once more before the stop
   ├─ UDP        a thread a published UDP address: one stream to the guest, a peer ID
   │             for each Mac peer (256 at most, 60 s idle)
   ├─ liveness   __guest-boot every 10 s: another ID (a panic's reboot: panic=5) → an
   │             ordered stop; 6 misses in a row → stopped at once
   ├─ console    past 4 MiB, its last 4 MiB copied to console.log.1, truncated
   └─ idle       5 minutes with no client and no container up → the stop
```

The guest's clock needs no thread of its own: every poll is a `Request` stamped with the
Mac's time, and the agent sets the guest's clock from it once the guest is ready.

### The guest

```
PID 1 (__guest-init)   stop signals blocked; /proc, /sys, …; the modules; the network
   │  the agent's vsock listener bound and listening (AGENT_PORT), kept for the guest's
   │  life (AGENT_LISTENER); a failure powers off, said
   │  the agent started (the listener at descriptor 3: AGENT_FD; close-on-exec otherwise)
   │  the data disk checked and mounted (the agent says NOT_READY until then)
   │  the shares: each a virtiofs tag at its Mac path (nosuid,nodev), shallowest first;
   │  the VM's own places hidden in any share; /tmp and /var/folders bound over from
   │  /private/…; the guest's own temporary files in /run/tmp (TMPDIR)
   │  Rosetta, when the command line says shipyard.rosetta=1: the share read-only at
   │  /run/rosetta, binfmt_misc, Apple's entry with flags CF (the interpreter opened now)
   │  shipyard system recover
   │  at stop: system stop, the shared listener shut, FITRIM the data disk, sync, unmount
   ▼
the agent (__guest-agent)   takes descriptor 3 only if it's the agent's socket, else
   │  refuses to run; a hello read within 10 s; a request stamped over a minute ago in
   │  its first minute refused, never followed (waited_too_long)
   ├─ a user command → Welcome, then Session(token) before any output, then its frames;
   │                   the token registered (SESSIONS) for the command's life
   ├─ __guest-control TOKEN → Signal and Resize frames applied at once (killpg the group
   │                   or the terminal's foreground group; TIOCSWINSZ); its end no hang-up
   ├─ __guest-boot   → the boot ID, answered before the READY check
   ├─ __guest-ports N → the published ports at version N (1, 2 or 3)
   ├─ __guest-forward IP PORT, __guest-forward-udp IP PORT → the container's own address
   │                   (its lease), looked up again every 2 s for UDP
   └─ the API        → Welcome, Session(token): a hang-up names the connection by it
```

The guest's firewall (ruleset v9) drops, in the forward chain and before DNAT's accept,
anything `ct status dnat` that came in on `eth0` (the `outside` set, filled only in a
guest Shipyard booted): no other VM on the Mac's NAT reaches a published port. The Mac
reaches them over vsock, dialled at the container.

### The control connection

```
Mac: shipyard run -i …             guest agent
  vm.sock ── HelloRun, Request ──▶  Welcome, Session(token)        (protocol 6 only)
  vm.sock ── __guest-control TOKEN ─────▶ control: Welcome
     Ctrl-C, SIGTERM ── Signal ──▶  killpg(group or foreground group)
     SIGWINCH ── Resize ──▶         TIOCSWINSZ (also in-band: typing never before its size)
     a hang-up ── Signal(SIGHUP) ─▶
  the command's own connection: its input, its output, its end, as before
```

A version-5 client never gets a `Session`, and an older guest never sends one: the Mac
then passes signals in-band, as before. Internal requests (`__guest-…`, `__host-…`) never
get one (`is_internal`, one rule on both sides).

### Files and wires

| File or wire | Version | Notes |
|---|---|---|
| Attach protocol | 6 | `Frame::Session(u128)`, kind 26; an older reader skips an unknown kind |
| The forwarded `Request` | 3 (stage 12's) | unchanged; a newer one refused with `Decline::Newer` (code 3) to a v5+ client, the words to older ones |
| The ports answer (`__guest-ports`) | 3 | v2: UDP listed with its protocol (13.5a); v3: `comes_back` (13.6b2); answered at the version asked, v1 without an argument (an older host's) |
| `ports.json` | 2 | `forwarded` and `unbound` with their protocols; v1 read |
| `vm.json` | 1 | `memory_mib`, `cpus`, each optional; bounded; a newer or unknown one refused, the default used and said |
| `network.json` | 2 | the card's address and its directory's device and inode; a v1 file adopted for its directory; another directory's gets a new address; the VM tests' slot cards kept v1 |
| `booted.json` | 1 | the host's PID, the guest's revision and binary sha256, the host's revision, the home and guest directory (optional: added in 13.6b1, unreleased); read only while that host runs; removed at its end |
| `comes-back.json` | 1 | `{version, containers}`, written only from a v3 answer, kept across stops, 0600 |
| `host.sock` | the forwarding wire | `HelloRun` and `Request::internal`; two requests only, anything else refused with a code |
| The nft ruleset | 9 | the `outside` set and its rule (13.5b) |
| The kernel command line | — | `shipyard.shares=` (hex, tags `share0`…), `shipyard.rosetta=1`, `panic=5`; refused past 2,048 bytes |
| The launchd plist | — | `SHIPYARD_VM_AT_LOGIN=1` and `RunAtLoad` with `--at-login`; `ExitTimeOut` from `LONGEST_STOP` (870 s) plus 60 s |

### The VM directory

| Place | What |
|---|---|
| `vm.lock` | the host's liveness (`flock`) and its PID, written after `host.sock` is bound: a PID with no socket is an older host's |
| `host.sock` | the host's own socket, from its start to its end, booting or not |
| `vm.sock`, `docker.sock` | as in stages 10 and 11; `docker.sock` launchd's after `vm install` |
| `vm.json`, `network.json`, `ports.json`, `booted.json`, `comes-back.json` | above |
| `start-asked`, `just-installed` | marks: a CLI's start through launchd (taken by the host with the lock); `vm install`'s own run at load, which boots nothing |
| `host.log`, `host.log.1` | the host's, moved aside by the host itself (0600) |
| `console.log`, `console.log.1` | the guest's console, capped at 4 MiB while it runs |
| `data.img` | sparse, 64 GiB; trimmed at each stop |

## Decisions and why

**The stage (2026-10-26, asked):**

- **D13-1: the Mac's `shipyard` keeps forwarding; its wire is fixed.** The move onto the
  API (D11.5b-1) would have been a second CLI backend made from Docker's JSON: about six
  endpoints of Shipyard's own, every command's output made again. Instead: signals,
  resizes and hang-ups on a channel of their own (F94, back from 15), an older guest's
  refusal said plainly (F167), F155, F147.
- **D13-2: Rosetta when asked, or when there's no arm64.** `--platform linux/amd64` asks;
  an image with no arm64 variant is pulled as amd64 with a warning (Docker Desktop's
  rule).
- **D13-3: a stable local signing identity now;** Developer ID signing and notarization
  scripted, run at release (stage 15: a paid account). A deployment target of macOS 13.
- **D13-4: F59, F69 and F77 to stage 15:** not Mac work.

**13.2, the VM (2026-10-26):**

- **D13.2-1 (asked): the VM's size from the Mac.** Memory a quarter of its RAM, 4 to
  16 GiB (6 GiB on this 24 GB Mac); CPUs half its cores, 1 to 8 and at most the
  framework's `maximumAllowedCPUCount`. `vm set` keeps them in `vm.json`, taken at the
  next start. A fixed 4 GiB was too much for an 8 GB Mac and too little for a 64 GB one.
- **D13.2-2 (asked): no balloon.** 13.1 and two more runs (with `init_on_alloc=0`) gave
  macOS nothing back; a device that does nothing would be a claim the docs can't make.
  F168 records the runs, to retry.
- **D13.2-3 (asked): TRIM at each stop,** not online `discard`: one `FITRIM` after the
  containers end costs nothing on the write path (67 GB of free space trimmed in 39 ms).
- **D13.2-4 (asked): F113 said, not copied out;** the copy-out to stage 15.
- **No new clock channel** (found by the survey): the ports poll already carries the
  Mac's time every 2 s. 13.2 proved it with a VM test instead.

**13.3, Rosetta (2026-10-26, defaults):** what runs here is the host's platform, then
amd64 where Shipyard's own Rosetta entry is registered (its interpreter
`/run/rosetta/rosetta`): a Lima VM's entry, or a qemu handler on Linux, isn't taken. One
rule at every read of the store, not only at the pull: before it, an amd64 image would
have vanished from `images` and been refused at every start. No `ImageRecord.platform`:
the record already holds its platform's manifest digest. Rosetta's cache left for F169.

**13.4, file sharing (2026-10-26):**

- **D13.4-1 (asked): Docker Desktop's set, at the Mac's own paths:** the home, `/Volumes`,
  `/private/tmp`, `/private/var/folders`, a virtiofs device each (its own request queue);
  `/tmp` and `/var/folders` bound over in the guest, so a path means the same thing from
  the Mac's CLI and from a Docker client, with nothing rewritten. Fixed, no `--share`.
- **D13.4-2: virtiofs isn't tuned:** there's nothing to tune (13.1). A budget keeps it
  from getting worse; F172 the bar.
- **D13.4-3: one rule for both doors:** the guest's engine refuses a bind source outside
  the shares, or in a hidden place, for the API and forwarded commands alike (H1); the
  Mac's CLI keeps its earlier check against the same list. Never the engine's own
  creates (the builder's internal binds: the plan review's critical finding).

**13.5, ports (2026-10-26):**

- **D13.5-1 (asked): loopback only stays;** F98 gets a warning instead.
- **D13.5-2 (asked): split** into a (UDP, dialling the container) and b (F108, F98).
- **One stream per published UDP address, not per peer** (the plan's Murphy review, two
  high): a peer ID in each frame (`[u32 peer][u16 length][bytes]`), a connected socket per
  peer in the guest. No thread or vsock connection per Mac peer, so a DNS stub's port per
  query costs a socket, nothing else.
- **The port held from boot** (13.5b): PID 1 binds the agent's vsock port before it starts
  anything and hands it to each agent. A guest that can't bind it powers off: without
  its agent it can do nothing.
- **The guest drops published ports from its NAT link** (13.5b): new and established
  alike; the Mac never needs that path. Containers still reach the Mac's own services at
  192.168.64.1 (Docker Desktop's `host.docker.internal`), said in `SECURITY.md`.

**13.6, the wire and the lifecycle (2026-10-26/27):**

- **D13.6-1 (asked): split** into a (the wire) and b (the lifecycle).
- **D13.6-2 (asked): the login start, opt-in** (`vm install --at-login`).
- **A control connection beside the command's,** named by a random 128-bit token the
  agent sends right after its welcome, single-use, kept for the command's life. Its end
  is no hang-up (the plan review: a control the Mac gave up on would have hung up a
  healthy command); the Mac sends `Signal(SIGHUP)` on it instead.
- **No cap on the agent's connections in 13.6a** (the plan review, high: it would lock
  out the boot asks, stops and hang-ups behind forwarded ports): F177.
- **D13.6b-1 (asked): b split** into b1 (start and stop) and b2 (sockets and login).
- **D13.6b-2 (asked): the guest found beside the binary,** as a fall-back; the install
  step to stage 15 (F178).
- **D13.6b-3 (asked): F152(b)** (the idle window's 503) said in the docs, not built.
- **A host socket of its own, never a PID** (13.6b1): a name you can connect to beats a
  number that's reused. The host's phases (`preparing`, `booting`, `checking`, `up`,
  `stopping`) answer with their remaining bound, and the CLI waits by that, never by a
  bound of its own: a healthy VM is never stopped by a CLI giving up.
- **The stop signals blocked from the host's first line,** so none kills it before its
  stop runs; a stop asked during a boot is taken once the agent answers, and during a
  disk check waits for it (PID 1 holds the stop: 13.2's high finding).
- **What comes back is the guest's own rule** (13.6b2): a running container counted as the
  host's stop leaves it (exit 143, not stopped by its user), so `on-failure` counts as the
  recovery starts it; one whose last start failed (125) isn't. The count goes over the
  ports answer, v3, and only a v3 answer writes it.
- **One budget for the host's sockets,** with 256 kept for `vm.sock` and `docker.sock`:
  a load on a published port never shuts out `shipyard` or `docker`.

**13.7, signing (2026-10-27):**

- **D13.7-1 (asked): a script makes the identity** (`make-signing-identity.sh`), run by
  the user against the login keychain; the step tests it only in a throwaway keychain.
- **Nothing trusted** (its probe, deviating from the design): `codesign` signs with an
  untrusted self-signed identity, and the designated requirement (`certificate leaf =
  H"<sha1>"`) is the same across rebuilds, which is what TCC keys on. So no
  `add-trusted-cert`, no change to the user's trust settings.
- **No hardened runtime locally** (13.7's Murphy review): it stopped `lldb` attaching.
  Virtualization.framework needs only the entitlement. `notarize.sh` adds it for a
  release.
- **The key's partition list set once by the script** (the Murphy review): otherwise a
  keychain dialog would hang every background build.

**13.8 (2026-10-27, asked):**

- **D13.8-1:** split into a (F132) and b (the close).
- **D13.8-2:** F132 bounded and measured; the KASAN kernel and an upstream report to 15.
- **D13.8-3:** the milestone's hands-on parts a checklist the user runs
  (`milestone-13.sh`).
- **D13.8b-1:** F92 fixed in the close: `build.sh` checks every installed package, its
  dependencies too, against a full lock (`scripts/guest/alpine.installed`: each one's name,
  version and identity in apk's database, the SHA-1 of its signed control segment, which
  holds its data's SHA-256); a build that installs anything else fails, saying what.
  The manifest already records each one's name and version.
- **D13.8b-2:** two whole-stage reviews (clean-code, Murphy), then three fresh-eyes ones
  (the VM and its host; the guest and the wire; security).

## Kernel and framework checks

**`scripts/probes/vz-probe/`** (13.1, 2026-10-26): `probe.swift` boots the guest's kernel
(6.18.58) with its initramfs and a second one appended (`probe-init`, `probe.sh`), a
virtiofs share `probe` for markers; on this Mac (macOS 27.0.1, Apple silicon, 4 CPUs,
4 GiB), run twice, the second run's times by `/proc/uptime`.

| Check | Result |
|---|---|
| Rosetta's share | `availability` installed; the share holds `rosetta` and `rosettad`; binfmt registered with Apple's magic and mask, flags `CF` |
| amd64 under Rosetta | static busybox `uname -m` → `x86_64`; a Rust program's 8 threads (musl's `clone`) ran and joined; `clone3` → `ENOSYS` (glibc falls back); `execveat` there; ~3 ms an exec against ~0.15 ms native (2,000 `busybox true`: 5.95 s against 0.29 s) |
| The traditional balloon | inflates (MemFree 3.89 GB → 0.74 GB in 5 s at a 1 GiB target), but the VM's footprint *rose* to 3,484 MB and stayed; with `init_on_alloc=0` it held at 2,448 MB; freed guest memory never returned by itself (F168) |
| TRIM | `discard_max_bytes` 4 GiB, granularity 512; 1 GiB written and deleted left the image at 1,051,948 KiB until `fstrim` (3,368 KiB); mounted `discard`, 3,484 KiB |
| virtiofs | 10,000 small files: written 2.45 s (ext4 in the VM 0.11, OrbStack 0.68), removed 1.15 s (0.06, 0.49); `find` 0.07 s; 1 GiB written 0.67 s, read 0.25 s: on par |
| The clock | a 60 s pause moved the guest's wall clock 0 s; 5 s after the resume it was still ~60 s behind |

The seccomp question (does a filter see arm64 calls under Rosetta?) was checked in 13.3's
VM test, with Shipyard's own runtime and its default profile: an amd64 static busybox
runs. Rosetta translates in user space; the kernel sees arm64's calls.

**`/Volumes`** (13.4, probed before building): a share of `/Volumes` reaches the volumes
mounted under it, those attached after the boot too (a disk image attached 15 s into the
boot was read and written). Files show as `root:root` in the guest. A directory has one
dentry in the guest whatever case names it, so a hiding tmpfs holds for every spelling
(the plan review's case-variant claim was false: tested).

**BSD's bind rules** (13.5b, probed on this Mac before building F98's probe): a wildcard
bind with `SO_REUSEADDR` beside the VM's own `127.0.0.1:P`, bound without it, succeeds;
beside another program's `*:P`, or a dual-stack `[::]:P`, it's refused (`EADDRINUSE`).
Linux refuses the first too (it wants `SO_REUSEADDR` on both), so the probe and its test
are macOS-only (`a_program_on_every_address_is_found`; AGENTS.md §10).

**An untrusted identity** (13.7, in a throwaway keychain): `security find-identity -v`
hides it (`CSSMERR_TP_NOT_TRUSTED`), yet `codesign` signs with it, and two builds share
one designated requirement. A binary signed so, with the hardened runtime, boots the VM
and passes `doctor` (`the_guest_boots_runs_a_container_and_keeps_its_data`,
`doctor_checks_the_vm`).

**Found by tests, each now in AGENTS.md §10:**

- **A network can drop TLS 1.3 hellos that carry the post-quantum key share** (13.0–13.4):
  this Mac's did, to Docker Hub and Alpine's CDN, and Shipyard's own rustls sends it too;
  it passed for an OrbStack fault. The guest builds over a mirror then
  (`SHIPYARD_GUEST_MIRROR`), and the build tests waited for another network (13.5a).
- **The guest's view of a share is cached** (13.4): a change the Mac makes is seen only
  once its entry lapses, so the test swaps a link through a container, as a hostile one
  would.
- **The fuzz build is nightly's** (13.5a): `AtomicUsize::fetch_update`'s deprecation
  showed there first.
- **`run-in-linux.sh tester` gives no delegated cgroup** (13.5a): a rootless build there
  fails in BuildKit's `runc` (F174).
- **A listening vsock socket completes connections in the kernel, accepted or not**
  (13.5b): once PID 1 held the agent's port, a bare `connect` proved nothing; the host
  asks the boot ID (`wait_for_agent`).

## macOS and Linux differences

- **Linux hosts are unchanged by the bind rule:** `Shared::here` is `None` off the guest,
  so `check_binds` passes everything; the existing bind tests pass as they did.
- **The firewall's `outside` set** is filled only in a guest Shipyard booted
  (`booted_by_shipyard`, failing closed); empty on a Linux host, where its rule matches
  nothing (`a_link_outside_reaches_no_published_port` proves it works there with a link
  put in the set).
- **Another platform:** amd64 runs only where Shipyard's own Rosetta entry is registered
  (the macOS guest); on Linux `--platform` takes only the host's, and a qemu handler isn't
  taken.
- **`poll` and accept:** an accepted socket inherits its listener's `O_NONBLOCK` on
  macOS, not on Linux (F147); every accept in `shipyard-vm` sets it blocking.
- **`SO_REUSEADDR`'s wildcard rule differs** (above): F98's probe is the Mac's alone.
- **Signals on the Mac** go on the control connection to a v6 guest; on Linux the CLI
  runs the command itself, and the attach protocol's control frames are never used.
- **macOS-only code:** the host, its control, the liveness watch, the size, the shares'
  devices, UDP's listeners, the signer and the floor. Everywhere: `phase.rs`,
  `booted.rs`, `comes_back.rs`, `settings.rs`, `ports.rs`'s file (their units run on
  Linux too). Linux-only: the guest's init, agent, control and relays.

## Failure-mode register

| Case | Handling | Test |
|---|---|---|
| A Mac's sleep pauses the VM; its clock stops | the next poll (2 s) sets it to the Mac's | `a_wrong_clock_is_set_right_by_the_host` |
| A guest that panics reboots in place mid-`e2fsck` (13.2's Murphy review, high) | stopped in order, PID 1 holding the stop until the disk's check ends, within 10 minutes | `another_boot_id_is_a_reboot` (the decision); end to end none (no unprivileged trigger) |
| A guest that stops answering | 6 asks in a row unanswered (~1–2.5 min) → stopped at once; a miss then an answer resets | `only_misses_in_a_row_are_a_hang` |
| An older guest that doesn't know `__guest-boot` | the probe left off, said once; `BOOT_UNREADABLE` told apart | `an_older_guest_is_left_unasked`, `a_boot_answer_is_heard_for_what_it_is` |
| The liveness stop racing a user's stop | one owner (`stopping.swap`) | review fix (13.2) |
| An unreadable or newer `vm.json` | the host says so and takes the default; `vm set` replaces it | `a_newer_or_unknown_vm_json_is_refused` |
| A VM directory copied and run as a second VM | a new address for it, said; never two VMs on one address | `a_copied_directory_gets_its_own_address` |
| The console growing without bound | copied and truncated past 4 MiB (the framework's descriptor appends) | `a_full_console_is_moved_aside_and_written_on` |
| A host build handed an amd64 base by the store (13.3's Murphy review) | store images given to BuildKit only of the build's own platform, each one's config read | review fix; `a_request_is_buildctl_s_arguments` (the platform named) |
| A stored image of another platform at a start or a create | the other-platform error classed once: a silent re-pull for a pull, a 404 in the API | `another_platform_is_told_from_other_failures` |
| A VM without Rosetta hiding amd64 images | views (`images`, `inspect`, `ps`) by any platform | review fix (13.3) |
| Rosetta's share failing at boot | the VM boots without it, said; amd64 refused with the hint | review fix (13.3) |
| A Lima VM's own `rosetta` entry | not taken: only Shipyard's interpreter path | `only_shipyard_s_rosetta_counts` |
| A bind outside the shares through the API (F143) | 400 "mounts denied", nothing made | `a_bind_outside_the_shares_is_refused` |
| A link in a share swapped by a container to lead out | the start re-checks a source in a share, before the plan follows its links, `/../` cleaned first | `a_bind_outside_the_shares_is_refused` (the swap through a container) |
| A link swapped in between the start's check and the mount (F173a) | the source judged once more and split at its share (`Shared::bind_of`), then reached from the share by descriptor, each component `O_NOFOLLOW`, a file or a directory, never a link (`clone_made`): the start fails (13.8c); a made source's directory re-rooted at its share too | `a_shared_bind_source_is_bound_by_descriptor` (runtime, Linux: a file, the whole share, a link on its way and at its end), `a_bind_is_reached_from_its_share` |
| A container streaming UDP to a Mac peer | the relay takes at most a round of answers between looks at its stop (13.8c) | `a_flooding_guest_never_holds_a_stop` |
| Heavy UDP traffic both ways | neither relay waits on the stream: an `Outbox` written non-blocking, an `Inbox` read so, whole frames handed on, a frame unfinished past 5 s a broken stream; a datagram with no room dropped (13.8c) | `a_guest_that_writes_without_reading_keeps_its_stream` (Mac), `a_mac_that_writes_without_reading_keeps_the_relay` (guest), `an_outbox_never_waits_and_loses_no_frame_it_took`; the `udp_frames` fuzz target feeds `Inbox` in any pieces |
| The host killed while the first boot formats its disk | `data.img.new` kept as `data.img` only once the guest answers (ready), or at an ordered stop that finds it ready; else made anew at the next start (13.8c) | `a_new_disk_is_kept_only_once_ready`, `a_new_disk_is_renamed_once`; the second's window: F188 |
| A build stage of another platform (`FROM --platform=`) naming a store image | each store image given for its own platform alone (`context:NAME::linux/arm64`), never plain (13.8c) | `a_local_image_is_built_from_without_a_pull` (another platform's stage pulls and fails; `$BUILDPLATFORM` builds), `a_platform_is_named_as_buildkit_names_it` |
| `save` or a stopped container's `cp` of an amd64 image once Rosetta is lost | read for any platform (it runs nothing) (13.8c) | `a_save_reads_an_image_of_any_platform` |
| A bind of the VM's own files (`data.img` under `$TMPDIR`) | hidden in every share, in any letter case; a bind of a hidden place refused, not merely empty | `the_vms_own_places_stay_hidden` |
| A share's device that failed to mount | judged by the mount the path lands on, not by its text | `the_macs_shares_are_told_by_their_mount` |
| An unreadable kernel command line in the guest once ready | the rule fails closed | `a_guests_shares_are_read_from_its_command_line` |
| A source holding a hidden place (`-v /tmp:/t`) | the hidden place must be covered | review fix (13.4); `the_vms_own_places_stay_hidden` |
| A socket as a bind source | refused | `a_bind_is_let_through_only_from_a_share` |
| A guarded folder macOS hasn't allowed | named with its permission, in the refusal and in `doctor` | `a_guarded_folder_names_the_permission`, `the_privacy_line_names_what_macos_holds_back` |
| The kernel command line too long for the shares | refused past 2,048 bytes | review fix (13.4) |
| An older host reading a v2 ports answer (13.5a's clean-code review, high) | the host asks `__guest-ports 2`; without the argument the guest answers v1, `udp` always written | `an_older_host_is_answered_at_v1`, `the_answer_is_at_the_version_asked` |
| A container taking another's address while a UDP relay is open (the plan review, high) | the target looked up at each new socket and every 2 s; a changed target drops the sockets | `datagrams_reach_the_container_and_come_back` |
| A flood of spoofed UDP sources from a Mac program | 256 peers per address (the least recently used let go), 60 s idle; 4,096 sockets across relays in the guest | `peers_are_remembered_while_heard` |
| UDP relays blocking each other | one poll loop; the stream's waits bounded at 2 s | review fix (13.5a) |
| The Mac relay's shared buffer cutting the next datagram (found in testing) | separate buffers | `each_peer_gets_its_own_replies_over_one_stream` (each round grows) |
| A failed UDP join or write | retried after 1 s | `a_broken_stream_is_joined_again` |
| A UDP port another program holds on every address | "in use"; bound once without `SO_REUSEADDR`, retried at 20/50/120 ms (F98's probe's instant) | `a_udp_port_held_on_every_address_is_busy` |
| A UDP publish no lease explains | refused (the monitor's held socket would swallow it); TCP falls back to the guest's DNAT, said once | `the_container_is_found_by_its_lease` |
| A container with vsock (`seccomp=unconfined`: the builder) binding the agent's port while it restarts (F108) | PID 1 holds the port from before any container to the guest's end | the guest `doctor`'s `agent port` line; the restart itself: F176 |
| A command that waited in the backlog while the agent restarted | refused past a minute in the agent's first minute, before any clock follows it (13.5b's Murphy review: its stamp set the clock back first) | `a_request_that_waited_too_long_is_refused` |
| A boot's bare `connect` succeeding with no agent | the host asks the boot ID | `wait_for_agent`; the VM suite's boots |
| Another VM on the Mac's NAT reaching a published port | dropped by `iifname @outside ct status dnat drop` | `a_published_port_doesnt_answer_on_the_vms_own_address` (with the set empty, the guest's address answered: fails for the right reason) |
| Another Mac program on every address shadowing a forwarded port | said by `vm status` and `doctor` | `a_mac_program_on_a_forwarded_port_is_said` |
| Signals queued behind input the command never reads (F94) | the control connection, read on its own thread | `a_signal_reaches_a_command_that_reads_no_input`, `a_signal_passes_a_paste_the_command_never_reads` |
| The token sent after output frames (13.6a's Murphy review, high) | registered and sent before any output pump starts | `a_session_token_follows_a_version_6_welcome` |
| A control the Mac gave up on (the plan review) | its end is no hang-up | `a_command_is_found_by_its_token_while_it_runs` |
| A signal after the command ended | none sent | `an_ended_command_is_signalled_no_more` |
| An internal request waiting for a token | `is_internal` on both sides | `the_hosts_own_requests_are_internal` |
| A client that sends no hello | given up after 10 s | `a_silent_client_is_given_up_on` (the helper); through the real agent none |
| A guest of another build refusing a newer request (F167) | `Decline::Newer`'s code; the same words on a terminal | `another_builds_vm_is_said` |
| An API hang-up landing on a connection that reused the port (F155) | named by the token | `an_api_connection_is_named_by_its_port_and_its_token` |
| A relay cut a minute after one side finished while the other still sends (F109) | the linger from the last byte, 10 minutes at most | `a_side_still_sending_is_not_cut` |
| A session ending as the grace's SIGTERM fires, exit 143 (F109) | `over` and the SIGTERM under one mutex | review fix (13.6a) |
| A foreground `run -i` whose container ended with input queued (found by 13.6b1's verify) | the queued input dropped, the exit the monitor sent read | `an_exit_sent_before_the_monitor_closed_is_read` |
| A SIGTERM before the host blocked its signals | blocked on the main thread before any other exists | `a_stop_is_asked_once_and_wakes_its_waiter`; `a_logout_stops_the_vm_in_order` |
| A stop during a start undone by the waiting start (13.6b1's Murphy review, high) | the stopped host exits 3 (`STOPPED_BEFORE_SERVING`); the start says "the VM is stopping", never a second start | `a_stop_during_a_start_succeeds` |
| `vm stop` signalling a reused PID (F93) | it asks `host.sock`; the PID only for an older host with no socket | `the_host_socket_answers_its_asks` |
| `vm stop` waiting by one snapshot | it asks again past each wait, within 20 minutes | `a_stop_waits_by_the_hosts_phase` |
| A first command giving up and stopping a healthy VM (F100) | acts by the host's phase: up connects again; stopping and checking wait; booting past its own bound asks a stop | `a_start_never_stops_a_healthy_host` |
| A launchd start whose run count couldn't be read (F144) | its run is the one that took this start's mark and failed; a mark it never wrote counts for nothing | `a_launchd_start_ends_by_launchds_word` |
| A second CLI rotating a running host's log (F106) | only the host rotates its log | `a_big_log_is_moved_aside` |
| A logout's stop cut by launchd | `ExitTimeOut` past `LONGEST_STOP` | `a_logout_stops_the_vm_in_order` |
| A connection non-blocking after launchd's socket was refused (F147) | every accepted connection set blocking; `refuse_waiting` polls and changes no flag | `an_accepted_connection_is_blocking` |
| A load on a published port shutting out `shipyard` and `docker` (F152c) | one budget, 256 kept for the sockets; slots and budget taken together | `the_budget_keeps_room_for_the_sockets` |
| `vm install`'s own run at load booting the VM (the plan review) | its `just-installed` mark, taken fresh within 150 s, boots nothing | `the_login_start_is_asked_by_one`; the VM mutant (13.6b2) |
| An older guest's silence setting the count to 0 | only a v3 answer writes it | `an_older_answer_keeps_the_count` |
| The stop's last look with the phase still up (13.6b2's Murphy review) | stopping said first, the look within `LAST_LOOK` | review fix (13.6b2) |
| The quiet poll outrunning a short idle limit (found by the VM tests) | the wait bounded by the idle limit | `commands_racing_the_idle_stop_all_run` |
| Another user's guest in `/tmp/guest` beside a binary (13.6b2's Murphy review) | taken only if it, and its directory, are the user's alone | `the_guest_is_found_beside_the_binary` |
| An older macOS | refused by name at the client's start, the host's boot and `doctor`; `minos` 13.0 lets dyld refuse first | `an_older_macos_is_refused_by_name` |
| The signing key's dialog hanging every background build (13.7's Murphy review) | the key's partition list set once by the script, at the terminal | by hand in a throwaway keychain; no test (a locked keychain prompts) |
| A local identity that can't sign | `sign.sh` falls back to ad hoc, said | none (a locked keychain prompts, which a test mustn't risk) |
| A VM test whose guest died waiting for ever (F132) | every command through `bounded` or `finish_within` (300 s), failing with the console's last lines | `a_child_past_its_limit_is_killed` |
| A rebuilt `shipyard` with a stage-12 guest binding the guest's own `/tmp` (the close's Murphy review, M1) | a command naming a place beyond the home is a request of version 4 (`beyond_home`), which an older guest refuses as another build's; a host with no `host.sock` said as an older build's | `only_paths_in_the_shares_are_forwarded`, `a_request_round_trips_and_its_limits_hold` |
| A stage-12 launchd agent's 120 s `ExitTimeOut` cutting a 0.5 stop (M2) | `vm status` and `doctor` say an agent an older shipyard installed; the upgrade written down (`macos.md`) | `the_agent_says_what_runs_and_what_it_holds` |
| A hung verdict during the disk check cutting `e2fsck` at 90 s (M3) | the watch's stop bound by the phase (`watched_stop_limit`) | `a_watched_stop_never_cuts_a_disk_check` |
| The liveness watch's stop still letting clients in to a guest being stopped (the fresh VM review) | `close_doors`, as a user's stop: clients refused as stopping, `vm.sock` removed | review fix (no unprivileged trigger for a hang) |
| A command refused as stopping giving up at 150 s while the host waits out a disk check | `waited_on`, shared with `vm stop`'s wait: the host's own bound, to the longest stop | review fix |
| A launchd host finding the lock held shared by a `vm status` for an instant, refusing every waiting `docker` | the lock tried again for 200 ms (`LOCK_TRIES`) | review fix (a microseconds' race) |
| `vm stop` signalling a PID when the ask of a current host failed | asked again, else waited for; only a host with no `host.sock`, still the lock's, is signalled | review fix |
| `make-signing-identity.sh` setting the partition list of every signing key in the keychain, a Developer ID's among them (the fresh security review) | this key alone, by its label and kind; the keys changed counted and said | `test-sign.sh` (a bystander key beside it; its mutant, the old call, fails "2 key's") |

## Fuzzing

- **`udp_frames`** (new, 13.5a): any bytes as the guest's frames read by
  `read_datagram` (the host reads what the guest sends): a peer ID, a length bounded at
  65,535, a short frame an error. Seeded in `scripts/fuzz-seeds.py`. 13.5a ran every
  target, 18 of them, 30 s each: clean.
- **Not fuzzed, argued:** `vm.json`, `booted.json`, `comes-back.json`, `network.json`
  (serde JSON, versioned, bounded reads, the user's own files); the ports answer (serde
  JSON from the guest's agent, bounded); the host's phase answer (two words, parsed by
  `PhaseAnswer`, unit-tested); the kernel command line's keys (written by the host,
  read by the guest's PID 1).
- **The close's run** (2026-10-27, `scripts/test-linux.sh --fuzz 300`): every one of the 18 targets for 300 s, 269 million runs in all, `udp_frames` (13.5a's) among them: no crash, no deprecation warning in the nightly build.

## Known limitations (by design, for stage 13)

- **Memory isn't given back to macOS while the VM runs** (F168): the VM holds what its
  containers touched until it stops; `vm set` is the lever.
- **Small files through a share** cost ~3.6× OrbStack's time to make and ~2.3× to
  remove (F172); reads, stats and large writes are on par.
- **A change on the Mac isn't seen as it happens** by a program waiting on it in a
  container: it has to poll.
- **Rosetta translates every amd64 program at its start** (~3 ms; F169); a glibc amd64
  program under the default seccomp profile is untested, and a container whose image
  needs a Rosetta since lost retries for good (F170).
- **A bind of Docker's socket is refused** on a Mac (F171): Ryuk needs
  `TESTCONTAINERS_RYUK_DISABLED=true`.
- **A VM an older `shipyard` started** may bind a `/tmp` path as its own (F173b, said);
  `/Volumes` reaches Time Machine's local snapshots (F173c).
- **The guest's NAT link** still reaches host-network containers and its own services
  (F175).
- **The agent's connections have no bound** (F177).
- **No install path but a checkout** (F178): the guest is found beside a binary built in
  the tree, or in the VM's directory.
- **The login start** rests on launchd's `RunAtLoad`, checked in the VM tests by
  `bootout` and `bootstrap`; a real logout and a reboot are the milestone's (F178).
- **Survivors and no-tests, each argued:** the liveness verdicts end to end (no
  unprivileged trigger: the decision unit-tested); a resize reaching a terminal whose
  input is stuck (the guest's own `run -it` passes it among its typing; the routing
  unit-tested); the hello deadline through the real agent; the agent's restart (F176);
  the guest's `/proc/meminfo` after `vm set` (it needs a pull); "nothing made in the
  guest" after a refused bind (no way to look without a bind the rule refuses); the
  signing fallback (a locked keychain prompts); the folder permission kept across a
  rebuild (by hand).

## Carried forward

- **Closed in stage 13:** F90, F103, F138 (13.2); F143, F105 (13.4); F96, F97 (13.5a);
  F108, F98 (13.5b); F94, F155, F167 (13.6a); F93, F100, F144, F109 (13.6b1); F147,
  F152, F106 (13.6b2, F106's other parts in 13.6b1); F140 (13.4's doctor and refusal
  parts, 13.7's identity). F92 in the close (D13.8b-1). F173a in 13.8c (the readiness
  review's fixes).
- **To stage 15** (hardening): F59, F69, F77 (D13-4); F113 (D13.2-4); F132 (D13.8-2:
  if the crash comes back, the same runs, a KASAN kernel, a report upstream with the
  consoles); F168 (the balloon); F169 (Rosetta's cache); F170 (the restart loop on a lost
  Rosetta; glibc amd64 under seccomp); F171 (Docker's socket bound); F172 (virtiofs'
  speed); F173b–d (an old VM's `/tmp`; snapshots; `doctor`'s waits);
  F174 (a rootless build without a delegated cgroup); F175 (an input rule on the guest's
  NAT link); F176 (the agent's restart tested); F177 (a bound on the agent's connections,
  room kept for the host's own; the group signalled through a pidfd); F178 (an install
  step; two log lines; an old count; a budget refusal's 503); F179 (consolidations); F180
  (a hung agent's stop waited for its bound; `network.json` one way; the signing key's
  reach; Docker's API through an older guest; the fresh reviews' lows); F181 (a build's
  context beyond the home refused); F182–F188 (the readiness review's lows, 13.8c).
- **Decisions to revisit:** the balloon on a later macOS and under real memory pressure
  (F168); a share of a project's hot tree of its own (F172); a seccomp profile of
  Shipyard's own for the builder instead of unconfined (from stage 12, still untested).

## Tests

New and grown test files, counted by test name in the tree on 2026-10-27, against stage
12's merge (`9088094`). A file whose count fell lost tests to a move, not a deletion.

| File | Tests | Stage 13's |
|---|---|---|
| `shipyard-cli/tests/vm.rs` | 30 | 16 new (14): 13.2 `a_wrong_clock_is_set_right_by_the_host`, `a_stop_gives_freed_space_back`, `the_vm_takes_its_size_from_vm_set`; 13.3 `an_amd64_image_runs_through_rosetta`; 13.4 `a_bind_of_tmp_is_the_macs`, `a_bind_outside_the_shares_is_refused`, `the_vms_own_places_stay_hidden`, `small_files_on_a_share_stay_within_a_bound`; 13.5a `a_udp_port_answers_on_the_macs_loopback`, `a_tcp_port_is_reached_at_the_containers_own_address`; 13.5b `a_published_port_doesnt_answer_on_the_vms_own_address`, `a_mac_program_on_a_forwarded_port_is_said`; 13.6a `a_signal_reaches_a_command_that_reads_no_input`, `another_builds_vm_is_said`; 13.6b1 `a_stop_during_a_start_succeeds`; 13.8a `a_child_past_its_limit_is_killed` (`vm_support`'s, run here) |
| `shipyard-cli/tests/vm_launchd.rs` | 5 | 2 new (3): `a_logout_stops_the_vm_in_order`, `the_vm_starts_at_login_for_containers_that_come_back` |
| `shipyard-cli/tests/vm_terminal.rs` (`harness = false`) | 7 | 1 new (6): `a_signal_passes_a_paste_the_command_never_reads` |
| `shipyard-cli/tests/vm_docker.rs` | unchanged | its waits bounded (13.8a) |
| `shipyard-cli/tests/firewall.rs` | 20 | 1 new (19): `a_link_outside_reaches_no_published_port` (root) |
| `shipyard-cli/tests/image.rs` | 25 | 1 new (24): `a_platform_that_doesnt_run_here_is_refused_first` |
| `shipyard-image/tests/pull.rs` | 38 | 2 new (36): `an_image_is_pulled_for_the_first_platform_that_runs_here`, `another_platform_is_told_from_other_failures` |
| `shipyard-vm/src` | 89 | 52 at stage 12: `settings/tests.rs` 5, `phase.rs` 3, `control.rs` 4, `liveness.rs` 3, `booted.rs` 3, `udp.rs` 3, `comes_back.rs` 2, `serving.rs` 2, `shares.rs` 1; `forwarding.rs` +4, `doctor.rs` +3, `host.rs`, `network.rs`, `paths.rs`, `ports.rs` +1 each; `client/host/tests.rs` extended |
| `shipyard-engine/src` | 343 | 324 at stage 12: `guest/shares.rs` 4, `guest/control.rs` 4 (2 moved from `agent.rs`), `guest/forwarded.rs` 2, `guest/doctor.rs` +3, `guest/agent.rs` +2 (4 new, 2 moved out), `guest/init.rs` +1 (3 new, 2 renamed), `image/import.rs` +2, `guest/ports.rs` +1, `run/recover.rs` +1; `build/builder.rs` −1 (its Mac-home test now `guest/shares.rs`'s `the_macs_shares_are_told_by_their_mount`) |
| `shipyard-runtime/src` | 486 | 479 at stage 12: `attach/forward.rs` +4, `attach/client.rs` +3 |
| `shipyard-sys/src` | 62 | 60: `relay.rs` +2 |
| `shipyard-image/src` | 210 | 209: `manifest.rs`'s `the_first_choice_the_index_has_is_taken` |
| `shipyard-cli/src` | 140 | 140: `forward.rs`'s two home tests renamed and widened to the shares |
| `shipyard-network/src` | 268 | 268: `ruleset/tests.rs`' two order tests and `linux_tests.rs` extended for v9 |
| `scripts/macos/test-sign.sh` | 1 script | new (13.7), in `verify.sh --mac` |

**Totals per step** (`verify.sh`, the parts each step ran):

| Step | macOS passed / failed | Linux both passes | VM |
|---|---|---|---|
| 13.1 | — (no product code) | — | — |
| 13.2 | 1,540 / 0 | not run (guest code and a constant only); clippy and the guest's 22 units | the 4 new and every non-pulling test pass; every pulling one failed on the network |
| 13.3 | 1,546 / 0 | 4,815 / 15 (all 15 the build tests: BuildKit's Docker Hub pull, the post-quantum hello) | 7/7 with Rosetta required |
| 13.4 | 1,556 / 0 | 4,832 / 14 (the build tests, the same) | 15 / 14 (every failure a pull from Docker Hub) |
| 13.5a | 1,566 / 0 | 4,847 / 15 (the build tests, before the network changed); then build tests root 11/11, rootless 10/10 | 37/37 (after one wording fix) |
| 13.5b | 1,569 / 0 | 4,867 / 2 (the BSD probe's test, made macOS-only); build tests tester 10/10, root 11/11 | 38 / 1 (the clock test, fixed: 4/4 after) |
| 13.6a | 1,578 / 0 | 4,891 / 0 (the build tests among them) | 42/42 |
| 13.6b1 | 1,588 / 0 | 4,899 / 0 | 43/44, the attach fix, then 44/44 |
| 13.6b2 | 1,597 / 0 | 4,909 / 0 (after a Linux-only clippy fix) | 45/45 |
| 13.7 | 1,599 / 0, `test-sign.sh` ok | not run (nothing compiled for Linux changed) | all passing |
| 13.8a | clippy clean | — | 46/46 |
| **13.8b (the close)** | **1,603 / 0** | **4,911 / 0** (the build tests among them; rustdoc, glibc and musl clippy ok) | **46 / 0** (the guest from this tree) |
| **13.8c (the readiness review's fixes)** | **1,615 / 0** | **4,933 / 0** (the build tests among them; rustdoc, glibc and musl clippy ok) | **45 / 1**, then **46 / 0**: a test read `data.img` the moment `vm start` returned, before the host's watch kept the new disk; it waits for it now (3/3 alone, with `a_stop_gives_freed_space_back`, which had the same race) |

fmt, clippy (macOS, glibc, musl), rustdoc, `cargo deny` and the `--cfg fuzzing` check
passed at each step that ran them (rustdoc failed once in 13.4 on a public doc linking a
private item, and once in 13.6a on two links; each fixed and re-run). The build tests
owed since #20 first passed in 13.5a, on a network that carries the post-quantum hello.

**Mutants, hand-picked, controls first, per step:** 13.2 8/8; 13.3 9/9; 13.4 14/14 (one
first survived, `hidden-case-kept`: APFS resolves case on a Mac, so the test now judges
the text directly); 13.5a 12/12; 13.5b 7/7 (with `clock-back-early`, and the `outside`
set's filling checked by hand in the VM); 13.6a 13/13 (7 macOS, 4 Linux, 2 in the VM);
13.6b1 11/11 (the attach fix's among them); 13.6b2 13/13 (9 macOS, 3 Linux, 1 VM); 13.7
5/5 (three caught by `test-sign.sh`); 13.8a 1/1. In all 93/93 before the close;
the close's own 6/6 (the beyond-home flag, its version, the credential's version kept, the watched stop's bound, an older agent's plist, and the signing script's key alone: `test-sign.sh` fails "2 key's" without it). In all 99/99. 13.8c's 10/11, with
one equivalent (the end's link check, which the "a directory or a file" check now also
refuses): the old Mac and guest UDP relays put back whole, the Mac's stream left blocking,
an inbox handing on a part, an outbox losing a part, a disk kept on another refusal, a
share's last part followed, plain BuildKit keys (the build test as root: another
platform's stage built on the store's image), a save read for this platform. Two Mac
tests first survived the old relay (they paced the guest gently enough for it): now the
guest outruns the relay, and holds off reading for 2 s.

**The crash rate** (13.8a, `guest-crash-rate.sh`, 120 boots a run, the guest from this
tree, kernel 6.18.58, macOS 27.0.1): four at once with 4 GiB each (the Mac swapping),
0 kernel crashes in 119 counted; four with 1 GiB, 0 in 114; two with 4 GiB (the tests'
setting), 0 in 120. The 7 left out were the registry's failed pulls. A rate still 11.5a's
(6/227) would give 0 in 353 about once in 70. `AT_ONCE` stays at 2.

## Review history

Counts are the findings by severity. From 13.4 on, a Murphy review of each plan ran
before it was built.

| Step | What | A finding of its own reviews |
|---|---|---|
| 13.1 (no reviews: research) | the probes: Rosetta, the balloon, TRIM, virtiofs, the clock | the balloon gives macOS nothing back; a pause freezes the guest's clock; virtiofs has no knob |
| 13.2 (clean-code 15, ready after fixes; Murphy 1 high, 2 medium) | the size, `vm set`, liveness, TRIM at stop, `network.json` v2 | a panic's reboot was force-stopped mid-`e2fsck` (high): now stopped in order, PID 1 holding the stop until the check ends; the liveness stop raced a user's |
| 13.3 (clean-code ready after fixes; Murphy no high, 4 medium) | Rosetta, `--platform`, `platforms_here` | a host build was handed an amd64 base from the store; views hid amd64 images on a VM without Rosetta; a failed share stopped the VM |
| 13.4 (plan Murphy: 1 critical, 5 high; clean-code 2 medium, 10 low; Murphy no high, 6) | the shares, "mounts denied", the privacy folders | the check at the engine would have refused the builder's own binds (critical: moved to the doors); a link swapped between create and start (the start's re-check). Building found the re-check ran after the plan followed the links |
| 13.5a (plan Murphy: 2 high, 6 medium, 5 low; clean-code 1 high; Murphy no high) | UDP, dialling the container, `udp_frames` | a thread and a connection per Mac peer (high: one stream an address); a v1 answer without `udp` broke an older host (high). Testing found the shared buffer cutting the next datagram |
| 13.5b (plan Murphy: 3 medium, 6 low; clean-code 4 medium, 11 low; Murphy 1 medium) | PID 1 holds the agent's port, ruleset v9, F98's probe | a stale request's old stamp set the clock back before it was refused. The verify found the BSD probe's test failing on Linux, and the clock test meeting the first-minute rule |
| 13.6a (plan Murphy: 3 high, 6 medium, 4 low; clean-code 5 medium, 13 low; Murphy 1 high) | the control connection, `Decline::Newer`, `booted.json`, the linger | the token could follow output frames (high); a cap on the agent's connections would have locked out the host's own asks (plan, high: F177) |
| 13.6b1 (plan Murphy: 2 high, 7 medium, 6 low; clean-code 4 medium; Murphy 1 high, 3 medium) | `host.sock`, the phases, stops by asking, the host's log | a stop during a start was undone by the waiting start, and `vm stop` said it succeeded (high); the first test had accepted the restart. The verify found a lost exit with input queued |
| 13.6b2 (plan Murphy: 2 high, 5 medium, 3 low; clean-code 1 high, 5 medium; Murphy 2 medium) | the login start, the budget, blocking accepts, the quiet poll, the guest beside the binary | another user's guest in `/tmp/guest` would have booted with the home shared; the login test passed without the login boot. The VM tests found the quiet poll outrunning a short idle limit |
| 13.7 (clean-code 5 medium; Murphy not ready: 3) | the signing identity, `test-sign.sh`, `notarize.sh`, the floor | the key's dialog would hang every background build once the identity existed; the hardened runtime stopped `lldb` attaching |
| 13.8a (no reviews: bounds and a measurement) | every VM test command bounded; the crash rate | 0 crashes in 353 counted boots: no trigger to name; the script's slot cards had to stay `network.json` v1 |
| 13.8b (the close) | F92, the whole-stage reviews, these notes, fuzz, `verify.sh`, the milestone (the book's chapters skipped, the user's choice, as stage 9's) | Whole-stage clean-code: ready after fixes (docs, small consolidations; the rest F179). Whole-stage Murphy: no critical, 3 medium (an older guest binding its own `/tmp`; a stage-12 launchd agent's `ExitTimeOut`; a hung stop during the disk check), 3 low: all three fixed, the lows F180. Fresh reviewers: VM and host 7.5/10 (1 medium, the watch's stop admitting clients; 4 low: 3 fixed, 1 F180); guest and wire 8.5/10 (4 low, F180); security 7.5/10 (1 medium, the signing script's partition list over every signing key, fixed before the user ran it; 1 medium-low, the test password on argv, documented; 2 low, F180 and SECURITY.md). Mutants 6/6 on the fixes. |
| 13.8c (the readiness review's fixes: five reviewers, 7–8/10; plan Murphy 1 high, 1 medium, 3 low; clean-code 1 high, 1 medium, 10 low; Murphy 1 medium, 5 low) | F173a closed (binds from a share by descriptor), the UDP relays never wait on their stream, the first boot's disk kept at readiness, BuildKit's keys per platform, `save`/`cp` of any platform, the milestone's clock check | the plan's first UDP design would still have stalled mid-frame (high: reads made non-blocking too); the milestone's `after-wake` ended under `pipefail` before checking anything (high); a disk never kept when the ready ask failed as a run (medium: the user's data wiped at each start). Building found macOS's `MSG_DONTWAIT` blocking in `sendto`; mutants found two Mac UDP tests too gentle to fail the old relay (rewritten); the VM run found a test racing the disk's keeping. Mutants 10/11, one equivalent. |

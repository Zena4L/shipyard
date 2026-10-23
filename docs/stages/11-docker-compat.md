# Stage 11 notes: Docker compatibility

The engineering record of stage 11: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [The Docker API](../user/api.md),
[Compose](../user/compose.md), [Containers](../user/containers.md),
[Running containers](../user/run.md), [Volumes](../user/volumes.md),
[Networking](../user/networking.md) and [Shipyard on a Mac](../user/macos.md).

## Outcome

Stage 11 asked for six things ([ROADMAP](../ROADMAP.md), [FEATURES
§11](../FEATURES.md#11-docker-compatibility--stage-11)), decided on 2026-10-15 (D25–D27):

1. **The container record split from `Config` and from the API's types** (moved from
   stage 10, D10.1d-1). Built as step 11.1.
2. **A subset of Docker's Engine API,** started on first use and gone when idle, with
   Shipyard's own views under `/shipyard/…`. Built as step 11.2 (a: the service and the
   core calls; b: logs, events, attach, exec; c: images, volumes, networks, `cp`, the
   views).
3. **The CLI a drop-in for everyday `docker`.** Built as step 11.3 (a: short forms,
   groups, exit codes; b: flags; c: output; d: live verbs; e: prune and mounts).
4. **`stats`, `top`, `pause`/`unpause` and live `update` by name.** Step 11.3d.
5. **Compose is Docker's `docker compose` against the API,** with compose and
   Testcontainers as acceptance tests. Step 11.4 (a: compose's needs; b: Testcontainers
   and joining another container's namespaces).
6. **Docker's clients on the Mac use the API** (D11.5b-1 revised D24: the Mac's own
   `shipyard` keeps stage 10's forwarding until stage 13). Step 11.5.

Step 11.6 fixed the eight findings a Docker client meets (11.6a1–a3), ran the six
whole-stage reviews (11.6b), and proved the milestone (11.6c: `docker compose` and
Testcontainers on Lima's Ubuntu 24.04, root and rootless, through a reboot, and on the Mac). From the hardening track the
stage took on H1 (the API, the filters, `info`, `version` and prune in the engine; the
API and the CLI both thin), H2 (every new file and wire versioned from its first write:
`created.json` v1–v5, `labels.json`, `run-dir.json`, `network.json`, the attach protocol
v5), H3 (a threat model in every plan; the Murphy review run against 11.4b2's design
before building), H5/H8 (Docker 29.8's own answers captured and pinned, Docker's CLI 24
and compose 5.6 driving the service in the test image), H6 (the guest's refusals as codes,
F99) and H7 (the existing budgets kept; none added for a run with a `VOLUME`).

It closed F34, F72, F81, F88, F99, F101, F114, F116–F121, F128, F129, F131 and F133.
F29 and F132 were narrowed; F62, F71, F76, F94 and F107 moved to stage 15 (D11.6-1).
A user can now:

- `export DOCKER_HOST=unix:///run/shipyard/api.sock` (or the user's
  `$XDG_RUNTIME_DIR/shipyard/api.sock`) and use Docker's own `docker` CLI: `run -it`,
  `logs -f`, `exec -it`, `cp`, `pull`, `login`, `volume`, `network`, `prune`, `stats`;
- `shipyard compose up -d` a project with health checks, named and anonymous volumes,
  a project network, published ports and `network_mode`/`ipc: service:`; `down -v`;
- run Testcontainers' tests against Shipyard, unchanged;
- type Docker's commands to `shipyard`: `-v`, `-h`, `--env-file`, `-l`, `--mount`,
  `--tmpfs`, `logs -n`, `ps --format '{{.Names}}'`, `inspect -f`, `image ls`,
  `container prune`, `version`, `info`, `system df`, with Docker's exit codes;
- `pause`, `unpause`, `top`, `stats` and `update -m 64m --cpus 0.5` a container by name;
- join another container's network or IPC namespace (`--network container:A`,
  `--ipc container:A`);
- on a Mac, run `shipyard vm install` once, then `docker` and `docker compose` against
  the Mac's `docker.sock`, launchd starting the VM on the first connection.

| Requirement | Where | Tested by |
|---|---|---|
| The record's config in frozen types of its own; `Config` without serde (D10.1d-1) | `containers/config/stored.rs` (`StoredConfig`), `shipyard_sys::versioned::read` | `version_3_reads_as_written`, `every_nested_form_is_written_as_released`, `restart_words_are_the_records_own`, `unknown_stored_values_are_storages_error` |
| `inspect` prints Docker's shape; one meaning of "restarting" (F72) | `docker/container.rs` (`ContainerInspect`), `containers::Gathered`; `inspect --shipyard` for Shipyard's view | `a_record_is_inspected_as_docker_shows_a_container`, `states_are_docker_s`, `every_setting_is_shown_as_docker_s`, `what_s_read_besides_the_record_is_shown`, `lower_dirs_are_top_first`; a differential against Docker 29.8 by hand (11.1) |
| The service: socket activation or its own bind, the peer's UID, the idle exit, a stop that waits (D11.2-1, D11.2-2) | `shipyard-api` (`lib.rs`, `idle.rs`, `socket.rs`), `shipyard_sys::listen`, `contrib/systemd/shipyard.{socket,service}` (and `user/`) | `it_serves_the_socket_systemd_passes`, `the_service_ends_once_idle`, `a_stop_signal_ends_the_service`, `a_second_service_is_refused`, `another_user_is_refused`, `only_the_process_named_takes_the_sockets`, `a_stop_waits_for_the_calls_alone` |
| Routes, versions, Docker's statuses and errors (D11.2-3) | `routes.rs`, `errors.rs` | `the_version_prefix_is_checked_and_taken_off`, `each_kind_is_answered_with_docker_s_status`, `statuses_are_docker_s`, `refusals_are_docker_s`, `a_body_past_the_bound_is_refused` |
| Containers: list, inspect, create, start, stop, kill, restart, wait, remove, rename | `containers.rs`, `create.rs`, `create/body.rs`; the engine's `docker::{ContainerSummary, list_containers_where, standing}` | `docker_drives_a_container_s_life`, `a_wait_for_the_next_exit_waits_for_a_new_one`, `compose_runs_a_rootless_project` (a recreate's rename) |
| Logs and events as Docker streams them | `streams.rs`, the runtime's `logs::Gone`, `logs::LineWriter`, `docker::{EventMessage, parse_time}` | `logs_are_docker_s`, `logs_follow_until_the_container_ends`, `a_stop_ends_a_follow`, `events_are_docker_s` |
| Attach, attached `docker run`, resize (D11.2b-4) | `hijack.rs`, `attach.rs` (`Pending`, `HelloHeld`) | `run_attached_is_docker_s`, `start_attached_and_attach_are_docker_s`, `terminals_are_docker_s`, `detach_keys_detach`, `an_attach_without_upgrade_is_refused`, `an_upgrade_is_asked_as_docker_asks_it` |
| Exec instances, a child `shipyard exec` (D11.2b-2, D11.2b-3; F117) | `exec.rs` (`DETACHED_HOLD`, `sweep_pid_files`), `Paths::exec_pid_file` | `exec_is_docker_s`, `exec_instances_are_docker_s`, `exec_terminals_are_sized`, `an_exec_s_values_are_on_no_argv`, `a_detached_exec_lets_the_service_go`, `an_exec_s_detach_keys_end_its_session_only` |
| Images: pull with progress, credentials, `login`, list, inspect, tag, remove (D11.2c-2, D11.2c-3; F118) | `images.rs`; `image::{pull_with, tag, remove, login}`, `docker::{ImageSummary, ImageInspect}`, `Index::by_id`, `Credential::RegistryToken`, `registry::is_host` | `images_are_docker_s`, `a_pull_takes_the_client_s_credentials`, `image_refusals_are_docker_s`, `a_tag_s_time_is_its_last_tag_time`, `a_registry_token_is_sent_as_it_is`, `a_sign_in_checks_the_credential_at_the_api_root`, `an_index_digest_and_a_tag_time_are_shown` |
| Volumes: labels, anonymous names, prune by Docker's API version | `volumes.rs`; storage's `labels.json` v1, `Volumes::create_with`; `volumes::{create_with, in_use, prune_where}` | `volumes_are_docker_s`, `a_volume_prune_takes_what_it_s_asked_to`, `volumes_are_filtered_as_docker_filters_them` |
| Networks: IDs, labels, options honoured or refused, built-ins, `dangling` (D11.2c2-3, D11.2c2b-1; F120) | `networks.rs`; `docker::{NetworkResource, check_options, network_id, endpoint_id, is_predefined}`, `network::prune_where` | `networks_are_docker_s`, `containers_join_and_leave_networks`, `rootless_networks_are_the_built_ins`, `dangling_networks_are_docker_s`, `networks_are_filtered_as_docker_filters_them` |
| `docker cp` both ways, the stat header, gzip (D11.2c3-2; F121) | `archive.rs`; `cp::{stat, PathStat}`, the `cp-stat` helper, `cp/scope.rs` (`in_scope`) | `cp_is_docker_s`, `archives_are_docker_s`, `a_link_s_target_is_followed_in_the_container`, `links_are_followed_link_by_link_in_scope`, `the_walk_stays_in_the_root` |
| Shipyard's own views; the `/etc` paths while up (D11.2c3-3; F121) | `own.rs`; `run-dir.json` v1 | `shipyard_s_views_and_inspect_s_layers`, `inspect_names_the_etc_files_while_it_runs` |
| A `die` said once (F88) | `containers::{mark_end_unsaid, end_said}`, `run/take.rs`, `run/recover.rs` (`settle_unrecorded_end`) | `an_end_recorded_but_unsaid_is_said_once`, `an_end_unsaid_across_a_reboot_is_said_once`, `a_mark_of_another_start_says_nothing` |
| Short forms, the `container` and `image` groups, `tag`, Docker's exit codes (D11.3-4) | `cli.rs` (`Command::flattened`, `Command::failure_code`), `reporter::EXIT_FAILURE`, `system::SOME_NOT_SETTLED` | `docker_s_short_forms_parse`, `groups_flatten_to_their_commands`, `failures_exit_as_docker_s`, `exit_codes_are_docker_s`, `docker_s_names_and_short_forms_work`, `a_partial_recovery_exits_as_the_boot_unit_expects`, `the_boot_units_take_a_partial_recovery_as_success` |
| `run`/`create` flags: `--env-file`, labels, `--expose`, `--cidfile`, `-q`, `-a`; `--privileged`/`--init` refused (D11.3-2) | the CLI's `run/files.rs`, `created.json` v1 | `files_labels_and_the_id_file_are_docker_s`, `attach_quiet_and_the_refused_flags`, `a_quiet_create_says_no_pull`, `exposed_ports_are_shown_and_published_with_dash_p` |
| A default process limit of 2,048 where `pids` reaches (D11.3-3, D11.3b-2) | `created.json`'s `default_pids`, `DEFAULT_PIDS_MAX` | `a_container_gets_a_process_limit_unless_it_asks_none`, `docker_run_gets_the_default_process_limit` |
| `ps` and `inspect` show what a start runs (F101, D11.3b-1) | `image::runtime_config`, `ImageConfigs`, `RunRequest::entrypoint_and_cmd` | `the_process_shown_is_the_one_a_start_runs`, `ps_and_inspect_show_the_image_s_own_command` |
| Go templates, Docker's columns, `-q`, `-f`, `--no-trunc` (D11.3c-1, D11.3c-3) | the CLI's `format/` (`gotmpl` 0.6.1, `tabwriter`), the engine's `docker/filters.rs` | `a_table_is_docker_s_byte_for_byte`, `a_value_can_t_forge_a_row`, `what_a_terminal_acts_on_is_escaped`, `lists_are_docker_s`, `list_flags_are_docker_s`, `images_and_volumes_list_as_docker_s` |
| `inspect` of any object; `version`, `info`, `system df` (D11.3c2-1, D11.3c2-2) | `docker/inspect.rs` (`inspect_any`, `passed_on`), `docker/system.rs`, the CLI's `about.rs` | `a_lookup_passes_on_only_what_isn_t_its`, `inspect_finds_any_object`, `version_and_info_say_docker_s`, `system_df_counts_what_is_in_use`, `objects_are_shown_as_docker_s_inspect` |
| `pause`, `unpause`, live `update` with limits | `run/pause.rs`, `run/update.rs` (`refuse_unsupported_limits`, `placeable`) | `pause_unpause_and_update_are_docker_s`, `a_paused_container_stops_and_takes_signals`, `a_refused_limit_leaves_the_container_as_it_was`, `docker_pauses_and_updates`, `a_paused_container_is_up_and_paused` |
| `top` and `stats` (D11.3d-2) | `docker/top.rs`, `docker/stats.rs`, `docker/proc.rs`, the CLI's `stats.rs` (`StatsRow`) | `top_is_docker_s`, `stats_is_docker_s`, `docker_tops_and_stats`, `docker_stats_once`, `a_stat_line_is_read_past_its_name`, `the_cpu_share_is_docker_s`, `memory_is_docker_s` |
| `container`, `image` and `system prune` (D11.3e-3) | the engine's `prune.rs`, the CLI's `prune.rs`, the API's prune routes | `container_prune_is_docker_s`, `image_and_system_prune_are_docker_s`, `docker_prunes`, `one_its_policy_restarts_is_never_removed_unasked`, `an_image_a_record_names_is_kept_without_its_pin` |
| `--mount` and `--tmpfs` (D11.3e-2, D11.3e2-2) | `run/mounts.rs` (`TmpfsMount`, `MountRequest`), `created.json` v2 | `tmpfs_and_mounts_are_docker_s`, `a_tmpfs_is_fresh_at_each_start`, `a_tmpfs_never_hides_a_mount_inside_it`, `docker_runs_with_tmpfs_and_mounts`, `an_unreadable_created_file_refuses_the_start` |
| Anonymous volumes and an image's `VOLUME`s (F128, D11.4a2-1) | `run/volumes.rs` (`name_anonymous`, `covers`), `volumes::create_anonymous`, `created.json` v3 | `anonymous_volumes_are_made_and_kept`, `anonymous_volumes_go_with_rm_v_and_dash_dash_rm`, `an_image_s_volumes_are_anonymous_ones`, `docker_s_anonymous_volumes_come_and_go`, `an_undone_create_removes_its_volumes` |
| Copy-up at the create; bind sources made (F129, D11.4a2b-1, D11.4a2b-4) | `cp/fill.rs`, `run::volumes::fill_volumes`, storage's `lock_fill` and `Filling` | `a_volume_is_filled_from_its_image`, `an_emptied_volume_is_filled_by_a_create_never_a_start`, `a_fill_that_fails_fails_the_create`, `a_killed_create_s_fill_is_finished_by_its_first_start`, `one_fill_at_a_time_and_its_marker_names_its_owner`, `a_bind_s_source_is_made_again_at_each_start` |
| `shipyard compose` and compose's needs (D27, D11.4-2, D11.4a-2) | the CLI's `compose.rs`, `api_service::ensure`, the events `label` filter, `ensure_no_container_up_on` | `compose_runs_a_project`, `compose_runs_a_rootless_project`, `compose_runs_against_the_socket`, `a_stopped_containers_network_goes_as_docker_s` |
| Testcontainers as acceptance (D11.4-3, D11.4b-4) | `crates/shipyard-acceptance`; `Health::starting` | `testcontainers_runs_against_shipyard`, `a_health_check_starts_as_starting` |
| Another container's network and IPC namespace (F34, D11.4b2-*, D11.4b2b-*) | the runtime's `process/joins.rs` (`NamespaceJoins`), `process/enterer.rs`, `process/rootfs.rs` (`clone_their_shm`); the engine's `run/joined.rs`; `created.json` v4 | `a_new_container_joins_another_s_namespaces`, `a_container_joins_another_s_network`, `the_joined_one_stops`, `joins_of_nothing_are_refused`, `a_container_joins_another_s_ipc`, `ipc_joins_refused`, `ipc_join_ends`, `lets_go_of_their_mounts` |
| The VM's network card keeps its address (11.5a) | `shipyard-vm/src/network.rs` (`network.json` v1) | `the_address_is_made_once_and_kept`, `made_addresses_are_local_unicast`, `a_wrong_file_is_refused`, `an_empty_file_makes_a_new_address` |
| Docker's API on the Mac (D11.5b-1) | `shipyard-vm/src/api.rs`, `serving.rs`; the guest agent's `forward_api` | `dockers_cli_uses_shipyard_on_the_mac`, `an_api_connection_keeps_the_vm_up`, `docker_compose_runs_on_the_mac`, `a_refusal_is_dockers_error` |
| launchd holds `docker.sock`; `vm install` (D11.5b2-1) | `shipyard-vm/src/launchd.rs` | `launchd_holds_dockers_socket`, `a_host_launchd_cant_boot_is_said`, `shipyard_compose_runs_on_the_mac`, `each_vm_directory_has_its_own_label`, `guarded_folders_are_named` |
| Refusals by code (F99); a start whose process ended said plainly (F119) | the runtime's `attach` v5 (`Frame::Declined`, `forward::Refusal`); `run/start.rs` (`ended_before_start`) | `a_refusal_goes_by_its_code_and_falls_back_to_words`, `a_container_ended_before_its_start_is_said` |
| The lifecycle findings (F81, F133, F114, F116) | `run/cleanup.rs` (`say_end`), `containers::health`, `drop_failed_start`, `raise_own_descriptor_limit`, `created.json` v5 (`stdin_once`) | `a_failed_start_sets_off_no_restart`, `the_companion_goes_with_each_start`, `health_is_in_the_status_while_it_runs_only`, `the_service_raises_its_own_descriptor_limit_only`, `stdin_once_outlives_the_service_that_made_it`, `stdin_once_is_kept_without_a_terminal`, `a_foreground_run_i_closes_its_input_once` |

**Moved out of stage 11:**

| Item | Now in | Why |
|---|---|---|
| The Mac's `shipyard` over the API instead of argv forwarding | stage 13 | D11.5b-1: Docker's API lacks the Mac CLI's terminals, signals and Shipyard's own flags |
| F62's remainder (`--isolate-host` for `bridge`) | stage 15 | D11.6-1: hardening of the default network, not Docker compatibility (the default process limit was done in 11.3b1) |
| F71 (one resize watch per process) | stage 15 | D11.6-1: the CLI never runs two sessions; the UI and hardening will |
| F76 (seams in `shipyard-sys`'s errors and names) | stage 15 | D11.6-1: internal, no user-facing change |
| F94 (signals behind input on the VM wire) | stage 15 | D11.6-1: a second channel on the VM wire, with stage 13's work or 15 |
| F107 (helpers that grew a copy per step) | stage 15 | D11.6-1: the versioned read is one since 11.1 (`read_file` since 11.6a2); the rest is internal |
| F29 (the runtime library in a long-lived program) | stage 15 | narrowed in 11.2c3b: the subreaper is put back on drop; reaping only the command's tree is left |
| F132 (the guest's kernel crashing with four VMs at once) | stage 13 | D11.5a-1: narrowed, mitigated (two VM tests at a time), cause not found |
| `--pid container:` (F134) | stage 15 | D11.4b2-5: the design review's findings (B's setup visible to A, B's orphans) need their own fixes |
| `--privileged`, `--init` | refused | D11.3-2: privileged would undo the threat model; use `--cap-add`, `--device` |
| A `Build Cache` row in `system df`; `POST /build/prune` with no cache | stage 12 | there's no build until BuildKit (D28) |

## Architecture

### The crates

```
shipyard-cli ──▶ shipyard-engine ──▶ shipyard-runtime, -network, -image, -storage
     │      ▲            ▲                         │
     │      └── shipyard-api (hyper 1, tokio)      ▼
     └──▶ shipyard-vm ──▶ shipyard-runtime ──▶ shipyard-sys (leaf)
shipyard-acceptance (dev only) ──▶ testcontainers =0.28.0, the `shipyard` binary
```

- **`shipyard-api`** (new, 11.2a): Docker's Engine API at 1.43, served by `shipyard
  system service`. Every engine call runs on a blocking thread with an engine of its
  own (`Shared::call`, or `Shared::call_reporting` for a pull's progress). `deny.toml`
  lets it and the CLI use the engine, nothing else.
- **`shipyard-engine`** gained `docker/` (Docker's shapes and the filters, H1),
  `prune.rs`, `api_service` (start the service when nothing answers), `run/pause.rs`,
  `run/mounts.rs`, `run/joined.rs`, `run/volumes.rs` and `cp/fill.rs`.
- **`shipyard-cli`** gained `format/` (Go templates through `gotmpl`, Docker's rows and
  tables), `about.rs` (`version`, `info`), `stats.rs`, `prune.rs` and `compose.rs`.
- **`shipyard-vm`** gained `api.rs` (`docker.sock`), `serving.rs` (one accept loop for
  both sockets), `launchd.rs` and `network.rs` (the card's address).
- **`shipyard-sys`** gained `listen` (systemd's socket), `versioned::read_file`,
  `relay::{Lingers, both_ways_lingering}` and `random::bytes`.
- **`shipyard-acceptance`** (new, 11.4b1): Testcontainers for Rust against the service,
  dev-only, its dependencies Linux-only.

### The API service

```
docker, docker compose, Testcontainers        (DOCKER_HOST=unix://…/api.sock)
   │  root: /run/shipyard/api.sock (0600)   rootless: $XDG_RUNTIME_DIR/shipyard/api.sock
   ▼
systemd's shipyard.socket ── or ── `shipyard compose` (api_service::ensure: started if
   │                                nothing answers, a process group of its own)
   ▼
shipyard system service    one current-thread tokio runtime; SO_PEERCRED on every
   │                       connection; stop signals blocked on every thread, taken by
   │                       one sigwait thread; idle after 60 s (SHIPYARD_API_IDLE)
   ├─ engine calls on blocking threads; at most 128 streams (logs, events, stats, cp)
   ├─ attach: the client taken back as its UNIX stream (101), the monitor's frames
   │    over attach_stream with HelloHeld; a container not running waits in Pending,
   │    welcomed by the API's start before the gate opens (at most 16 at once)
   ├─ exec: a child `shipyard exec --env=KEY … -- C CMD` of /proc/self/exe; values in
   │    SHIPYARD_CALLER_ENV, never argv; a raw pty or pipes, pumped on threads;
   │    its host PID in <runtime root>/.exec-<id>.pid
   └─ cp: the engine's helpers (cp-out, cp-in, cp-stat) on one blocking thread
containers it makes: run::create, run::start; their monitors leave the service at once
```

On Linux the `shipyard` CLI never uses the API (D11-2, D20): it calls the engine itself.

### The Mac's Docker socket

```
docker (Mac) ──▶ ~/Library/Application Support/Shipyard/vm/docker.sock
   │              launchd's after `vm install` (launch_activate_socket), else the host's
   ▼
__shipyard-vm   serving.rs: 0600, the peer's uid, counted for idle, at most 1024 open
   │            each connection: vsock 1024, forward::API_REQUEST, then raw bytes
   │            (relay::both_ways_lingering: unlimited after the client's end, 60 s
   │            after the service's); a guest not ready yet waited for up to 150 s
   ▼
guest agent ──▶ api_service::ensure ──▶ shipyard system service on /run/shipyard/api.sock
```

launchd runs the host once at load to make the socket; the host checks in first and
exits unless a connection waits or the CLI left a fresh `start-asked` mark. A host the
CLI started leaves an installed agent's `docker.sock` alone (`launchd::installed`).

### Files and formats

| File or wire | Version | Written by | Notes |
|---|---|---|---|
| `record.json` | 3 (unchanged) | the engine | its config now `StoredConfig`, frozen; `Config` has no serde (11.1) |
| `created.json` | 5 (1–4 read) | a create, before the record is placed | v1 `exposed`, `default_pids`; v2 `tmpfs`; v3 `anonymous`, `make_sources`; v4 `network_from`, `ipc_from`; v5 `stdin_once`. Written at the oldest version that holds it; unreadable, the start is refused (D11.3e2-2) |
| `end-unsaid` | — | a start, before it clears the exit | the start's time; removed by `__cleanup` after its `die` (F88) |
| `run-dir.json` | 1 | each start | the run directory's place, for inspect's `/etc` paths; removed on the host's network (F121) |
| `volumes/<name>/labels.json` | 1 | `create_with`, before `_data` | labels, 64 KiB at most; a missing file is no labels |
| `volumes/.fill-locks/<name>`, `<name>/filling` | — | a create's fill | one fill at a time; the marker names its container (F129) |
| `networks/<name>/labels.json` | 1 | a create, before `config.json` | the network's ID, labels and options, 0600 |
| The image index's entries | unchanged map | the image store | optional `index_digest` and `tagged_at`; an older Shipyard ignores them (F118) |
| The attach protocol | 5 (4 read) | the monitor, the guest agent | `Frame::Declined` (kind 25) with a code: stopping, not ready, other (F99); a newer client answered at the guest's own version |
| `forward::API_REQUEST` | — | the Mac host | a connection asked to the guest's API, then raw bytes |
| `network.json` (the Mac) | 1 | the VM host, once | the card's locally administered MAC; a wrong or newer file refused, an empty one made again |
| `.exec-<id>.pid` | — | the exec child | the command's host PID; swept at the service's start when older than `KEPT` |
| `api.sock`, `api.sock.lock` | — | the service | a socket never taken from another; removed at the end only if still its own |

### Locks

Stage 10's order holds (AGENTS §4). Stage 11 added two:

- **A volume's fill lock** (`.fill-locks/<name>`) is innermost: taken while the volume is
  held shared, nothing taken under it. Its wait gives up when `HeldOff` asks (Ctrl-C).
- **The `restarting` lock** stays outside the order. `__cleanup` takes it before it lets
  `running` go and hands it to the policy, which holds it through the backoff; `rm`
  without force refuses while anyone holds it (11.3e1).

A create now holds the new container's `op.lock` from its record to its end, its undoing
included (11.4a2b).

## Decisions and why

**The stage (2026-10-18, asked):**

- **D11-1:** hyper 1 and tokio (current-thread) in a new crate; the engine stays
  blocking; attach and exec take the connection over (HTTP's upgrade).
- **D11-2:** on Linux the CLI keeps calling the engine (daemonless); the API is for
  Docker's clients.
- **D11-3:** `--format` through a maintained crate, its gaps documented. Revised by
  D11.3c-1 (`gotmpl`, not `gtmpl`).
- **D11-4:** six steps, each split again when planned.

**11.2, the API:**

- **D11.2-1, the socket:** Shipyard's own path, never Docker's, so both can live on one
  host. Root's is 0600 and root's alone: a group on it would be root for its members.
- **D11.2-2, idle:** 60 s with no connection and no request; systemd starts it again.
- **D11.2-3, the version:** Engine API 1.43 (Docker 24's); `/v1.24`–`/v1.43` taken, a
  newer one refused so clients negotiate down.
- **D11.2-4, acceptance:** Docker's CLI 24.0.9, pinned by sha256, in the test image; a
  raw HTTP client in test-support for exact answers.
- **D11.2b-2, D11.2b-3:** an exec is a child `shipyard exec`, its instances in the
  service's memory (lost at its exit, as Docker's at a restart). The engine's exec
  installs a process-wide supervisor, so it can't run inside the service.
- **D11.2b-4:** an attach to a created container waits, and the start connects it before
  the gate opens: no output is lost.
- **D11.2c-2, the image ID is the manifest's digest (revised):** a real Docker 29.8 showed
  its containerd store's `Id` is the digest pulled, not the config's. Equal for an image of
  one platform; no change to the record.
- **D11.2c-3:** `X-Registry-Auth` is honoured, handed to the pull only, never logged.
- **D11.2c2-2:** labels in a `labels.json` of their own, versioned, beside the existing
  files: the DNS servers' unversioned `config.json` stays as older readers expect.
- **D11.2c2-3:** the built-in networks are listed (rootless: `host`, `none`, `pasta`);
  changes to them refused 403.
- **D11.2c2b-1:** a network option is honoured or refused, never ignored: Docker's bridge
  defaults pass, Shipyard's own keys (`com.shipyard.network.isolate-host`,
  `…conntrack-limit`) turn its features on.
- **D11.2c3-2:** a PUT archive is plain or gzip; bzip2 and xz refused by their magic.
- **D11.2c3-3:** only the cheap inspect keys in 11.2; the `/etc` paths later (F121, done
  in 11.6a2).

**11.3, the CLI:**

- **D11.3-2:** `--privileged` and `--init` refused, saying what to use instead.
- **D11.3-3, D11.3b-2:** a default limit of 2,048 processes (Podman's), applied at each
  start where the `pids` controller reaches, never stored as the container's limit. A
  start from where `pids` isn't delegated never fails for it.
- **D11.3-4, exit codes:** Docker's, checked on Docker 29.8: 125 for `run`'s own failures
  and usage errors, 1 for every other command's. `system recover` and `system stop` keep
  125 for "some, not all", which the boot units count as success.
- **D11.3b-1:** `ps` and `inspect` read the image's config by the container's digest and
  merge it by the start's own rules: no format change.
- **D11.3c-1:** `gotmpl` 0.6.1, pinned exactly: MIT, no `unsafe`, bounded execution,
  sorted maps. gtmpl spawns a thread per parse and iterates maps at random.
  `{{.Label "x"}}` works by rewriting the parsed tree.
- **D11.3c-3:** `--format json` is Docker's row shape; Shipyard's snake_case list JSON goes.
- **D11.3c2-1:** on the Mac, `version` and `info` are forwarded (both sides the guest's).
- **D11.3c2-2:** `inspect -f` fails on any missing key, as Docker's `missingkey=error`.
- **D11.3d-2:** `top` reads `/proc` itself in `ps -ef`'s columns; `ps` options refused.
- **D11.3e2-1, D11.3e2-2:** an anonymous `--mount` volume first refused (lifted by F128 in
  11.4a2a); a `created.json` that can't be read refuses the start, since it may hold tmpfs
  mounts meant for memory (this reversed 11.3b1's warning).
- **D11.3e-3:** `image prune` is Docker's, and now asks unless `-f`.

**11.4, compose and Testcontainers:**

- **D11.4-2:** `shipyard compose` starts the service itself when nothing answers, detached;
  it exits once idle (D20).
- **D11.4a-2:** Docker's rule for a network's removal: refused only while a container
  that's up is on it. This reverses stage 9's 9.9a M2; a stopped container's start then
  fails, "there's no network named …".
- **D11.4a2-1, D11.4a2-3:** an image's `VOLUME`s get anonymous volumes; an empty volume is
  filled from the image's files (copy-up), unless no-copy.
- **D11.4a2b-1 (revised):** fill at the create only, as moby's `populateVolumes`; the first
  plan said "each start", which the Murphy review and moby's source corrected.
- **D11.4a2b-4 (revised):** a bind's missing source (the API's `CreateMountpoint`, compose's
  short syntax) is made at the create and at each start, as Docker's `MountPoint.Setup`;
  the CLI's `-v` still refuses one (D11.4a2b-5).
- **D11.4b-3, D11.4b2-2:** a rootless join also joins the target's user namespace (its
  maps); the join is kept in `created.json` by the other's ID.
- **D11.4b2-4:** a container others joined can be stopped and removed, said with a warning
  naming them.
- **D11.4b2-5:** network and IPC now, PID later (F134), after the design's Murphy review
  scored it 5/10.
- **D11.4b2b-2, D11.4b2b-3:** B's `/etc` files built at each start from A's data; B's
  `/dev/shm` is A's, cloned by a helper inside A's mount namespace.

**11.5, the Mac:**

- **D11.5a-1:** F132 narrowed, mitigated (VM tests two at a time), moved to stage 13.
- **D11.5b-1:** the API socket now; the Mac CLI keeps forwarding (D24 revised).
- **D11.5b2-1, D11.5b2-2:** `vm install`/`uninstall` with launchd holding the socket; the
  VM tests may bootstrap temporary launchd jobs.
- **D11.5b2b-1:** F119 said plainly and its cause recorded; no hunt.

**11.6, the close:**

- **D11.6-1:** the eight findings a Docker client meets fixed now (F81, F114, F116, F117,
  F118, F120, F121, F133); F62, F71, F76, F94, F107 to stage 15.
- **D11.6-2:** a = the findings, b = the reviews and these notes, c = the proof.

## Kernel checks

**`scripts/probes/foreign-shm.c`** (11.4b2b2), before `--ipc container:` was built:

| Question | Result |
|---|---|
| Can `mount(MS_BIND)` or `open_tree(OPEN_TREE_CLONE)` take `/proc/PID/root/dev/shm` from a mount namespace the caller isn't in? | no: `EINVAL` (the kernel's `check_mnt`), on 7.2.9 and Ubuntu 6.8 |
| Can a helper that enters that namespace clone it, and attach the detached mount elsewhere? | yes: root on both kernels, rootless on 7.2.9 (Ubuntu's AppArmor refused the unconfined probe a user namespace) |

**Found by tests, each now in AGENTS §10:**

- **`do_exit` empties `/proc/PID/cmdline` (`exit_mm`) before it closes the files
  (`exit_files`):** a scan reading an empty command line as "gone" saw the DNS server gone
  with its lock still held (11.3a). Wait on the lock.
- **A cgroup whose processes were moved out can still refuse `rmdir` (`EBUSY`) until
  `cgroup.events` says `populated 0`** (11.6a1's `exec_refuses_a_replaced_cgroup`).
- **`dup2(fd, fd)` keeps close-on-exec:** the service's socket-activation test lost its
  socket whenever the listener was already 3 (11.3b1).
- **The umask is the process's:** the API's bind narrowed it beside another test making a
  directory (11.2b2a).
- **A process's soft descriptor limit bounds numbers:** the service raises its own to the
  hard limit; each monitor gets the caller's back, never below its highest handed
  descriptor + `ROOM_ABOVE_HANDED` (F114).

**The OCI conformance suite:** `linux_ns_path` passes 3 of 5 subtests (ipc, net); uts
fails on the suite's hostname (refused in a joined UTS namespace); mnt and pid stay
refused (`scripts/conformance/expected.txt`).

**The macOS guest's kernel (F132, 11.5a):** with `guest-crash-rate.sh`, on 6.18.58 four
VMs at once crashed 6 boots in 227 and one at a time 0 in 157 (Fisher one-sided p ≈ 0.06);
6.12.114 crashed too, so the version isn't the cause. Before that, about 400 boots used up
`bootpd`'s 253 leases: Virtualization.framework gave the card a new MAC each boot.

**launchd on macOS 27** (11.5b2a, by experiment): a socket job is run once at load; the
socket exists only once the job checks in (`launch_activate_socket`); a connection starts
the job, throttled to once in 10 s. A launchd-started program reads the folders macOS
guards only with the user's permission (F140).

**Docker 29.8 and its CLI 29.8.2** (on this Mac, OrbStack's daemon) were the reference for
every shape pinned: inspect, `ps`, events, images, volumes, networks, `version`, `info`,
`system df`, exit codes, `stats` after a stop, `unhealthy` after an end.

## macOS and Linux differences

- **The API on a Mac** is the guest's service, reached through `docker.sock` in the VM's
  directory; on Linux it's systemd's socket or the service's own. A Mac bind source outside
  the home reaches the VM's own path through `docker.sock` (F143); the CLI's home rule
  still holds for `shipyard`.
- **Socket activation:** systemd passes descriptor 3 (`LISTEN_PID`, one socket); launchd
  hands its socket through `launch_activate_socket`, the one FFI call, and runs the job once
  at load.
- **The peer's identity:** `SO_PEERCRED` on Linux, `getpeereid` on the Mac.
- **A UNIX socket closed unread** doesn't reset its peer on macOS: the host's refusal
  drain did nothing there and was removed (11.5b1).
- **`shipyard compose`** finds `docker` on `PATH` on both; on a Mac it runs against
  `docker.sock`, started as `vm start` starts it.
- **`version` and `info` on a Mac** are forwarded: Client and Server are the guest's.
- **Linux only:** the service's exec spawn, `top` and `stats` (`/proc`), the joins, the
  fill helper, the guest agent's API forwarding; the acceptance crate's dependencies.
- **A background job's SIGINT** is ignored on both and stays ignored across `exec`: the
  signal tests reset it (`Shipyard::command`, the VM tests' `with_sigint`); this was F131.

## Failure-mode register

| Case | Handling | Test |
|---|---|---|
| A SIGTERM to the service mid-create | stop signals blocked on every thread, taken by one `sigwait` thread; the stop waits for the engine calls under way, and none starts once stopping (503) | `a_stop_signal_ends_the_service`, `a_stop_waits_for_the_calls_alone` |
| Another local user connecting to root's or a user's socket | the socket 0600 in a private directory, and `SO_PEERCRED` checked | `another_user_is_refused` |
| A second service, or a socket path something else holds | refused; a served socket never taken; removed at the end only if still its own | `a_second_service_is_refused` |
| `docker run` waiting for `wait`'s headers before its start (11.2a's Linux run: it hung) | `wait` answers 200 and its headers at once, the body at the end | `a_wait_for_the_next_exit_waits_for_a_new_one`, `docker_drives_a_container_s_life` |
| A client that stalls or goes during a follow; a follow at the service's stop | a bounded channel; `send` waits in 50 ms slices asking `gone`; at most 128 streams, then 503 | `a_stop_ends_a_follow`, `logs_follow_until_the_container_ends` |
| Docker's CLI half-closing at once with no input (11.2b2a: all output lost) | the monitor's write half held for the session | `run_attached_is_docker_s` |
| An attach to a created container racing its start | `Pending`, welcomed before the gate opens; a client gone drops its sender; at most 16 at once, the rest join late | `start_attached_and_attach_are_docker_s`, `stdin_once_outlives_the_service_that_made_it` |
| An exec's process left in the background holding its pipes or pty | the pumps drain for 2 s after the end | `exec_is_docker_s` (`sleep 60 &` doesn't hold `exit 7`) |
| An exec's variable in `ps` | the values in `SHIPYARD_CALLER_ENV`, only the keys on argv, `--` before the container | `an_exec_s_values_are_on_no_argv` |
| An exec detached by its keys at the service's idle exit (11.6a3, medium-high) | it holds the service up until it ends; a `-d` exec lets go after `DETACHED_HOLD` | `a_detached_exec_lets_the_service_go`, `an_exec_s_detach_keys_end_its_session_only` |
| A thread spawn failing in the service, with `panic = "abort"` (11.6b) | `Builder::spawn`; a refused watcher ends the exec's group | review fix; no test (a spawn can't be made to fail) |
| A registry's text, or a password, leaking | credentials never `Debug`, never in an error, log or file; registry words stripped of control characters | `a_pull_takes_the_client_s_credentials` (a walk of the service's files) |
| `serveraddress` of `localhost:80@evil.example` read as loopback (11.6a2) | `registry::is_host` guards `is_loopback`; `login` refuses such an address first | `a_sign_in_checks_the_credential_at_the_api_root` and its units |
| One unreadable image, volume label or network `labels.json` in a list | left out, or listed without it, said in `Warnings`; never the list refused | `images_are_docker_s`, `volumes_are_docker_s`, `networks_are_docker_s` |
| An unreadable container record during `dangling` or `network rm` (11.6a2) | `containers::list_every` is strict: refused, never read as "not in use" | `dangling_networks_are_docker_s` |
| `docker network create bridge` taking over the default network | the name `bridge` refused as Exists (409); `default` refused 403 | `networks_are_docker_s` |
| A GET archive whose copy fails after its headers (11.2c3a: `docker cp` "succeeded") | the stream cut with an error frame | `archives_are_docker_s` |
| A stalled PUT or GET holding `op.lock` | 5 minutes without progress, then given up; a body error is sticky, never an EOF | `archives_are_docker_s`; the body reader's units |
| A symlink swapped under a `cp` path; a link chain | resolved in the container's root by the helper (`open_in_root`, `in_scope`, 255 links) | `cp_is_docker_s`, `a_link_s_target_is_followed_in_the_container`, `a_loop_ends` |
| `__cleanup` killed between the end's record and its `die`; a reboot in that window (F88) | `end-unsaid` names its start; `reconcile` or the recovery pass says it; a mark of another start says nothing | `an_end_recorded_but_unsaid_is_said_once`, `an_end_unsaid_across_a_reboot_is_said_once`, `a_mark_of_another_start_says_nothing` |
| A crash-looping `restart: always` container pruned in `__cleanup`'s gap (11.3e1, high) | the `restarting` lock taken before `running` is let go; `rm` refuses while it's held | `one_its_policy_restarts_is_never_removed_unasked`, `a_killed_restart_waiter_is_settled` |
| `image prune -a` with a create killed before its pin, or an old pinless record | a manifest any record names is kept | `an_image_a_record_names_is_kept_without_its_pin` |
| `docker update --memory` without `--restart` (11.3d1, high: the policy reset) | a policy only when named | `docker_pauses_and_updates` |
| A limit the kernel refuses half way through an update | the record first, then the cgroup; a refusal puts the record back and writes the kept limits again | `a_refused_limit_leaves_the_container_as_it_was` |
| Docker's CLI sending `PidsLimit: 0` without the flag (11.3b1, high) | 0 is unset: the default applies | `docker_run_gets_the_default_process_limit` |
| A tmpfs over a volume's parent (`--tmpfs /data -v db:/data/db`) | the root's mounts sorted by depth, stable | `a_tmpfs_never_hides_a_mount_inside_it` |
| A damaged `created.json` | the start refused (it may hold tmpfs mounts); `inspect` warns; `rm` is the way out | `an_unreadable_created_file_refuses_the_start` |
| `rm -v` of an anonymous volume given by name | only the list the create made (`created.json` v3) is removed, read under the removal's lock | `anonymous_volumes_go_with_rm_v_and_dash_dash_rm` |
| A prune between an anonymous volume's making and its record | held shared until the record names it | `an_undone_create_removes_its_volumes` |
| A create killed mid-fill; another container using that volume since (11.6b) | the marker names its container; its first start finishes the fill if nothing used the volume, else it's given up and said | `a_killed_create_s_fill_is_finished_by_its_first_start` |
| Two creates filling one empty volume | `lock_fill` (10 minutes, then Busy; Ctrl-C gives up); the helper checks emptiness again | `one_fill_at_a_time_and_its_marker_names_its_owner` |
| A hostile image's files copied into a volume | cp's unpacker beneath `_data` by descriptor, its limits, the image store's free-space reserve; a failure fails the create | `a_fill_that_fails_fails_the_create` |
| A reused PID named as the namespace to join | the start time checked through the held `/proc/PID` descriptor; nothing looked up again | `a_new_container_joins_another_s_namespaces` |
| B's settings landing in A's namespaces | hostname, IPC and `net.*` sysctls and devices refused with a join (`JoinedSetting`) | `a_new_container_joins_another_s_namespaces`, `joins_of_nothing_are_refused` |
| B's init pinning A's mount tree after A's end (11.4b2b2, high) | A's mount-namespace descriptor closed after the setup | `lets_go_of_their_mounts` |
| A joins a network that's restarted by its policy | B stays on the old one, said only for a user's own stop (F136, Docker's too) | `the_joined_one_stops` |
| `docker.sock` path too long under a long home (11.5b1: a total outage) | the longest socket path checked | review fix; `check_socket` |
| A pooled client keeping the VM up for ever | a linger per side: unlimited after the client's end, 60 s after the service's | review fix |
| A cold `docker` start meeting the guest's `NOT_READY` as a 503 | `api::joined_once_ready` waits up to 150 s | `dockers_cli_uses_shipyard_on_the_mac` |
| A stale `start-asked` mark booting the VM at login | written only while no host runs, younger than 150 s, taken by the host | `launchd_holds_dockers_socket` |
| A CLI-started host unlinking an installed agent's `docker.sock` (11.6b) | left while the agent is installed (`launchd::installed`) | review fix |
| An older guest's welcome, or an older CLI meeting a newer guest | the protocol negotiated; refusals by code from v5, by words before (`Refusal::from_words` only) | `a_refusal_goes_by_its_code_and_falls_back_to_words` |
| A container whose process ended between the runtime's create and start (F119) | "its process ended before it could start (…): its command never ran", kind NotRunning | `a_container_ended_before_its_start_is_said` |
| A failed user start running the restart policy (F81) | `__cleanup` runs it only when this start ran (the runtime's word) | `a_failed_start_sets_off_no_restart` |
| The service's descriptor limit under many sessions (F114) | raised to the hard limit; monitors and exec commands get the caller's back | `the_service_raises_its_own_descriptor_limit_only` |
| The volume store as open as the directory above it (11.6b) | `volumes/` made 0700, an older one narrowed | its unit test in storage |
| A Docker client killed in a quiet `exec` on the Mac | the host's relay sees its client gone (a zero-length `send` fails `EPIPE`), cuts, and sends `__guest-api-hang-up PORT`; the guest shuts that API socket both ways, the service sees `POLLHUP` and hangs the exec up (11.7) | `a_killed_quiet_exec_is_hung_up`, `a_client_gone_ends_the_relay_and_a_half_close_doesnt` |
| A slow or stalled API client | its stream waits on its channel's progress (bounded, `gone` asked each 50 ms), never a fixed sleep (11.7) | `a_writer_goes_on_as_its_client_reads`, `a_body_is_read_as_each_frame_comes`, `bulk_streams_keep_their_budget` |
| An `update` refused part way | each cgroup file read before its write and put back in reverse (11.7) | `a_refused_update_puts_the_cgroup_back`, `put_back_values_are_what_was_there` |
| A symlink put where Shipyard made a bind source | made and bound by descriptor from its directory; the start refused (11.7) | `a_made_bind_source_is_never_reached_through_a_symlink`, `a_made_bind_source_is_bound_by_descriptor` |
| A fill another container's first start would finish over a volume another mounts | given up, said (11.7) | `a_finish_fill_leaves_a_volume_another_uses` |

## Fuzzing

- **`api_request`** (new, 11.2c3b): over `shipyard_api::fuzzing` (`#[cfg(fuzzing)]`): the
  routes, the query, every filter, the container, network, volume and exec create bodies,
  the exec child's argv, `X-Registry-Auth`; the prune filters joined it in 11.3e1. Seeds in
  `scripts/fuzz-seeds.py`. Two runs of 300 s: 4.31 M and 3.47 M runs, no crash.
- **`attach_frames`** covers protocol v5's `Declined` through its round trip: 300 s,
  11.8 M runs, no crash (11.5b2b).
- **The new formats** (`created.json`, both `labels.json`, `run-dir.json`, `network.json`,
  the index's new fields) are serde JSON read with a bound, each with a version test. Their
  writers are the user's own processes.
- **The close's run** (11.6c): all 15 targets, 300 s each, clean (millions of runs each).
  Its first run built nothing: the `cfg(fuzzing)` call had an old signature. Fixed, and
  `verify.sh --mac` now checks that code (`--cfg fuzzing`).

## Known limitations (by design, for stage 11)

- **The subset is what Docker's clients were checked with** (CLI 24, compose 5.6,
  Testcontainers 0.28): an image's history, plugins and swarm answer 404. A create never
  pulls (Docker's CLI pulls and creates again).
- **Refused, saying what to use instead:** `--privileged`, `--init`, `--pid` (F134),
  `--ipc host|none`, `--uts`, `--userns`, `--shm-size`, `--gpus`, other log or volume
  drivers, a volume's driver options, network options Shipyard can't honour, `top`'s `ps`
  options (F127), lifting a process or CPU limit by `update` (F125).
- **Approximations, documented:** an image's `Size` is compressed and `Comment` empty; ports
  aren't grouped into ranges in `ps`; `inspect` keeps one form of each mount (a tmpfs in both
  `HostConfig.Tmpfs` and `Mounts`); the `/etc` paths are empty once a container ends;
  `CreatedAt` of a volume is its directory's time (F146); events of an older journal carry
  no labels.
- **Rootless has no shared networks:** a rootless compose project needs `network_mode`
  `none`, `host`, `pasta` or `service:`.
- **The Mac:** `docker.sock` takes binds outside the home as the VM's own paths (F143); a
  launchd-started host needs the user's permission for guarded folders (F140); the guest's
  kernel sometimes crashes with four VMs at once (F132); the Mac's `shipyard` still
  forwards argv (stage 13).
- **Downgrades:** a container made with `--tmpfs`, started by 0.2.0, runs without its tmpfs
  mounts (0.2.0 doesn't read `created.json`); an older 0.3 build refuses a newer
  `created.json`; an older Shipyard rewriting `index.json` drops the index digests.
- **Survivors and no-tests, each argued:** `non-empty-copied` (11.4a2b: the host's check
  under the fill lock already skips a non-empty volume); `Busy` from a prune (needs a stuck
  clean-up); the cidfile removed when a `run -d` is undone; F81's check on `retry_end`'s
  finished branch (the same line as `main`'s, which is tested); the stop-between-reads race
  in `stats`; `top`'s effective UID (needs a setuid binary in the fixture).

## Carried forward

- **Closed in stage 11:** F72 (11.1), F88 (11.2c3b), F101 (11.3a, 11.3b2), F128 (11.4a2a),
  F129 (11.4a2b), F34 (11.4b2a, 11.4b2b1, 11.4b2b2), F99 and F119 (11.5b2b), F131
  (11.5b2b: a background job's ignored SIGINT, not load), F81, F114, F116, F133 (11.6a1),
  F118, F120, F121 (11.6a2), F117 (11.6a3).
- **To stage 12** (build and images): F145 (what grew twice: four `/proc/PID/stat`
  parsers, five `PATH` lookups, the hex encoder six times, the size formatters, the big
  modules); F146 (a volume's `CreatedAt` moved by its first fill). Stage 12 starts with
  BuildKit on demand (D28), `push`, `save`/`load`, `history`, and a `Build Cache` row and a
  real `/build/prune`.
- **To stage 13** (macOS production): F132 (the guest's kernel crashes with four VMs at
  once: 2 GiB guests, the stage-10 guest, a KASAN kernel, an upstream report); F138 (a
  copied VM directory clones the card's address); F140 (guarded folders for a
  launchd-started host); F143 (`docker.sock` binds outside the home); F144 (launchd's start
  waits 150 s when `launchctl print` fails); the Mac's `shipyard` over the API (D11.5b-1).
- **To stage 15** (hardening): F29 (reap only the command's tree); F62 (`--isolate-host` for
  `bridge`); F71; F76; F94; F107; F115 (`panic = "abort"` ends the service); F122 (a `die`
  said twice, never none, in a microsecond window); F123 (a long stop holds `op.lock`);
  F124 (a firewall test, 1 in about 14); F125 (lifting a limit); F126 (a `pause` killed
  mid-freeze); F127 (`top`'s `ps` options, UTC `STIME`); F130 (a network removed before
  the boot's recovery); F134 (`--pid container:`); F135 (the monitor's failures as words;
  an enterer killed after its clone); F136 (a joined container restarted by its policy);
  F137 (the shared `/dev/shm`'s rules untested end to end); F139 (`wait?condition=removed`
  after a failed `--rm` start); F141 (`/auth` behind a redirect); F142 (the service's stop
  edges: systemd's 90 s, a 30 s keep-alive).
- **Still open from earlier:** F24, F31, F47 (a known limitation), F51, F59, F67, F69,
  F77, F79, F87, F89, F90, F92, F93, F95–F98, F100, F103, F105, F106, F108, F109, F111,
  F113.

## Tests

New and grown test files, counted by `#[test]` attributes (or a `harness = false` file's
list) in the tree on 2026-10-23, against stage 10's merge (`b2686fb`):

| File | Tests | Stage 11's |
|---|---|---|
| `shipyard-cli/tests/api.rs` | 48 | all: Docker's CLI and the raw client against the service, root and tester: lifecycle, statuses, the service's life, logs, events, attach, exec, images, volumes, networks, `cp`, views, prune, mounts, limits |
| `shipyard-cli/tests/containers.rs` | 61 | 40 new (21 at stage 10): exit codes and short forms, flags, lists and templates, `inspect`, `df`, pause/update, `top`/`stats`, prune, tmpfs and mounts, anonymous volumes and fills, joins, F119 |
| `shipyard-cli/tests/compose.rs` | 2 | all: a root project (health, volumes, ports, attached `up`, `down -v`, `network_mode`/`ipc: service:`), a rootless one (with a recreate) |
| `shipyard-cli/tests/vm_docker.rs`, `vm_launchd.rs` | 3, 3 | all: Docker's CLI and compose on the Mac; launchd holding the socket |
| `shipyard-acceptance/tests/testcontainers.rs` | 1 | `testcontainers_runs_against_shipyard` (everyday calls, the rest of what it uses, an exit wait, a pull) |
| `shipyard-api/tests/serve.rs` | 5 | all: ping, version, refusals, the body bound, the idle end |
| `shipyard-engine/tests/detached.rs` (`harness = false`) | — | 7 new: `a_failed_start_sets_off_no_restart`, `an_end_recorded_but_unsaid_is_said_once`, `one_its_policy_restarts_is_never_removed_unasked`, `an_image_a_record_names_is_kept_without_its_pin`, `an_end_unsaid_across_a_reboot_is_said_once`, `a_mark_of_another_start_says_nothing`, `docker_inspect_reads_the_image_and_the_area` |
| `shipyard-runtime/tests/containers.rs` (`harness = false`) | — | 1 new: `a_new_container_joins_another_s_namespaces` (root and tester) |
| `shipyard-api/src` | 73 | all: routes, errors, idle, hijack, create mapping, filters, exec, images, archive |
| `shipyard-engine/src` | 296 | 215 at stage 10: `docker/` 51, created, volumes, mounts, prune, scope, filters |
| `shipyard-cli/src` | 132 | 89 at stage 10: `format/` 27, `cli.rs`'s parse tests, `compose.rs` |
| `shipyard-vm/src` | 52 | 35 at stage 10: `network`, `api`, `launchd` |
| `shipyard-runtime/src` | 477 | 465 at stage 10: isolation's joins, the enterer, `forward`'s refusals, the subreaper |
| `shipyard-storage/src` | 21 | 12 at stage 10: labels, fills |
| `shipyard-sys/src` | 52 | 45 at stage 10: `listen`, `versioned`, `random` |
| `shipyard-image/src`, `shipyard-network/src` | 174, 267 | 164 and 263 at stage 10: lookups by ID, the index's new fields; network meta |

**Totals:** the close's full `verify.sh` (11.6c): macOS 1,438 passed, 0 failed (3
ignored); Linux, both passes, 4,503 passed, 0 failed (419 ignored); the VM tests 24 of 24;
fmt, clippy (macOS, glibc, musl), rustdoc, `cargo deny` and the `--cfg fuzzing` check
clean. The milestone: compose 8 of 8 as root and rootless and Testcontainers both ways on
Lima's Ubuntu 24.04, checked again after a reboot; the Mac's 11 of 11. Fuzzing: see [Fuzzing](#fuzzing).

**Mutants, hand-picked, controls first, per step:** 11.1 11/11; 11.2a 15/15, 11.2b1 10/10,
11.2b2a 6/6, 11.2b2b 8/8, 11.2c1 12/12, 11.2c2a 10/10, 11.2c2b 7/7, 11.2c3a 7/7, 11.2c3b
9/9; 11.3a 11/11, 11.3b1 12/12, 11.3b2 11/11, 11.3c1 18/18, 11.3c2 14/14, 11.3d1 15/15,
11.3d2 14/14, 11.3e1 12/12, 11.3e2 10/10; 11.4a1 8/8, 11.4a2a 12/12, 11.4a2b 13 of 14 (one
argued), 11.4b1 2/2, 11.4b2a 12/12, 11.4b2b1 11/11, 11.4b2b2 8/8; 11.5a 10/10, 11.5b1 10/10
(one more survived, `refusal-undrained`: its code was useless on macOS and removed),
11.5b2a 9/9, 11.5b2b 5/5; 11.6a1 9/9, 11.6a2 11/11, 11.6a3 8/8.

## Review history

Scores are clean-code / Murphy unless said otherwise.

| Step | What | A finding of its own reviews |
|---|---|---|
| 11.1 (7 / 8.5) | the record's frozen config; Docker's `inspect`; F72; one versioned read | `network::list` wrote, locked and failed all-or-nothing for an inspect (now `leases_of`, read only); the v3 pin didn't check key order. The VM run found the first command after `vm start` refused (`client::until_ready`) |
| 11.2a (7 / 7) | the service, the core container calls, the units | a SIGTERM could kill the service mid-create (blocked signals, a `sigwait` thread). Linux found `docker run -d` hanging on `wait`'s headers |
| 11.2b1 (7.5 / 7) | logs and events | a stalled client held the stop for good; streams could exhaust tokio's blocking pool (128, then 503) |
| 11.2b2a (6.5 / 5) | the hijack, attach, resize, attached `docker run` | the pending attach's handover race, and a pending attach outliving its client. Linux found the dropped write half losing all output |
| 11.2b2b (7 / 6.5) | exec instances and the child `shipyard exec` | a background holder of the pipes or pty hung the end for good (the drain deadline); a cooked pty doubled `\r` |
| 11.2c1 (7 / 7) | images, credentials | one unreadable image failed the list; a 1-character ID matched; removal by ID could fail half way (`remove_all`) |
| 11.2c2a (7.5 / 7) | volumes | tree walks under the container store's lock; a nameless volume not anonymous |
| 11.2c2b (7 / 7) | networks | `create {"Name":"bridge"}` took over the default network; one `labels.json` failed the list |
| 11.2c3a (7 / 6.5) | `docker cp` | a GET whose copy failed after its headers ended cleanly, so `docker cp` "succeeded"; a stalled PUT held `op.lock` unbounded |
| 11.2c3b (7.5 / 7) | F88, F29, views, inspect keys, `api_request` | a reboot in F88's window lost the `die`; stale marks could say a second one |
| 11.3a (7.5 / 7) | short forms, groups, `tag`, exit codes | `system recover`'s partial failure would have failed `shipyard-recover.service` (high) |
| 11.3b1 (7 / 6.5) | run flags, `created.json`, F62's default | Docker's CLI sends `PidsLimit: 0`, so the API never applied the default (high); a default frozen into the record could make a rootless container unstartable |
| 11.3b2 (7 / 7) | F101's views, exec/logs/stop flags, create labels | the views found the image through a tag, lost after a re-pull or `rmi -f` |
| 11.3c1 (7 / 6.5) | Go templates, Docker's columns, the filters into the engine | tabs and line breaks in values could forge rows; `pad` unbounded |
| 11.3c2 (7 / 6) | `inspect` of any object, `version`, `info`, `df` | rootless `inspect VOLUME` stopped at networks' `NeedsRoot` (high); `container inspect` searched every kind (high). The full Linux pass found 35 root tests reading the old `network inspect` through test-support |
| 11.3d1 (7 / 6.5) | `pause`, `unpause`, `update` with limits | `docker update` without `--restart` reset the policy to `no` (high, both) |
| 11.3d2 (7 / 7) | `top`, `stats` | a stream that failed after its 200; `btime` cut off on large hosts |
| 11.3e1 (7 / 6) | prune | a crash-looping `restart: always` container pruned in `__cleanup`'s gap (high); `image prune -a` going by pins alone |
| 11.3e2 (6.5 / 7.5) | `--mount`, `--tmpfs` | a tmpfs mounted after the volumes hid a volume under it (both) |
| 11.4a1 (6.5 / 7) | events' labels, the network rule, `shipyard compose` | the idle exit racing a new compose; labels crowding the engine's own attributes |
| 11.4a2a (6.5 / 7) | anonymous volumes | the find outside the lock: a rename between could take another's volumes. Linux found `rm -v` taking a volume given by name |
| 11.4a2b (7 / 6.5, round 2) | copy-up, bind sources | the first plan filled at each start (Murphy; moby fills at the create); then a refill could unpack over live data (high) |
| 11.4b1 (7 / 6.5, one reviewer) | Testcontainers; health `starting` | the regression test caught a revert only by timing |
| 11.4b2a (6.5 / 7; the design's Murphy 5) | the runtime's joins | the design: B's setup re-execed in A's user namespace, visible to A (PID joins deferred). The build: a container with its own user namespace joining an IPC namespace could never start |
| 11.4b2b1 (7 / 7) | `--network container:` | root in a user namespace `setns`-ing its own (`EINVAL`) |
| 11.4b2b2 (7 / 6.5) | `--ipc container:`, `/dev/shm` | B's init kept A's mount namespace open for its life (high) |
| 11.5a (7 / 7) | F132 measured; the card's address kept | `network.json` written without a sync, an empty one stranding the VM |
| 11.5b1 (6.5 / 6.5) | `docker.sock` on the Mac | an unlimited linger let a pooled client keep the VM up for ever; a long home was a total Mac outage |
| 11.5b2a (7 / 6) | launchd, `vm install`, compose on the Mac | a stale mark booted the VM at login; a failure before the check-in left clients hanging. A mutant's test found a cold start's 503 |
| 11.5b2b (7 / 7) | F99, F119 | the guest's welcome at its own version broke `-t` on an older CLI. The verify runs found F131's cause |
| 11.6a1 (6.5 / 7) | F81, F133, F114, F116 | `retry_end`'s finished branch had no F81 check; health after an end is Docker's `unhealthy` (checked on Docker 29.8.2) |
| 11.6a2 (8 / 8) | F118, F120, F121 | `localhost:80@evil.example` passed the loopback check: a password over plain HTTP to another host |
| 11.6a3 (7 / 7) | F117 | an exec detached by its keys died at the service's idle exit (medium-high) |
| 11.6b (API 7, security 7.5, engine 8; runtime 8, Mac 7.5, clean-code 7.5) | six fresh whole-stage reviewers in two waves | `POST /containers/{id}/rename` unrouted, so compose's recreate stopped half way (high); finishing a killed create's fill could overwrite another container's data; the volume store's mode rested on its parent's; a CLI-started host unlinked launchd's `docker.sock`. F142–F146 recorded |
| 11.6c (the proof) | fuzz 15 × 300 s, Lima Ubuntu 24.04 root and rootless through a reboot, the Mac, full `verify.sh` | the close's fuzz run built nothing (a `cfg(fuzzing)` call with an old signature): fixed, and checked by `verify.sh --mac` since |
| 11.7 (measured review; plan Murphy: 3 high, 3 medium) | the measured review's findings: the API's streams, the Mac's hang-up, `ps`, `update`'s put-back, fills, made bind sources | the Mac's first fix stopped at the host: Virtualization.framework passes a close on as a half-close, so the guest was told by `API_HANG_UP` (found by the VM test, measured in the guest); `make_sources` held every API bind (a symlink rule would refuse `/var/run/docker.sock`); a made source's directory gone after a reboot never started again; an older runtime would have ignored `made_below` (plan v2) |

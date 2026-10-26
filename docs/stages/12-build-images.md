# Stage 12 notes: build and images

The engineering record of stage 12: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [Images](../user/images.md) (building, pushing, signing
in, saving and loading, importing, history), [Containers](../user/containers.md)
(`export`), [The Docker API](../user/api.md) and [Shipyard on a Mac](../user/macos.md).

## Outcome

Stage 12 asked for two things ([ROADMAP](../ROADMAP.md), [FEATURES
§12](../FEATURES.md#12-build-and-images--stage-12)), decided on 2026-10-15 (D28):

1. **`shipyard build`,** BuildKit started on demand in a container and stopped after the
   build, Dockerfiles in full. Probed as step 12.1 (what BuildKit needs in a Shipyard
   container), built as step 12.3.
2. **The image commands:** `push`, `tag`, `login`/`logout`, `image inspect` / `history` /
   `save` / `load`, `import`/`export`. `tag` and `image inspect` came with stage 11
   (11.3a, 11.2c1); the rest were built as 12.2 (a: `save`, `load`, `history`; b:
   `import`, `export`) and 12.4 (`push`, `login`, `logout`).

Step 12.5 took the F-rows homed here: 12.5a the behaviour fixes (F146, F148, F149 (a)–(c),
F151), 12.5b F145's consolidation (b1 the shared helpers, b2 the big files and long
functions), 12.5c the close (c1 gap tests and the whole-stage reviews; c2 these notes, the
fuzz run, the full verification and the milestone).

From the hardening track the stage took on H1 (build, the archives, push and the
credentials in the engine and the image crate; the CLI and the API thin), H2 (every new
file and wire versioned from its first write: a volume's `created.json` v1, the forwarded
`Request` v3; Docker's own formats read as Docker writes them), H3 (a threat model in
every plan; each step's Murphy review run against its design before building, 12.2a to
12.5b2), H5/H8 (OrbStack's Docker 29.8.2 both ways for `save`/`load`/`export`/`import`,
distribution's `registry:3` for `push`, `differential.sh` against the tree before 12.5b2)
and H6 (`LeftOut`, `Refused`/`Denied`, the API tests by status). No H7 budget was added:
nothing new is on a run's path (checked in 12.5c1).

It closed F145, F146, F148 and F151 (rejected, with evidence: an exec's command already
gets its container's limits), and F149's (a)–(c); F149 (d) moved to stage 15 (D12.5-2).
It opened F156–F162, all homed in stage 15. A user can now:

- `shipyard build -t app:1 .`, with `-f` (`-f -` too), `--build-arg`, `--target`,
  `--no-cache`, `--pull`, `--label`, `-q`, `--progress`, `--secret id=X,src=FILE`, and
  `-o type=local,dest=DIR` or `-o type=tar,dest=FILE` instead of an image;
- `shipyard builder prune -f`, and see the cache in `system df`'s `Build Cache` row;
- `shipyard push registry.example/app:1` (or `push -a` for every tag), after
  `shipyard login registry.example` (`-u`, `-p`, `--password-stdin`, or a prompt), and
  `shipyard logout`; a sign-in shared with `docker login` both ways;
- `shipyard save -o app.tar app:1`, `shipyard load -i app.tar` (Docker's archives, old and
  new, and BuildKit's OCI layout, plain or gzipped), `shipyard history app:1`;
- `shipyard export -o fs.tar web` (a running or stopped container's own files) and
  `shipyard import fs.tar base:1 --change 'CMD ["/bin/sh"]'`;
- through the API: `docker save`, `docker load`, `docker history`, `docker export`,
  `docker import`, `docker push`, `docker builder prune`;
- on a Mac: all of the above through Shipyard's VM, the context in the home; `login` and
  `logout` on the Mac itself, a forwarded `push` or `pull` handed the Mac's credential.

| Requirement | Where | Tested by |
|---|---|---|
| `build`: BuildKit in a container started for the build and removed at its end (D28, D12-2, D12.3-1) | the engine's `build/` (`mod.rs`: `BuildRequest`, `BUILDKIT_IMAGE`; `builder.rs`: `build`, `Job`, `Removed`, `SCRIPT`; `args.rs`: `checked`, `buildctl_args`); the CLI's `building.rs` | `a_dockerfile_builds_and_runs`, `a_failing_build_leaves_nothing`, `an_interrupted_build_leaves_nothing`, `a_build_without_a_name_runs_nothing`; units `a_request_is_buildctl_s_arguments`, `options_are_given_only_when_asked`, `what_a_build_refuses_before_it_runs` |
| Dockerfiles in full: multi-stage, cache mounts, build args, targets, secrets | BuildKit's `dockerfile.v0` frontend; secrets copied 0400 and bound at `/secrets` | `a_dockerfile_builds_and_runs` (the secret's hash written by its `RUN`: the positive control; the secret in no layer) |
| Its cache kept (a directory per context, not the volume first planned: D12.3-5; FEATURES says so since the close) | `build/cache.rs` (`state_name`, `builder_dir_of`, `usage`, `prune`, `remove_builders`) | `the_build_cache_is_kept_and_pruned`, `a_killed_build_s_builder_goes_with_its_cache`; units `a_context_s_state_is_named_by_its_path`, `a_state_is_found_from_what_it_left`, `root_s_builder_is_beside_the_store` |
| `-o type=local` / `type=tar`; the output read as hostile | `builder.rs` (`open_output`, `outputs`); `image::load_named` | `a_build_s_files_come_out`; units `a_build_s_output_is_only_a_file`, `a_build_s_output_takes_its_names_alone` |
| A `Build Cache` row; a real `/build/prune`; `builder prune`; `system prune`'s cache | `cache::{usage, prune}`, the API's `images.rs` (`POST /build/prune`), the CLI's `prune.rs` | `the_build_cache_is_kept_and_pruned`, `system_df_counts_what_is_in_use`, `docker_prunes` |
| `push`, `push -a` (D12.4-1's defaults) | `shipyard-image`'s `push.rs` (`open`, `push`, `WORKERS`), `registry/upload.rs` (`blob_exists`, `upload_blob`, `put_manifest`); the engine's `image/push.rs` (`push`, `names_a_tag`); the CLI's `signing.rs` | `a_pushed_image_pulls_back_the_same` (both the CLI's and the image crate's), `a_shared_layer_is_pushed_once`, `a_push_never_flattens_the_index_it_came_from`, `a_name_gives_a_tag_after_its_last_slash` |
| A push's sign-in: `pull,push` scope, refused vs denied, no credential elsewhere | `Registry::{with_auth, for_push}`, `same_authority`, `RegistryError::{Refused, Denied}` | `a_push_signs_in_for_push_and_says_denied_from_refused`, `an_upload_led_elsewhere_is_never_sent_there`, `a_redirected_upload_is_refused`, `an_upload_gets_over_what_goes_wrong_midway`, `an_expired_token_at_the_commit_resends_only_the_commit`; units `a_blob_is_read_by_offset`, `a_digest_joins_the_location_s_query`, `the_registry_is_known_by_its_scheme_host_and_port` |
| `login`, `logout` where `docker login` keeps them (D12.4-1) | `credentials/write.rs` (`Credentials::keep`, `forget`); the engine's `image/signing.rs` (`login_and_keep`, `logout`) | `a_sign_in_is_kept_and_removed`; units `a_sign_in_keeps_the_rest_of_the_config`, `a_broken_config_is_left_as_it_is`, `the_file_keeps_its_link_and_mode`, `a_config_keeps_its_owner`, `a_helper_keeps_and_forgets_it`, `a_sign_out_removes_every_spelling`, `a_killed_write_s_temporary_is_swept`, `a_config_is_read_bounded_through_its_link`; the prompt's keys `a_hidden_line_s_keys` |
| The Mac's sign-in: `login`/`logout` on the Mac, its credential handed to a forwarded `push`/`pull` (D12.4-2) | the CLI's `forward.rs`; `image::docker_credential_for`, `GivenCredential`; the runtime's `forward::{REQUEST_VERSION, REGISTRY_AUTH_FD}`; the guest's `agent.rs`; the CLI's `signing::take_handed` | `a_handed_credential_signs_a_pull_in` (Linux: the CLI's side), `a_credential_goes_and_comes_back_as_docker_s` |
| `image inspect`, `tag` | stage 11 (11.2c1, 11.3a) | `images_are_docker_s`, `a_built_image_is_tagged_pushed_and_runs_elsewhere` |
| `history` (read leniently) | `shipyard-image`'s `history.rs`; the engine's `docker::image_history`; the CLI's `format/rows/images.rs` | `history_shows_docker_s_table`, `docker_saves_loads_and_shows_history`; units `a_history_is_read_as_written`, `a_history_that_isnt_the_spec_s_is_read_as_far_as_it_goes`, `a_history_is_bounded`, `a_history_row_is_docker_s` |
| `save`: Docker 25's archive, the store's own bytes (D12.2-2) | `archive/save.rs`; the engine's `image::{save, check_saved}`; the CLI's `output::write_archive_to` | `a_saved_image_loads_elsewhere_and_runs`, `a_save_is_docker_s_layout_and_the_same_each_time`, `a_save_of_a_missing_image_writes_nothing`, `save_into_a_fifo_or_a_device_never_replaces_it`, `a_save_past_the_descriptor_limit_is_refused_up_front`; units `a_saved_archive_loads_back_as_it_was`, `a_shared_blob_is_saved_once`, `an_image_saved_with_its_index_loads_by_its_digest`, `a_blob_missing_from_the_store_is_the_store_s` |
| `load`: three formats, hostile, staged (D12.2-2, D12.2-3) | `archive/load.rs`, `archive/staging.rs`, `archive/legacy.rs`; the engine's `image::{load, load_named}`, `Loaded::lines`; `Index::record_many` | `a_load_moves_a_tag_and_says_so`, `an_unsound_archive_loads_nothing_and_asks_no_registry`, `a_prune_beside_a_load_takes_nothing_of_it`, `a_killed_loads_staging_is_swept`, `a_tag_two_images_claim_names_the_last`, `an_archive_s_words_are_escaped`; units `an_archive_is_read_never_extracted`, `what_isnt_sound_is_refused`, `an_archive_past_its_bounds_is_refused`, `an_archive_past_its_entries_bounds_is_refused`, `a_manifest_past_the_layers_bound_is_refused`, `a_legacy_archive_gets_a_manifest_of_its_own`, `a_legacy_hard_link_is_its_target`, `an_oci_layout_is_named_by_its_annotations`, `names_and_platforms_are_checked`, `a_killed_load_s_staging_is_swept` |
| `export`: a container's own files, running or stopped (D12.2-5, D12.2-8) | the engine's `cp::export_by_name`, `run::Wanted::OwnFiles`, the `export` helper, `Packer::within_mount`; the runtime's `start_helper_alone`, `rootfs::clone_root_alone`, `HelperRoot`; `shipyard_sys::fs::mount_id` | `export_leaves_out_volumes`, `export_s_walk_keeps_to_the_root_mount`, `a_created_container_s_export_makes_no_volume`, `a_rm_container_ending_during_its_export_fails_it` |
| `import`: a one-layer image, named, `--change` (D12.2-4, D12.2-6, D12.2-7, D12.2-9) | `shipyard-image`'s `import.rs`, `changes.rs` (`Changes::parse`); the engine's `image::import` (`ImportRequest`, `Imported`) | `an_exported_container_imports_and_runs`, `an_unsound_import_records_nothing`; units `an_imported_tar_is_a_layer_of_its_own_digest`, `what_isnt_a_whole_tar_imports_nothing`, `a_gzip_of_two_members_is_read_whole`, `a_gzip_read_a_byte_at_a_time_is_gzip`, `an_image_archive_imported_is_noticed`, `each_instruction_is_docker_s`, `what_import_doesnt_take_is_refused`, `only_this_host_s_platform_is_its_own` |
| The archives, push and the build prune through the API | the API's `images.rs` (save, load, history, `/images/create?fromSrc=-`, push, `/build/prune`), `archive.rs` (export, `body_input`, `pump_body`), `query.rs` (`Query::get_all`) | `docker_saves_loads_and_shows_history`, `docker_exports_and_imports`, `docker_pushes_through_the_api`; units `save_load_and_history_are_found`, `every_value_of_a_key_is_had` |
| The Mac: archives as files in the home, a build of a home context (12.5c1) | the CLI's `forward.rs` (`terminal_hint`, `dockerfile_from_terminal`); `builder.rs` (`on_the_macs_home`) | `archives_cross_as_files_in_the_home`, `a_build_in_the_home_runs`; units `save_and_load_name_their_files`, `a_dockerfile_is_never_read_from_a_terminal`, `the_macs_home_is_told_by_its_mount` |
| **The v0.4 milestone:** an image built, tagged, pushed, pulled by its digest elsewhere, run | the whole of the above | `a_built_image_is_tagged_pushed_and_runs_elsewhere` (tester 10 s, root 16 s, 12.5c1); by hand (12.5c2): Ubuntu 24.04 (AppArmor's restriction on) and Fedora 44 (SELinux enforcing), root and rootless, through a reboot, 42/42 each; the Mac through its VM 13/13 |
| The store's images built on (12.5c3: `FROM`, `COPY --from` an image only the store has; `--pull` asks the registry, Docker's rule) | the engine's `build/local.rs` (`wanted`, `lay_out`, `buildctl_args`: a named context a name over an OCI layout of hard-linked blobs); `builder.rs` (`Job::local_images`, the `/layout` bind) | `a_local_image_is_built_from_without_a_pull` (the `--pull` failure the control; a stage of the image's name built as the stage); units `the_images_a_dockerfile_could_name_are_given`, `a_stage_s_name_is_never_a_store_image_s`, `a_build_is_given_a_bounded_number_of_images`, `the_layout_links_the_store_s_blobs`, `each_name_is_a_context_over_the_layout` |
| A cache unused for a week removed (12.5c3) | `cache.rs` (`mark_used`, `sweep_unused`, `unused`, `prune_where`); `builder.rs` (`unused_swept`) | `the_build_cache_is_kept_and_pruned` (a month-old state gone, a day-old one kept); unit `a_state_unused_for_a_week_is_found` |
| Archives the caller's alone; root's `-o type=local` never through a link (12.5c3) | the CLI's `output::write_archive_to` (0600); `builder.rs` (`output_dir`: `shipyard_sys::fs::make_below` from `/`) | units `an_archive_written_to_a_file_is_the_caller_s_alone`, `a_root_build_s_files_never_go_through_a_link` (root) |
| F146: a volume's `CreatedAt` never moves | `shipyard-storage`'s `volume.rs` (`created.json` v1, `Volumes::created`) | `a_volume_s_making_stays_its_own` |
| F148: an exec's start given back when nothing ran; a finished session shut both ways | the API's `exec.rs` (`Instance::claim`, `unclaim`), `exec/session.rs` | `a_start_s_claim_is_given_back_when_nothing_ran`, `a_finished_exec_s_session_ends_both_ways` |
| F149 (a)–(c): a self-bound service's idle end, no start limit, `service.log` | the API's `socket.rs` (`Bound::unlink`); `contrib/systemd/*.service` (`StartLimitIntervalSec=0`); the engine's `api_service.rs` | `an_idle_service_s_socket_goes_before_it_does`, `the_api_units_say_what_the_design_needs`, `a_service_s_words_are_logged_and_the_log_is_bounded` |
| F151 (rejected): an exec's descriptor limit is its container's | unchanged: the runtime's snapshot of the container's limits | `an_exec_gets_its_container_s_descriptor_limit` |
| `exec -i` input as it comes (found in 12.5a) | the API's `exec/session.rs` (`pass_on`) | `an_exec_s_input_reaches_it_as_it_comes` |
| F145: one helper each (`/proc/PID/stat`, `PATH`, hex, the clock, sizes); the big files split | `shipyard_sys::{proc, path, hex, time}`; the CLI's `format/rows/units.rs`; the splits of 12.5b2 | `a_stat_line_s_fields_come_from_after_the_name`, `what_s_gone_and_what_isn_t_a_line`, `this_process_s_stat_reads`, `a_program_is_found_where_execvp_would`, `every_byte_is_two_lowercase_digits`, `a_time_s_seconds_since_the_epoch`, `sizes_are_docker_s`, `go_g_is_go_s`; the whole suite and `differential.sh` for the moves |

**Moved out of stage 12:**

| Item | Now in | Why |
|---|---|---|
| F156: Docker's own `docker build` / `docker buildx` through the API (buildx asks for `Privileged`; no `/build`) | stage 15 | D12-2: `shipyard build` is the way to build; the API stays as it was |
| F157: dangling images (`load` of an untagged image, `import` or `build` with no name refused) | stage 15 | D12.2-3, D12.2-9, D12.3-2: Shipyard keeps no untagged images; refusing is said and safe |
| F158: as root, a build's context must be readable to every user | stage 15 | D12.3-3: an ID-mapped mount (Linux 5.12) would let the builder read as the caller |
| F159: a context from standard input or a URL, `--ssh`, `--cache-from`/`--cache-to`, other platforms, a private base image's credentials | stage 15 | each is one of `buildctl`'s own options, none needed for the milestone |
| F160: cross-repository mounts, resumable uploads, pushing an index, two sign-ins at once, the flatten guard's reach, a stalled API client's push | stage 15 | 12.4's Murphy review: optimisations and Docker's own behaviour |
| F161: every root build one host ID range (`RLIMIT_NPROC` shared) | stage 15 | 12.5c1's security review: a denial of service between root's builds only |
| F162: `login` as root through a user's config link | stage 15 | 12.5c1's security review: Docker's `configfile.Save` follows the link too |
| F149 (d): a connection cap and a stalled body's timeout | stage 15 | D12.5-2: only the socket's owner can connect (`SO_PEERCRED`) |
| F107 widened: the load's staging `Flock`s | stage 15 | 12.2a: internal |
| `POST /build` | 404 (F156) | as above, said in `api.md` |

## Architecture

### The crates

No crate is new. What each gained:

- **`shipyard-image`:** `archive/` (`save.rs`, `load.rs`, `staging.rs`, `legacy.rs`: the
  archives), `history.rs`, `import.rs` and `changes.rs`, `push.rs`, `registry/upload.rs`,
  `credentials/write.rs` (the reader moved into `credentials.rs` beside it),
  `from_store_by_digest`, `Index::record_many`, `Store::open_sized`.
- **`shipyard-engine`:** `build/` (`mod.rs`, `args.rs`, `builder.rs`, `cache.rs`),
  `image/archive.rs`, `image/import.rs`, `image/push.rs`, `image/signing.rs`,
  `image/given.rs` (Docker's credential form, `X-Registry-Auth`, moved from the API so the
  CLI's forwarding uses it too), `cp::export_by_name` and the `export` helper,
  `ErrorKind::Build`, `WarningKind::{ExportPartial, ImportedArchive}`.
- **`shipyard-runtime`:** `start_helper_alone` (a helper whose root is a running
  container's root mount alone), `HelperRoot`, a helper's `note` and `wait_noting`; the
  forwarded `Request` v3 with `registry_auth`, `REGISTRY_AUTH_FD`.
- **`shipyard-sys`:** `fs::mount_id` (12.2b), and 12.5b1's `proc`, `path`, `hex`, `time`.
- **`shipyard-api`:** the archive, history, import, export and push endpoints;
  `/build/prune` real; `exec.rs` split into `exec/{spawn,session}.rs` and `create.rs` into
  `create/{mounts,host,network}.rs` (12.5b2).
- **`shipyard-cli`:** `building.rs` (`build`, `builder prune`), `signing.rs` (`push`,
  `login`, `logout`, `take_handed`), the archive commands in `images.rs` and
  `containers.rs`, `output::write_archive_to`; `format/rows.rs` split (12.5b2).
- **`shipyard-vm`:** `client.rs` split into `client/{host,ask,command,session}.rs`
  (12.5b2); nothing else changed.
- **`shipyard-test-support`:** `push_registry` (a registry that takes pushes, with scoped
  tokens and misbehaviours on demand), `stores::Stores`, `Scratch::{on, shipyard, ok}`,
  `eventually_every`, `eventually_some`.

### A build

```
shipyard build -t app:1 .           (the CLI: building.rs → engine build::build)
 │  checks first: names (-t or -o, not both), the context a directory not holding the
 │  store, every file readable to the builder (root: o+r, o+rx, ancestors o+x; the Mac's
 │  home share as the caller's own), /etc/subuid and /etc/subgid clear of root's range
 │  BuildKit's image found by its digest, else pulled (before signals are held off)
 │  <builder>/state-<16 hex>.lock taken exclusive (one build of a context at a time;
 │  "waiting for another build" after NOTICE_AFTER, 1 s; given up after 1 h)
 │  sweep: builders labelled com.shipyard.builder=<state> removed, its ….run-… removed
 │  its lock's time stamped (its last use); other states unused 7 days removed (tried)
 │  HeldOff from here: a Ctrl-C ends the follow, then the clean-up, then the signal
 ▼
shipyard-build-<12 hex>   run::create + run::start (no auto-remove), moby/buildkit:v0.34.1
 │  in a user namespace: rootless the caller's; root 0 → 2100000000 (ROOT_FIRST), 65,536
 │  CAP_SYS_ADMIN in it alone; seccomp unconfined; the default network
 │  binds:  /context            the context                       read-only
 │          /in                 ….run-…/in: Dockerfile (+ .dockerignore), 0400   read-only
 │          /secrets            ….run-…/secrets: each secret, 0400   read-only
 │          /out                ….run-…/out: what it writes
 │          /var/lib/buildkit   state-<16 hex>/: the context's cache
│          /layout             ….run-…/layout: an OCI layout of the store's images the
│                              Dockerfile names, blobs hard-linked   read-only
 │  sh -c SCRIPT sh ARGS…    buildkitd --oci-worker-no-process-sandbox; ready within 30 s
 │                           (else exit 90: NOT_READY); buildctl "$@" (argv, never this
 │                           shell's); buildctl prune --keep-storage 10240; buildkitd
 │                           sent SIGTERM, waited for 10 s; buildctl's code
 ▼
its logs followed as Progress::Build lines (the last 20 kept); run::wait; removed (Removed)
out/image.tar → image::load_named (the -t names alone), or out/files → the unpacker
(-o local, the caller's ownership), or out/files.tar → the writer (-o tar); each opened
below out/ by open_output: no symlink, a regular file, no FIFO wait
```

A `RUN` step runs as BuildKit's own process in the builder (no process sandbox: the
rootless mode). The reach probe (12.3) showed what it reaches: the builder's processes and
their command lines, never their files through `/proc/PID/root`. So `buildctl`'s argv
carries only what the image records anyway (build args, labels); secrets go by file.

### Places on disk

| Place | Owner, mode | What |
|---|---|---|
| `<store>/builder/` (rootless) | the caller, 0700 | the builder's directory: the caller is its builder's root |
| `<store>-builder/` (root) | root, 0711, checked by `unsafe_dir` | beside the store: root's builder (another ID) can't enter root's 0700 store |
| `…/state-<16 hex>/` | the builder's root, 0700 | BuildKit's root for one context: the first 16 hex digits of the sha256 of its canonical path |
| `…/state-<16 hex>.lock` | the caller | the build's lock: outermost in the lock order |
| `…/state-<16 hex>.run-<12 hex>/` | the builder's root | one build's `in/`, `secrets/`, `out/`; removed at its end, a Ctrl-C included; a killed build's swept by the next or by a prune |
| `<store>/blobs/.load-<pid>-<n>-<nanos>/` | the caller | a load's or an import's staging: made as `….new`, renamed once its `flock` is held; blobs checked against their digests there; promoted under the store's shared lock; one a killed load left swept by the next load and the store's collection when its lock is free (`sweep_staging`) |
| Docker's `config.json` | kept as found (mode, link; owner by root only) | `login`'s `auths` entry, or the helper's `store`; a `.config.json.…` temporary of a killed write swept after 10 minutes (`LEFT_BEHIND`) |
| `volumes/<name>/created.json` | the caller | v1, `{"version":1,"created":<secs>}`: F146; best effort on a run's making |
| `service.log` beside `api.sock` | the caller | F149 (c): the service `ensure` starts; moved aside to `.1` past 4 MiB |

### Files and formats

| File or wire | Version | Notes |
|---|---|---|
| `save`'s archive | Docker 25's form | an OCI layout (`oci-layout`, `index.json`, `blobs/sha256/…`) with `manifest.json` (no `repositories`: the design named it, the build left it out, as Docker 25 can read without it); deterministic headers; an image pulled through an index carries that index (12.3's fix) |
| `load`'s input | Docker 25+, an OCI layout alone (BuildKit's), Docker's legacy archive; plain or gzip | bzip2, xz and zstd refused by their magic; bounds `MAX_DOCUMENTS` (16 MiB held in all), `MAX_PATHS` (4,096), `MAX_NAMES` (1,024), `MAX_ENTRIES` (1,024), `MAX_LAYERS` at the parse, `MAX_BYTES` (64 GiB decoded), 8 link hops |
| `import`'s layer | gzip, its `diff_id` the decoded tar's sha256 | one pass: decode, hash, gzip, hash; the tar read to its end marker; `MAX_IMPORT_BYTES` 16 GiB |
| `--change` | Docker's nine instructions | `MAX_CHANGES` 64, `MAX_CHANGE` 64 KiB, an `EXPOSE` range of at most `MAX_RANGE` 1,024 ports |
| The forwarded `Request` | 3 (2 written without a credential) | `registry_auth` (hidden from `Debug`): a guest of an older build reads every other request and refuses only that one |
| `created.json` (a volume's) | 1 | a newer or damaged one is a warning in `inspect`, the directory's time instead |

### The Mac's credential hand-over

```
shipyard push reg/app:1  (the Mac)     forward.rs: push/pull only → docker_credential_for
   │                                   (the Mac's helper, its keychain; 15 s; a failing
   │                                   helper warned, forwarded without)
   ▼
Request v3 { registry_auth }           over the VM's wire; never in an environment
   ▼
guest agent (agent.rs)                 a pipe; the child's pre_exec puts it at
   │                                   descriptor 3 (REGISTRY_AUTH_FD),
   │                                   SHIPYARD_REGISTRY_AUTH_FD=3 names it
   ▼
shipyard push (the guest)              main: take_handed first (one thread): the variable
                                       removed, descriptor 3 taken (fd::take_inherited),
                                       read whole (16 KiB at most), closed
```

`login` and `logout` never cross: they run on the Mac (`Place::Here`), against its own
helpers.

### Locks

Stage 11's order holds (AGENTS §4). Stage 12 added:

- **0, a build's context lock** (`state-….lock`), outermost: the builder container is made
  and removed under it (taking 1–6 inside as any create and removal do). A prune and
  `system df` only *try* it: a build running keeps its state, said as busy.
- **Before 4, a load's staging lock**, taken with no other held and kept across the
  store's shared lock for the promotion.
- **A push holds the store's shared lock only to open its blobs** (`push::open`, as
  `save`'s), then uploads by offset (`pread`) from those descriptors with no lock held.
- **An export holds the container's `op.lock` while it streams** (F60's rule): a `stop`
  meanwhile waits up to 120 s (`OP_WAIT`), as for `cp`.
- **Docker's config has no lock** (Docker's has none): two sign-ins at once can lose one
  (F160).

## Decisions and why

**The stage (2026-10-23, asked):**

- **D12-1:** five steps: 12.1 a probe first (what BuildKit needs in a Shipyard
  container), then 12.2 the archives (a build's output lands through `load`), 12.3 build,
  12.4 push and sign-in, 12.5 the F-rows and the close.
- **D12-2, how build drives BuildKit: B.** The engine runs `moby/buildkit` itself, in a
  user namespace with `CAP_SYS_ADMIN` in it, the keyring allowed, BuildKit's rootless mode
  with no process sandbox, `buildctl` from the image. The alternative, A (Docker's
  `docker buildx` against the API, as compose), would have needed `Privileged: true` in
  the API, and privileged without a user namespace a writable cgroupfs: it would undo the
  threat model. Cost: Docker's own `docker build` through `docker.sock` doesn't work (F156).

**12.2, the archives:**

- **D12.2-1:** split: 12.2a `save`/`load`/`history`, 12.2b `import`/`export`.
- **D12.2-2:** `save` writes Docker 25's form, each image's stored blobs as they are, so
  its ID and digests are kept. A legacy archive's image gets a manifest of its own, so its
  ID differs from Docker's config-digest ID (documented).
- **D12.2-3:** `load` refuses an image the archive gives no name, said; F157.
- **D12.2-4:** `import --change` takes the config instructions; a URL source refused.
- **D12.2-5:** `export` of a running or stopped container, its own filesystem only:
  volumes, binds and tmpfs left out; what a volume covers exported as it is underneath.
- **D12.2-6:** `import` gzips its layer in one pass (decode, hash the `diff_id`, gzip,
  hash the blob).
- **D12.2-7:** `--change` by a small parser of Shipyard's own (a flag's value, no
  owned-surface row), fuzzed.
- **D12.2-8:** a running container's export through a non-recursive clone of its root
  mount (`open_tree` without `AT_RECURSIVE`), made by the helper in its namespaces before
  its capabilities drop; where the kernel refuses it (`EINVAL`, `EPERM`, `ENOSYS`,
  `EACCES`), the live tree walked with every other mount skipped by its mount ID
  (`statx`'s `STX_MNT_ID`, never `st_dev`: overlayfs reports a lower layer's), said.
- **D12.2-9:** `import` with no name is refused, as `load`'s untagged image.

12.2a's plan Murphy review (two critical, five high) changed the load before it was
built: resolved by digest from the staged blobs, never its names; every name parsed, a
digest name only the image's own; bounded memory; no store lock while bytes come; a cut
archive (no tar end marker) refused.

**12.3, build:**

- **D12.3-1:** a BuildKit container per build, gone at its end. Built without
  `auto_remove`: the engine waits for its end and reads its last lines, then removes it
  (the plan's Murphy review).
- **D12.3-2:** `build` without `-t` and without `-o` refused before anything runs.
- **D12.3-3:** the context bound read-only; as root, a file the builder can't read fails
  the build, said; F158.
- **D12.3-4:** Docker's flags as listed above; the context a directory (F159).
- **D12.3-5, a cache per context:** `state-<16 hex>/` by the context's canonical path. A
  project's builds share theirs, nothing crosses projects: a hostile Dockerfile can only
  touch its own project's later builds. One lock per state, so builds of different
  projects run at once. The alternative, one cache per store, would have let any build
  poison every other's.
- **Root's builder IDs from 2,100,000,000** (12.3's Murphy review, high): the plan's
  `--uidmap 0:100000:65536` is the first user's own subordinate range on Ubuntu, Debian
  and Fedora, so that user could have become the builder's root and reached its cache. A
  range no user is given, checked against `/etc/subuid` and `/etc/subgid` each build.
- **The builder's places beside root's store** (found by 12.3's tests): root's builder
  is another user and can't enter the 0700 store.

**12.4, push and sign-in (2026-10-25):**

- **D12.4-1:** `login` writes where `docker login` does: the registry's `credHelpers`
  helper, else `credsStore`'s, else `auths` in the file (warned as unencrypted), every
  other key kept. Docker and Shipyard share one sign-in both ways. Defaults chosen without
  asking: a push sends the stored manifest (its ID kept; an image pulled through an index
  is pushed as its platform's manifest, refused if that would replace the index at the
  tag: `WouldFlatten`); uploads as Docker's CLI (`POST`, `PATCH`, `PUT ?digest=`); no
  cross-repository mount (F160).
- **D12.4-2:** on a Mac, `login`/`logout` run on the Mac; a forwarded `push` or `pull` is
  handed the Mac's credential for its image's registry, never stored in the guest. The
  plan's Murphy review moved it from an environment variable to a request field and a
  pipe at descriptor 3: no `/proc/PID/environ` holds it, no child inherits it.

**12.5, the F-rows and the close (2026-10-25):**

- **D12.5-1:** 12.5 split: a the behaviour fixes, b F145's consolidation, c the close.
- **D12.5-2:** F149 (d) to stage 15: only the socket's owner can connect.
- **D12.5-3:** F145 split: b1 the shared helpers and duplicates, b2 the big files and
  long functions (a move with no behaviour change, checked by `differential.sh`).
- **D12.5-4:** one size formatter, Docker's units everywhere: pull and push progress
  change from `1.0 KiB` to Docker's pull's `2.048kB` (precision 4, go-units' `HumanSize`).
- **D12.5-5:** the close in two: c1 gap tests and the whole-stage reviews; c2 the notes,
  the docs, fuzz, `verify.sh`, the milestone.
- **D12.5-6:** the close's reviews: clean-code and Murphy over the whole diff, plus an
  adversarial security reviewer (a hostile Dockerfile with seccomp unconfined, hostile
  archives, credentials written and handed to the guest).
- **D12.5-7:** the milestone by hand on Ubuntu 24.04 arm64 (AppArmor's restriction on)
  and a fresh Fedora (SELinux enforcing), root and rootless, through a reboot, and on the
  Mac through Shipyard's VM.
- **D12.5-8:** the book's stage-12 chapters after these notes.

## Kernel checks

**`scripts/probes/buildkit.sh`** (12.1, 2026-10-23), on the Linux test container
(OrbStack's kernel 7.2.12, arm64), `moby/buildkit:latest` and `:rootless`, a two-stage
Dockerfile with a cache mount; root, and `tester` with its delegated cgroup:

| Setting | Root | Rootless | Fails on |
|---|---|---|---|
| Shipyard's defaults | no | no | mounts (`operation not permitted`); the rootless image: rootlesskit's user namespace (`EPERM`) |
| `--cap-add SYS_ADMIN` / `ALL`, with or without `seccomp=unconfined` | no | — | each `RUN`: runc can't make its cgroup (`/sys/fs/cgroup` read-only) |
| `--oci-worker-no-process-sandbox` without a user namespace | no | — | BuildKit refuses it (`can't enable NoProcessSandbox without Rootless`) |
| the rootless image, any seccomp, caps, IDs | — | no | `newuidmap` (no file caps, then IDs past the container's 65,536), then rootlesskit's child hangs |
| the root image in a user namespace, `SYS_ADMIN`, no process sandbox, Shipyard's seccomp | no | no | each `RUN`: runc's keyring (`add_key`: `operation not permitted`) |
| **the same with `seccomp=unconfined`** | **yes, 5 s** | **yes, 1 s** | none; the cache in a directory the namespace's root owns: 3 steps cached after a restart |

BuildKit in a user namespace runs in its rootless mode: no cgroup of its own, so nothing
of the host's cgroup tree is written. A namespace's root can't reach a named volume (the
store is 0700): the cache needs a directory that ID owns. buildx's `docker-container`
driver asks the API for `Privileged: true` (F156). Untested: a seccomp profile of
Shipyard's own with `add_key`/`keyctl` instead of unconfined.

**The export clone** (12.2b, through the helper's real path, OrbStack 7.2): a
non-recursive `open_tree("/", OPEN_TREE_CLONE)` made in the container's namespaces after
the joins and before the capabilities drop works as root and rootless: an export of a
running alpine with a volume on `/etc/apk` shows alpine's own `/etc/apk`. The forced walk
writes `etc/apk`, `etc/hosts` and `proc` as places only. The fallback walk needs
`STX_MNT_ID` (Linux 5.8). The plan's `scripts/probes/export-clone.sh` wasn't kept: the
probe ran through the helper itself.

**The reach probe** (12.3, `moby/buildkit:v0.34.1`, root and rootless): a `RUN` sees the
builder's processes and their command lines (`buildkitd`, `buildctl build …`,
`buildkit-runc`) but can't read through `/proc/<pid>/root` (the context, the state:
nothing listed). Rootless needs a delegated cgroup (runc: "no cgroup mount found").

**The Mac's home share** (12.5c1, in Shipyard's VM): through the virtiofs share tagged
`home` the guest sees each file as the asker's own: a 0700 tree under a 0700 tree was
read by UID 2100000000. So on that mount the modes say nothing: the context is checked
as a rootless caller's (`on_the_macs_home`, by the mount the context is on, its ID in
`/proc/self/mountinfo`).

**Found by tests, each now in AGENTS §10:**

- **BuildKit copies a context file it can't read as an empty one,** exit 0: the context is
  checked whole first (12.3).
- **`std::io::copy` from a UNIX socket into a pipe uses `splice` and held a typed line
  until the client closed** (12.5a's probe: `docker exec -i` answered 15 s late): a
  read-then-write loop.
- **A file written whole by rename replaces whatever has the name,** `/dev/null` or a FIFO
  as easily as a file (12.2a's Murphy review): `write_archive_to` writes into anything
  that isn't a regular file.
- **`--uidmap 0:100000:65536` is the first user's own subordinate range** (12.3).
- **`test-linux.sh`'s passes have no DNS for a rootless container** (12.3's verify): the
  build tests run in steps of their own after both passes.
- **`ImageReference::parse` fills in `latest`** (12.4's Linux run): whether a tag was
  given is read from the text (`names_a_tag`).

**H5, against Docker by hand:** OrbStack's Docker 29.8.2 (its containerd store): its
`save` of `alpine` loads in Shipyard with the same `RepoDigests` and `history` rows; and
Shipyard's `save` loads in Docker with the same ID and runs (12.2a). Its `export` of
alpine imports and runs; Shipyard's export of it is 8,943,104 bytes as Docker's, the same
names (directories now named with a trailing `/`, as Docker's), and Docker imports and
runs it (12.2b). distribution's `registry:3`, run by Shipyard: `busybox:1.37` pushed,
pushed again (every blob already there), pulled into another store with the same ID and
run, `push -a` of two tags (12.4).

## macOS and Linux differences

- **Every build, archive, push and sign-in runs in the guest on a Mac,** but `login` and
  `logout`, which run on the Mac against its helpers (its keychain).
- **A forwarded `push` or `pull`** carries the Mac's credential in `Request` v3 and gets it
  on descriptor 3; on Linux the command reads Docker's config itself. A forwarded
  `run`/`create` gets none: a private image is pulled first (documented).
- **The context and every file a command names** (`-f`, secrets, `-o`, `save -o`,
  `load -i`, `import FILE`, `export -o`) must be in the home on a Mac, as `cp`'s; `-` for
  standard input declares its input, and is refused from a terminal on both (`build -f -`
  on a Mac read a typed Dockerfile until 12.5c1).
- **The home share's modes** say nothing in the guest: a build there is checked as a
  rootless caller's (12.5c1); on Linux, root's build needs a context readable to all.
- **Linux only:** the builder (`build/builder.rs` is `cfg(target_os = "linux")`), the
  export helper and its clone, the stores' extraction.

## Failure-mode register

| Case | Handling | Test |
|---|---|---|
| A hostile archive's member names (`../../x`, absolute, links) | matched by name to blobs and documents, never extracted | `an_archive_is_read_never_extracted` |
| An archive's blob that isn't its digest; a layer that isn't its `diff_id` | refused as it's staged; the layer checked as it's unpacked, as a pull's | `what_isnt_sound_is_refused` |
| A store blob removed by an `rmi` while a build links it | linked under the store's shared lock (no collection meanwhile); one already gone leaves that image out, so it's pulled as before | `the_layout_links_the_store_s_blobs` (a missing blob's image left out) |
| A rootless builder that remounts `/layout` writable (`CAP_SYS_ADMIN` in its namespace) and writes a linked blob | the store's blob is the caller's own data; a pull checks each layer's `diff_id` at its unpack; a `RUN` step never sees the builder's files (the reach probe). As root the blobs are root's, 0444, an ID the builder's namespace doesn't map: unwritable | none: the builder is BuildKit's pinned image, trusted |
| Another context's state in use while the sweep runs | its lock only tried; a build of it keeps it | the prune's `busy` path, as `builder prune`'s |
| A sweep or prune killed half way through a state's removal | the state's directory renamed to a build directory's name (`set_aside`) under its lock first: its next build removes it and starts afresh | `a_state_is_listed_once_and_set_aside_whole` (the kill itself: none, a `remove_dir_all` can't be stopped at a chosen file) |
| A store image that can't be linked (another filesystem, no room) | left out, said, pulled as before; never copied under the store's lock | `the_layout_links_the_store_s_blobs` (a bad digest's case) |
| `rm` the moment a container with no policy ended (its clean-up between letting it go and its policy's decision) | the restart lock taken only for an end the policy starts again (`restart_policy::next_delay`): ended for good, removed | `one_no_policy_restarts_is_removed_at_once`, `a_user_s_stop_never_waits_for_the_restart` (the policy's case: `one_its_policy_restarts_is_never_removed_unasked`) |
| A Dockerfile stage named as a store image (`FROM x AS base`, an image `base`) | the image never given by a stage's name (BuildKit puts a named context in the stage's place) | `a_stage_s_name_is_never_a_store_image_s`; end to end in `a_local_image_is_built_from_without_a_pull` |
| `-o type=local`'s directory a link another user planted (root) | refused (`ELOOP`/`ENOTDIR` by `make_below`), nothing written where it leads | `a_root_build_s_files_never_go_through_a_link` |
| An archive missing a layer | fails the load; never asks a registry (resolved by digest, fetch policy never) | `an_unsound_archive_loads_nothing_and_asks_no_registry` |
| An archive cut short (between members, or mid-member) | the tar's end marker required (`EndSeen`); nothing recorded, the staging removed | `an_unsound_archive_loads_nothing_and_asks_no_registry`, `what_isnt_sound_is_refused` |
| An archive past its bounds (documents, entries, layers, names, decoded bytes) | refused before the work (`MAX_ENTRIES` before any entry is read) | `an_archive_past_its_bounds_is_refused`, `an_archive_past_its_entries_bounds_is_refused`, `a_manifest_past_the_layers_bound_is_refused` |
| A load killed mid-stream | its staging (`.load-…`, held by its `flock`) swept by the next load or the store's collection once free; nothing promoted | `a_killed_loads_staging_is_swept`, `a_killed_load_s_staging_is_swept` |
| A sweep taking a staging directory being made | made as `….new`, renamed once locked | `a_killed_load_s_staging_is_swept` (12.2a's review fix) |
| A prune while a load is still reading | the blobs are staged outside the store; promotion under the shared lock | `a_prune_beside_a_load_takes_nothing_of_it` |
| A load over a tag another image has | the tag moves, said ("was sha256:…"); the old image keeps its other names | `a_load_moves_a_tag_and_says_so` |
| Two images in one archive claiming a tag | the last wins, said once from the final records | `a_tag_two_images_claim_names_the_last` |
| An archive's words with escapes | left out or shown `printable`; the API's lines `plain` | `an_archive_s_words_are_escaped`, `a_registry_s_words_are_made_plain` |
| `save -o /dev/null` (or a FIFO, a link) as root | written into, never renamed over; a link truncated | `save_into_a_fifo_or_a_device_never_replaces_it` |
| A save of more blobs than descriptors | the CLI raises its soft limit; a save past it refused up front | `a_save_past_the_descriptor_limit_is_refused_up_front` |
| A save of an image the store lost a blob of | the store's error (Storage), as push says it | `a_blob_missing_from_the_store_is_the_store_s` |
| An import out of space, past 16 GiB, or cut | `NoSpace`, `TooLarge`, `Cut` (never "a bad archive"); nothing recorded | `what_isnt_a_whole_tar_imports_nothing`, `an_unsound_import_records_nothing` |
| An import of an image archive | warned ("`load` it") | `an_image_archive_imported_is_noticed` |
| A hostile `--change` | bounded, typed refusals; never a shell | `what_import_doesnt_take_is_refused`; fuzz `import_change` |
| A volume shared with another container leaking through an export | the root mount cloned alone; a stopped export takes no volume | `export_leaves_out_volumes`, `a_created_container_s_export_makes_no_volume` |
| The kernel refusing the clone | the walk by mount ID, said | `export_s_walk_keeps_to_the_root_mount` (forced); the `ENOSYS`/`EACCES` fallback itself untested |
| A `--rm` container ending during its export | `lost_its_layer` fails the export, never exit 0 with a mixed archive | `a_rm_container_ending_during_its_export_fails_it` |
| A symlink out of a container during an export | written as a link, never followed; a FIFO never opened | `export_leaves_out_volumes` |
| A hostile Dockerfile's `RUN` | in the builder's user namespace, `SYS_ADMIN` only there; it sees processes, not files (the reach probe); the context read-only; its own project's cache only | `a_dockerfile_builds_and_runs`; the probe |
| A repository's `Dockerfile` (or its `.dockerignore`) that's a link to `~/.ssh/…` (12.5c1, high) | `read_within`: resolved inside the context or `-f`'s directory, else refused | `a_dockerfile_link_leading_out_is_refused` |
| Secrets read by a frontend as the Dockerfile's files (12.5c1) | bound apart at `/secrets` | `a_request_is_buildctl_s_arguments` |
| A build's output that's a symlink or a FIFO the builder planted | `open_output`: below `out/` by descriptor, a regular file, no FIFO wait | `a_build_s_output_is_only_a_file` |
| A tampered `image.tar` naming `postgres:16` | loaded under the `-t` names alone (`named_only`) | `a_build_s_output_takes_its_names_alone` |
| A context file the builder can't read (BuildKit would copy it empty) | the context checked whole first; refused by name | `an_unreadable_context_file_is_said` (with its control) |
| A context under an unenterable parent (`/root/…`) | its ancestors checked `o+x` | `an_unreadable_context_file_is_said` |
| A context that holds the store | refused | `what_a_build_refuses_before_it_runs` |
| Root's builder range overlapping a user's subordinate range | refused, naming the user | `a_subordinate_range_overlapping_the_builder_s_is_found` |
| A build's CLI killed (SIGKILL) | its builder runs on until the next build of that context sweeps it under the lock, or a prune removes it with the cache | `a_killed_build_leaves_the_next_free`, `a_killed_build_s_builder_goes_with_its_cache` |
| Ctrl-C mid-build | `HeldOff`: the follow ends, the builder and the run directory (its secrets) go, then the signal | `an_interrupted_build_leaves_nothing` (within 60 s) |
| A user's `rm -f` of the builder mid-build | said as that, not a build failure | review fix; no test |
| `container prune` taking a builder mid-build | builder-labelled containers left to the build and `builder prune` | `a_killed_build_s_builder_goes_with_its_cache` |
| A prune of a state a build is using | its lock only tried: left, said as busy | no test (the same `try_state_lock` `usage` uses) |
| `system prune --filter` wiping every cache | a filtered prune leaves the cache | `the_build_cache_is_kept_and_pruned` |
| BuildKit never ready | exit 90, said as that (`Environment`), not a build failure | review fix; no test |
| A first pull of BuildKit with signals held off | pulled before `HeldOff` | review fix |
| An offline host | BuildKit's image found by its digest, whatever it's named: a `load` serves | the build tests (loaded from an archive kept in cargo's temporary directory) |
| An upload's `Location` on another host or port | refused, never sent to (credentials or not); no redirect followed | `an_upload_led_elsewhere_is_never_sent_there`, `a_redirected_upload_is_refused` |
| A 500 or a 401 mid-upload; a lost commit | started over from `POST`, the blob reread by offset; a lost commit found by `HEAD` | `an_upload_gets_over_what_goes_wrong_midway` |
| A token expiring at a large layer's commit (Docker Hub's 300 s; 12.4's Murphy review, high) | the commit alone re-signed and sent again | `an_expired_token_at_the_commit_resends_only_the_commit` |
| A credential refused vs an account without push access | `Refused` (sign in again) vs `Denied` (Docker's words) | `a_push_signs_in_for_push_and_says_denied_from_refused` |
| A push that would replace a multi-platform index at its tag | refused (`WouldFlatten`) | `a_push_never_flattens_the_index_it_came_from` |
| A layer shared by two tags of `push -a` | sent once | `a_shared_layer_is_pushed_once` |
| A prune during a push | blobs opened under the store's shared lock, then read by offset | no test of its own (the same opening as `save`'s, tested there) |
| A push or `login` killed mid-write of Docker's config | written whole (temporary, `fsync`, rename); a leftover temporary swept | `a_killed_write_s_temporary_is_swept` |
| A config Shipyard can't parse | refused, never overwritten | `a_broken_config_is_left_as_it_is` |
| A symlinked config; another user's config as root | written through to its target; mode kept; owner kept by root only | `the_file_keeps_its_link_and_mode`, `a_config_keeps_its_owner` |
| A config that's a FIFO | `read_bounded`: never waited on | `a_config_is_read_bounded_through_its_link` |
| `logout` with several spellings of the registry | every one removed | `a_sign_out_removes_every_spelling` |
| Ctrl-C at the password prompt | read byte by byte with echo, line editing and signal keys off; Ctrl-C cancels at once; the terminal restored | `a_hidden_line_s_keys` |
| The Mac's handed credential leaking into a child | read and closed first thing in `main`, the variable removed; never in an environment | `a_handed_credential_signs_a_pull_in` (its control: the pull refused without it) |
| A failing Mac helper on a forwarded pull | warned, forwarded without (15 s wait) | review fix |
| An exec's spawn failing (F148) | its start given back, a retry allowed | `a_start_s_claim_is_given_back_when_nothing_ran` |
| A client queued at a self-bound service's idle end (F149 a) | its socket's name removed and its lock let go before the last look at the backlog | `an_idle_service_s_socket_goes_before_it_does` |
| A crashing request retried until systemd's start limit (F149 b) | `StartLimitIntervalSec=0`; the socket's trigger limit instead | `the_api_units_say_what_the_design_needs` |
| A volume's time moved by its first fill (F146) | its making kept in `created.json`; a volume made before reads its directory's time | `a_volume_s_making_stays_its_own` |
| A refused body leaving Docker's CLI a broken pipe | the rest read and dropped (up to 1 GiB, a minute; 8 drains at once) | review fix; the early 400 checked with Docker's CLI by hand |

## Fuzzing

- **`load_archive`** (new, 12.2a): any bytes, plain or gzipped, read as `load` reads them
  into a scratch store's staging, then its images found; a canary directory beside the
  store must keep its file; the same bytes read as a config's history. Seeds of each
  format in `scripts/fuzz-seeds.py`. 300 s, 3,454,724 runs (12.2a); 300 s, 3,112,905 runs
  (12.2b); no crash.
- **`import_change`** (new, 12.2b): any bytes as `--change` lines; what it makes stays
  within its bounds. 300 s, 6,881,081 runs, no crash.
- **`unpack`** (an import's layer is the unpacker's): 300 s, 1,860,954 runs (12.2a);
  1,577,448 (12.2b); no crash.
- **Not fuzzed, argued:** the registry's upload answers (headers, bounded, no new parser:
  the token and challenge parsers are `challenge`'s target); Docker's `config.json` (the
  user's own file, read bounded by serde); `created.json` (serde JSON, versioned).
- **The close's run** (12.5c2): all 17 targets, 300 s each, no crash; runs: `manifest` 12.8M, `challenge` 16.8M, `reference` 19.9M, `tar` 13.5M, `unpack` 1.7M, `netlink` 29.7M, `ports` 45.1M, `dns` 12.8M, `proc_net` 16.9M, `cri_record` 22.2M, `logs_dir` 5.1M, `attach_frames` 14.1M, `process_json` 12.6M, `cp_unpack` 2.4M, `api_request` 4.5M, `load_archive` 3.3M, `import_change` 4.9M.

## Known limitations (by design, for stage 12)

- **Docker's `docker build` and `docker buildx`** don't work through the API (F156);
  `POST /build` answers 404.
- **The builder is a wider grant than a container:** seccomp unconfined in its user
  namespace (BuildKit needs the keyring), said in the docs; build only Dockerfiles you'd
  run. Every root build shares one host ID range (F161).
- **As root, a context must be readable to every user** (F158); a Mac's home is exempt
  (12.5c1).
- **No untagged images:** `load` of an unnamed image, `import` without a name and `build`
  without `-t` or `-o` are refused (F157).
- **`build`:** no context from standard input or a URL, no `--ssh`, no cache import or
  export, no other platform, no private base image (F159). A build's progress is
  BuildKit's plain lines.
- **`push`:** no cross-repository mount, no resumed upload, one platform's manifest; two
  sign-ins at once can lose one (F160). A push through the API goes on when its client
  goes, as a pull does (Docker's daemon too).
- **Approximations, documented:** `history`'s `SIZE` is the compressed layer's (as
  `images`'); a legacy archive's image ID differs from Docker's; `save` by ID gives an
  untagged archive.
- **Survivors and no-tests, each argued:** the CLI's terminal refusals of `save`/`load`
  (a pty harness for two lines; the Mac's are unit-tested); the descriptor raise itself
  (process-wide); the `ENOSYS`/`EACCES` clone fallback, the drain's bound of 8, the
  staging reserve's look-ahead, a stopped `--rootfs` export's walk (can't be made to
  happen here); a prune of a busy state, a prune of a killed build's run directories,
  the `/etc/subuid` refusal end to end (its parser unit-tested and mutated); the guest
  agent's descriptor 3 through a VM, a registry's `Docker-Content-Digest` naming another
  digest, a stalled `PATCH` reaching its send timeout, a prune during a push, the API's
  every-tag push (Docker's CLI 24 sends a tag); an exec's give-back on a real failed
  spawn, `inspect`'s fallback on a damaged `created.json`, a queued connection served at
  the idle end; an interrupt held off before a start's runtime create (12.5b2: no test
  reached it before or after).

## Carried forward

- **Closed in stage 12:** F145 (12.5b1, 12.5b2), F146 (12.5a), F148 (12.5a), F149's (a)–(c)
  (12.5a), F151 (12.5a, rejected with evidence).
- **From the readiness review (12.5c3), recorded:** F164 (the API's `save` descriptor
  budget, the idle end's last connection, archive `EPIPE`), F165 (`load`'s duplicate
  entries and bounds, `--change`'s cost, the fuzz target's reach), F166 (the builder's
  readiness bound, Ctrl-C in the output, whole images through disk, root's by-path
  context, NSS subordinate IDs), all to stage 15; F167 (an old guest's refusal of a
  v3 request) to stage 13.
- **To stage 15** (hardening): F149 (d) (D12.5-2); F156 (buildx through the API); F157
  (dangling images); F158 (root's context through an ID-mapped mount); F159 (build's other
  inputs); F160 (push's mounts, resumes and index; concurrent sign-ins); F161 (a range of
  IDs per state, or the builder's `pids` limit); F162 (root's `login` through a user's
  link); F107 widened (the staging `Flock`s).
- **Decisions to revisit:** a seccomp profile of Shipyard's own for the builder (the
  keyring calls allowed) instead of unconfined: untested in 12.1.
- **Found by the close's Fedora milestone, fixed:** every rootless run with a named volume
  failed where `newuidmap` has file capabilities instead of the setuid bit (Fedora): the
  volume's fill helper (`process/helper.rs`, `made`) was non-dumpable while its starter
  mapped it, and a non-dumpable process's `/proc/PID/uid_map` is root's. From stage 11's
  fill (11.4a2b), whose milestone ran on Ubuntu alone (setuid `newuidmap`). It's dumpable
  for that window now, and hidden again once root in its namespace;
  `a_volume_fills_where_newuidmap_has_file_capabilities` (root: a file-capability copy of
  the tools on a rootless user's `PATH`) fails without it with Fedora's own error.

## Tests

New and grown test files, counted by `#[test]` attributes (`grep -c '#\[test\]'`) in the
tree on 2026-10-25, against stage 11's merge (`635d6ef`, `git show main:FILE`). A file
whose count fell lost tests to a split, not to a deletion (12.5b2's moves; the API's
platform test moved to the engine in 12.5c1).

| File | Tests | Stage 12's |
|---|---|---|
| `shipyard-cli/tests/archive.rs` | 12 | all: `save`, `load`, `history` end to end, root and tester |
| `shipyard-cli/tests/export_import.rs` | 6 | all: `export` running and stopped, the walk, `import`, the `--rm` race |
| `shipyard-cli/tests/build.rs` | 10 | all, with the network: builds, the cache, the outputs, killed and interrupted builds, root's context; the v0.4 milestone |
| `shipyard-cli/tests/push.rs` | 3 | all: push and pull back, sign-in kept and removed, the handed credential |
| `shipyard-image/tests/push.rs` | 8 | all: the registry client against `push_registry`'s misbehaviours |
| `shipyard-cli/tests/api.rs` | 57 | 7 new (50 at stage 11): `docker save/load/history`, `export/import`, `push`; F148, F149 (a), F151, `exec -i` |
| `shipyard-cli/tests/vm.rs` | 14 | 2 new (12): `archives_cross_as_files_in_the_home`, `a_build_in_the_home_runs` |
| `shipyard-image/src` | 209 | 174 at stage 11: `archive/tests.rs` 14, `credentials/write/tests.rs` 8, `import/tests.rs` 5, `history.rs` 3, `changes.rs` 2, `registry/upload.rs` 2, `registry.rs` +1 |
| `shipyard-engine/src` | 314 | 300 at stage 11: `build/` 10 (args 3, builder 4, cache 3), `image/` 4 (archive, given, import, push), `api_service.rs` +1; `docker/top.rs` −1 (12.5b1) |
| `shipyard-cli/src` | 138 | 132 at stage 11: `format/rows/` split 12 → 16 (12.5b2), `forward.rs` +2, `signing.rs` 1, `cli.rs` +1; `compose.rs`, `output.rs` −1 each (12.5b1) |
| `shipyard-api/src` | 77 | 75 at stage 11: `routes.rs` +1, `query.rs` +1, `exec/tests.rs` +1, `create/mounts/tests.rs` 1 (moved); `create/tests.rs`, `images/tests.rs` −1 each |
| `shipyard-sys/src` | 60 | 54 at stage 11: `proc.rs` 3, `path.rs` 1, `hex.rs` 1, `time.rs` 1 |
| `shipyard-storage/src` | 22 | 21 at stage 11: `a_volume_s_making_stays_its_own` |
| `shipyard-vm/src` | 52 | 52: `client.rs`'s 9 moved into `client/tests.rs` (6), `client/host/tests.rs` (1), `client/session/tests.rs` (2) |

`shipyard-engine/tests/detached.rs`, `shipyard-cli/tests/terminal.rs` (`harness = false`)
and `shipyard-runtime/tests/containers.rs` changed only to use test-support's helpers
(12.5b1): no test added or removed.

**Totals per step** (`verify.sh`, all of it, each step):

| Step | macOS passed / failed / ignored | Linux both passes | VM |
|---|---|---|---|
| 12.2a | 1,465 / 0 / 3 | 4,596 / 0 / 420 | 25/25 |
| 12.2b | 1,472 / 0 / 3 | 4,624 / 0 / 420 | 25/25 |
| 12.3 | 1,480 / 0 / 3 | 4,674 / 0 / 422; build steps tester 7 + 1 ignored, root 8 | 25/25 |
| 12.4 | 1,502 / 0 / 3 | 4,726 / 0 / 422, the build steps ok | 25/25 |
| 12.5a | 1,505 / 0 / 3 | 4,740 / 0 / 422, the build steps ok | 25/25 |
| 12.5b1 | 1,507 / 0 / 3 | 4,735 / 11 / 422 in a DNS outage (the 11: the build tests); re-run root 8, tester 7 + 1 ignored | 25/25 (re-run) |
| 12.5b2 | 1,510 / 0 / 3 | 4,752 / 0 / 410 (rootless 2,150, root 2,602, the build steps included) | 25/25 |
| 12.5c1 (touched parts) | — | engine, image, API units rootless 312/206/77, root 313/209/77; `api` 53; build tester 9 + 1 ignored, root 10 | 27/27 |
| **12.5c2 (the close)** | 1,516 / 0 / 3 | 4,773 / 0 / 423 (both passes and the build tests' steps) | 27/27 |

fmt, clippy (macOS, glibc, musl), rustdoc, `cargo deny` and the `--cfg fuzzing` check
passed at each step (rustdoc failed once each in 12.2a, 12.2b, 12.3 and 12.4 on a
bracket read as a link, and once in 12.5c2 on links 12.5c1's moves left, each fixed and
re-run). The milestone, by hand
(12.5c2, `milestone12.sh`, a static musl build): Ubuntu 24.04 arm64 (AppArmor's restriction
on) and Fedora 44 (SELinux enforcing), root and rootless, through a reboot, 42/42 each (17
checks before, 4 after, for each); the Mac through a VM of its own and a Docker config of
its own, 13/13 (a build of a home context, `login` kept on the Mac, `push` and `pull`
signed in by the handed credential, save/load). Fuzzing: see
[Fuzzing](#fuzzing).

**Mutants, hand-picked, controls first, per step:** 12.2a 24/24 (a first round's 4
"survivors": one real gap, `a_shared_blob_is_saved_once` added; two commands that ran no
test; one replacement equal to the text); 12.2b 17/17 (`sniff-whole` first survived: the
`while` around a `take(6).read_to_end` did nothing, removed); 12.3 16/16 (5 macOS, 8
tester, 3 root); 12.4 13/13; 12.5a 9/9; 12.5b1 6/6; 12.5b2 4/4; 12.5c1 9/9 (two first
"survived": the Mac's ran no test, so the runner now fails a control that runs none; the
prune's test was vacuous, its builder running, which `container prune` never takes); 12.5c2
1/1 (the fill helper made non-dumpable again: Fedora's error).

**The differential** (12.5b2): `differential.sh --base-dir` against the tree copied before
the step: macOS identical (100 cases), tester and root identical (173 each) once
Docker's `"Pid"` is normalised.

## Review history

Scores are clean-code / Murphy unless said otherwise; counts are the findings by
severity.

| Step | What | A finding of its own reviews |
|---|---|---|
| 12.1 (no reviews: research) | the BuildKit probe; D12-2 = B | buildx asks the API for `Privileged`; BuildKit in a user namespace needs only the keyring beyond `SYS_ADMIN` |
| 12.2a (plan Murphy: 2 critical, 5 high; built: Murphy 8 + 3 small, clean-code 19) | `save`, `load`, `history`, `load_archive` | the plan resolved a load by its names (a pull from a registry for a missing blob); unbounded re-reads of `index.json` entries; `save -o /dev/null` replaced by a rename; a sweep could take a staging directory being made |
| 12.2b (plan Murphy: 3 high, 4 medium, 2 low; built: clean-code 1 medium bug, 2 medium gaps, 11 low; Murphy 3 medium, 6 low) | `import`, `export`, the root clone, `import_change` | the clone had to be made before the helper's capabilities drop; a `--rm` container ending mid-export gave a mixed archive, exit 0; an import out of space said as a bad archive. H5 found Docker's trailing `/` on directories |
| 12.3 (plan Murphy: 4 high, 7 medium, 3 low; built: Murphy one high, and clean-code) | `build`, the cache per context, `builder prune`, `Build Cache` | root's builder at the first user's own subordinate range (high); Ctrl-C left the builder and the copied secrets; a filtered `system prune` wiped every cache. Testing found BuildKit's empty copy of an unreadable file and root's builder unable to enter the store |
| 12.4 (plan Murphy: 5 high, 7 medium, 5 low; built: clean-code 0 high, 5 medium, 10 low; Murphy 1 high, 2 medium, 4 low) | `push`, `login`, `logout`, the Mac's hand-over | a large layer never pushable where tokens last 300 s (high: the whole upload restarted at each commit's 401); Ctrl-C at the prompt waited for Enter. The Linux run found `push -a` refusing every name |
| 12.5a (clean-code 2 high, 3 medium, 6 low; Murphy 1 medium-high, 1 medium, 5 low) | F146, F148, F149 (a)–(c), F151 rejected | the idle unlink kept the lock, so a queued long client left every new one failing (both reviews). Testing found `exec -i`'s `splice` |
| 12.5b1 (Murphy nothing above low; clean-code 9 low) | F145's helpers: `proc`, `path`, `hex`, `time`, sizes | `top` read the stat line as text, so a process naming itself non-text bytes vanished |
| 12.5b2 (clean-code 8.5/10, 2 medium, 10 low; Murphy no change of behaviour) | F145's splits and long functions; `differential.sh --base-dir` | `ReadyToCreate`'s doc claimed an order the code doesn't follow. An own slip: a `git checkout` of `rows/tests.rs` threw away the stage's uncommitted lines, restored from the copy (AGENTS.md's gotcha) |
| 12.5c1 (clean-code 7.5, Murphy 7.5, security 7) | the whole-stage reviews; gap tests and the v0.4 milestone test | every build on a Mac's home refused (Murphy, high, confirmed in a VM); a repository's Dockerfile link handed BuildKit any file (security, high); a killed build's builder pruned out from under; F161, F162 |
| 12.5c2 (the proof) | these notes, the docs, fuzz 17 × 300 s, `verify.sh`, the milestone (Ubuntu, Fedora, the Mac) | Fedora's milestone: every rootless run with a named volume refused where `newuidmap` has file capabilities (from 11.4a2b): fixed, with a root test |
| 12.5c3 (the readiness review: runtime 8.5, image 8, security 8, engine 7.5, CLI 7.5) | the four medium findings fixed; the rest F164–F167; the full run's one failure (a stage-11 race: `rm` refused as restarting the moment a no-policy container ended, 0/40 alone) fixed, mutants 2/2. Then the PR's own review (clean-code: yes after fixes; Murphy below): a store image put in a same-named stage's place, `./out` refused as root, `stop` then `rm` still racing, the index error unsaid: fixed, mutants 3/3; a sweep "removing the lock file" was wrong. Murphy (yes after fixes): the sweep's listing quadratic in lock files (one pass now), a sweep killed half way leaving half a state its next build used (set aside first), a link failure failing the build (said, pulled; no copy), a stage named on a continued line, digests unchecked as paths: fixed, mutants 3/3; the rest in F166 | a build couldn't start `FROM` an image only the store had (engine); `-o` archives 0644 (CLI); no bound on the caches of contexts gone (engine); root's `-o type=local` followed a planted link (security). BuildKit's named contexts probed first, on v0.34.1 |

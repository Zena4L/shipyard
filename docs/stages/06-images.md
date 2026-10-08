# Stage 6 notes: images and storage

The engineering record of stage 6: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [Images](../user/images.md), [Volumes](../user/volumes.md)
and [Running commands](../user/run.md).

## Outcome

Stage 6 asked for images and storage: registry pulls, OCI and Docker manifests, safe layer
unpacking, a content-addressed store, overlayfs and volumes ([ROADMAP](../ROADMAP.md),
[FEATURES §6](../FEATURES.md#6-images-and-storage--stage-6)). A container no longer needs a
hand-built root filesystem. `shipyard run --image alpine:3.20` does all of this:

- finds the image in the store, or pulls it from its registry (signed in with Docker's
  credentials when it needs to be);
- verifies every byte against the digests that name it;
- extracts each layer once, by directory descriptor, with its owners, modes and
  capabilities;
- stacks the layers into an overlay inside the container's own namespaces;
- runs the image's command with Docker's rules.

Rootless runs work too: a normal user's layers are extracted inside a user namespace of
their own, so they belong to the user's subordinate IDs, as Podman's do. `shipyard pull`,
`images`, `rmi`, `image prune`, `system df` and `volume …` manage what's stored, and runs,
pulls and collections can all happen at once.

The stage was built once, then reviewed and largely rebuilt (see [Review
history](#review-history)): the first version had four ways for an image to reach the host's
files, and no locking.

| Requirement | Where | Tested by |
|---|---|---|
| Image references: Docker's grammar, Hub defaults, one canonical name (`docker.io/library/alpine:latest`) | `shipyard-image/reference.rs` | `docker_hub_defaults_apply`, `docker_hubs_other_names_are_docker_hub`, `what_the_grammar_refuses_is_refused`, `the_canonical_form_round_trips`; the `reference` fuzz target |
| OCI and Docker v2 manifests, indexes, configs; the platform chosen with its variant; `null` lists | `shipyard-image/manifest.rs` | `an_index_selects_the_wanted_platform`, `the_best_variant_wins_and_a_newer_one_never_does`, `docker_writes_unset_lists_as_null_and_strings_as_empty`, `the_parsers_never_panic_on_arbitrary_bytes`; the `manifest` fuzz target |
| A registry client: Bearer and Basic sign-in, retries with `Retry-After`, bounded bodies, HTTPS (OS trust) except loopback | `shipyard-image/registry.rs` | `quoted_commas_escapes_and_several_challenges`, `a_token_realm_off_https_is_refused_before_any_request`, `retries_stop_without_progress_or_at_the_cap`, `tests/pull.rs`; the `challenge` fuzz target |
| Credentials from Docker's config: `auths`, `credsStore`, `credHelpers`, identity tokens | `shipyard-image/credentials.rs` | `a_registrys_helper_comes_before_the_store_and_the_store_before_auths`, `a_helper_that_hangs_or_floods_is_stopped`, `debug_hides_the_secrets`, `a_private_image_pulls_with_the_password_from_dockers_config`, `an_identity_token_is_exchanged_by_the_refresh_grant` |
| Pulls that check everything: a digest pin, index entries, sizes, free space; resumable with `Range` | `shipyard-image/pull.rs`, `store.rs` (`BlobWriter`) | `a_pinned_digest_is_checked_against_the_bytes`, `a_lying_registry_is_refused`, `a_blob_longer_than_its_size_is_refused_at_its_size`, `a_cut_transfer_goes_on_from_where_it_stopped`, `a_pull_goes_on_from_what_a_stopped_one_left` |
| A content-addressed store, private to its owner, shared by concurrent processes (`gc.lock`) | `shipyard-image/store.rs`, `index.rs` | `the_store_is_private`, `the_lock_is_shared_or_exclusive_and_goes_with_its_holder`, `records_made_at_once_are_all_kept`, `a_corrupt_index_is_moved_aside_and_stops_collection` |
| Safe layer extraction, by directory descriptor: no escape; whiteouts, opaque directories, owners, modes, times, capabilities; digest and `diff_id` checked | `shipyard-image/unpack.rs`, `tar_reader.rs`, `fsat.rs` | `nothing_is_written_through_a_symlink`, `a_whiteout_or_opaque_marker_never_reaches_through_a_symlink`, `a_hard_link_can_only_reach_what_the_layer_made`, `the_digests_are_verified`, `as_root_owners_whiteouts_opaque_markers_and_capabilities_are_made`; the `tar` and `unpack` fuzz targets |
| Rootless extraction and removal in a user namespace (the storage helper) | the CLI's `storage_helper.rs`, `image.rs` | `rootless_layers_belong_to_subordinate_ids_and_are_removed`, `a_volume_a_rootless_container_wrote_as_another_user_is_removed` |
| Layers placed by one `rename(2)`, removed through trash | `shipyard-image/images.rs` | `a_layer_placed_by_another_process_is_kept`, `a_removal_cut_short_leaves_trash_not_a_half_emptied_layer`, `two_first_runs_of_an_image_at_once_share_its_layer` |
| A writable layer per run, alive while locked, swept when stale | `shipyard-image/containers.rs` | `an_area_is_live_while_held_and_stale_after`, `each_run_has_its_own_writable_layer_and_removes_it`, `a_killed_runs_writable_layer_is_swept_by_the_next` |
| Garbage collection that keeps what runs and pulls use: `rmi`, `image prune` | `shipyard-image/index.rs`, the CLI's `image.rs` (`collect`) | `layers_running_containers_use_are_kept`, `an_image_in_use_is_kept_until_its_container_ends`, `a_prune_during_a_pull_waits_for_it`, `temporaries_go_and_are_not_reported` |
| The overlay mounted in the container's namespaces: the new mount API, a legacy fallback, hints when refused | `shipyard-runtime/process/rootfs.rs` (`overlay`), `failure.rs`, `isolation.rs` (`Overlay::validate`) | `an_image_of_sixty_layers_runs`, `legacy_options_are_relative_and_bounded`, `an_overlay_refusal_says_why`, `a_store_on_overlayfs_is_explained` |
| `--uidmap` with an image, as root: shifted layer copies and a parent-built overlay | `unpack.rs` (`Shift`), `spawn.rs` (`prebuilt_overlay`), `rootfs.rs` (`staging`) | `a_shift_maps_each_id_to_its_host_id`, `mapped_layers_are_named_by_their_maps`, `an_image_with_uidmap_belongs_to_the_containers_root` |
| `run --image`: Docker's entrypoint, command, working directory and env rules; `--pull` | the CLI's `run.rs` (`check`, `Planned::finish`) | `the_command_is_dockers`, `an_explicit_workdir_wins_even_slash`, `an_images_env_never_reads_the_host`, `volumes_and_entrypoints_as_docker_has_them`, `a_missing_working_directory_is_made`, `a_bad_flag_costs_no_pull` |
| Named volumes and bind mounts, held while in use | `shipyard-storage/volume.rs`, the CLI's `volumes.rs` | `a_held_volume_is_never_moved_aside`, `no_volume_name_meets_a_lock_or_the_trash`, `a_volume_in_use_is_never_removed`, `volume_prune_without_a_terminal_needs_force` |
| Commands: `pull`, `images`, `rmi`, `image prune`, `system df`, `volume create/ls/inspect/rm/prune` | the CLI's `main.rs`, `image.rs`, `volumes.rs` | `rmi_goes_on_past_an_image_it_cant_remove`, `system_df_counts_images_containers_and_volumes`, `a_pulled_image_runs_without_its_registry` |

**Moved out of stage 6:**

| Item | Now in | Why |
|---|---|---|
| `tag`, `inspect`, `history`, `save`/`load`, `import`/`export` | stage 9 | they name images and containers the way stage 9's state does; the store and index are ready for them |
| zstd layers | later (a C dependency, feature-gated) | gzip covers almost every published layer; a zstd layer is refused by name |
| The `fuse-overlayfs` fallback | later, as a helper outside the container | see [Decisions](#decisions-and-why): run inside the container, the daemon would let container root out |
| `shipyard login` | later | `docker login` writes the config Shipyard reads |
| tmpfs mounts, mount propagation options for `--volume` | stage 9 (`--mount`) | they come with the full mount syntax |
| `push`, signature verification | later ([FEATURES §6](../FEATURES.md#6-images-and-storage--stage-6)) | pulling comes first |

Paths below are relative to `crates/`.

## Architecture

```
shipyard run --image REF [CMD]        (shipyard-cli/run.rs)
  RunArgs::check: every flag, the spec, the --volume binds   ← no side effects: a typo
  image::prepare(REF, --pull, uidmap)                          costs no download
    store: open_store (0700), gc.lock SHARED
    sweep_stale: areas whose flock is free (killed runs)
    ready:
      from_store (missing) or pull (always / not stored):
        Reference::parse → Registry (credentials looked up at the first 401)
        manifest/index (pin checked) → config → layers (sizes, free space)
          each blob: BlobWriter (.partial, flock, Range resume) → verify → rename
        Index::record (index.lock, temp + fsync + rename)
      extract_layers: per missing layer
        root: unpack_layer in-process;  rootless: storage helper in a user namespace;
        root + --uidmap: in-process with Shift (layers/<hex>-map-<id>)
        → .tmp-<name>/diff → syncfs → rename to layers/<name>
    ContainerArea::create: containers/.new-… → flock → rename to run-…
    gc.lock released (the area's own flock keeps its layers alive)
  Planned::finish(image): command, env, workdir, user; Root { overlay }; volumes held
  supervise → the init, in the container's namespaces (process/init.rs):
    rootfs::enter → mount_overlay: fsopen/fsconfig(lowerdir+…)/fsmount/move_mount,
                    else one mount(2) with relative paths (≤ a page)
      (root + --uidmap: the parent built it detached; the init attaches it)
    → pivot_root → make the working directory → the command
  the run ends: the area moved to trash and removed (under gc.lock shared)

shipyard rmi / image prune            (shipyard-cli/image.rs: collect)
  gc.lock EXCLUSIVE (waits up to 15 s for pulls and preparing runs)
    areas: live (locked) ones list layers to keep; stale ones go to trash
    Index: drop the references; blobs and layers nothing needs → .trash-…
  gc.lock SHARED: delete the trash (runs and pulls carry on meanwhile)
```

**The store** (`/var/lib/shipyard/images` for root, `$XDG_DATA_HOME/shipyard/images` or
`~/.local/share/shipyard/images` otherwise, or `SHIPYARD_IMAGE_STORE`):

```
gc.lock                    shared: pulls, extractions, runs being prepared; exclusive: GC
index.json, index.lock     references → manifest, config, layers
blobs/sha256/<hex>         read-only, named by digest; .<hex>.partial while downloading
layers/<hex>[-caller|-map-<id>]/diff
                           one per layer and owner kind; .tmp-… while extracting, .trash-… going
containers/run-…/          upper, work, merged; flock-held for the run's life
volumes/<name>/_data       volumes/.locks/<name>, volumes/.trash/<unique>
```

## Decisions and why

**Made with the user, before the fixes:**

- **Rootless extraction runs in the user's user namespace, as Podman does.** A layer's files
  belong to its users (`root`, `postgres`, `nobody`). A normal user can only make files they
  own; root *inside a user namespace* with the caller's subordinate ranges can make files
  owned by any of them. So the CLI re-executes itself as the storage helper
  (`__shipyard-storage`), in a new user namespace mapped with `newuidmap`/`newgidmap`
  (stage 4), and passes it the store as an open descriptor. Removing those files needs the
  same namespace, so `rmi`, `prune`, volume removal and the run's own clean-up go through it
  too.
- **Credentials are read-only, from Docker's config.** `docker login` already writes
  `~/.docker/config.json` and the helpers; Shipyard reads `auths`, `credsStore`,
  `credHelpers` and identity tokens in Docker's order. `shipyard login` comes later.
- **Downloads resume across pulls.** A blob downloads into a locked `.partial` file that
  stays in the store, as containerd's ingest does. The next pull goes on with a `Range`
  request.
- **`volume prune` asks first** on a terminal, and needs `-f` without one. **`rmi a b c`
  goes on past a failure** and exits 125 if any failed.
- **No fuse-overlayfs, for now.** Run inside the container, the daemon would live in the
  container's PID namespace, and container root could reach its descriptors to the store and
  walk out of it (`/proc/<pid>/fd/N/..`) to the user's files. It comes later as Podman
  runs it: one helper per container, outside the container's PID namespace. Until then, a
  host that refuses a rootless overlay gets a message that names the cause and the fix.
- **Root with `--uidmap` gets shifted layer copies.** Without idmapped mounts, a layer's
  files are owned by host IDs; a container whose root is host ID 100000 would see its own
  `/etc` as `nobody`'s. So the layers are extracted again with every owner shifted by the
  map (`layers/<hex>-map-<id>`, one copy per distinct map), and **the parent, real root,
  builds the overlay**: a mount made inside the container's user namespace could only use
  `userxattr`, which those layers, made by root, don't carry.

**Engineering:**

- **Two crates, split by platform and trust.** `shipyard-image` (references, the registry
  client, credentials, manifests, the store, extraction) is portable, so most of the
  stage is tested and fuzzed on macOS too. `shipyard-storage` holds the volumes. The
  overlay mount stays in `shipyard-runtime`, where the container's init runs. There's no
  engine crate yet: the CLI wires them together until stage 9's state gives an engine its
  real boundary.
- **Blocking I/O, no async runtime.** A pull is a handful of large downloads; scoped
  threads over blocking `ureq` run the layers in parallel, and are joined before anything
  is extracted. `tokio` waits for the API (stage 11). New dependencies: `ureq` with
  `rustls` (its `ring` provider is the one C dependency, allowed in `deny.toml`), `sha2`,
  `flate2` (pure Rust), `base64`. The `tar` crate only builds test archives; layers are
  read by Shipyard's own reader.
- **Every path in a layer is reached by directory descriptor.** `unpack.rs` walks to an
  entry's parent with `openat(O_NOFOLLOW|O_DIRECTORY)`, one component at a time (caching the
  descriptors), and creates the entry with an `*at` call relative to that descriptor:
  `openat(O_CREAT|O_EXCL|O_NOFOLLOW)`, `mkdirat`, `symlinkat`, `mknodat`. A symlink in the
  layer is never followed, wherever it is, so it can't lead out. A hard link's source must be
  a regular file this same layer wrote (`linkat` with no flags). Whiteouts are only recorded
  while the layer is read, and made at the end, by descriptor. This is the fix for the first
  version's three escapes (see the [Failure-mode register](#failure-mode-register)).
- **Our own tar reader** (`tar_reader.rs`). It holds nothing an archive can inflate
  unbounded: extension headers (PAX, GNU long names) are capped at 1 MiB together per entry,
  headers at a million, paths at 4096 bytes and 256 components. Sparse files and data on an
  entry type that has none are refused, never guessed at.
- **One pass verifies both digests.** The compressed stream is hashed as it's read (the
  blob's digest, again, against a corrupt store) and the decompressed stream too (the
  config's `diff_id`), with a byte budget between them. A mismatch fails the layer, which is
  then discarded.
- **Ownership is applied as the image says.** As root (or root in the helper's namespace):
  `fchownat`, then the mode (a `chown` clears setuid), times, `security.capability`;
  directories' modes and times last, deepest first. Without the right to chown
  (`Ownership::Keep`), setuid and setgid bits are dropped: a setuid file owned by the caller
  means nothing good.
- **Whiteouts are 0:0 character devices, opaque directories an xattr.** That's what overlayfs
  reads. `user.overlay.opaque` always (a rootless mount reads it with `userxattr`),
  `trusted.overlay.opaque` too as real root. If the xattr can't be set, extraction fails:
  there's no `.wh.` file fallback, which would make a wrong filesystem quietly.
- **The store is one lock and a few renames.** `gc.lock` is shared by everything that adds or
  depends (pulls, extraction, preparing a run) and exclusive for collection, which then
  shares it again for the slow deletes. A layer exists when its directory's rename lands; a
  removal renames to `.trash-…` first; a run's area exists when it's locked and renamed.
  Liveness is always a lock (`flock(2)`, which the kernel drops however the holder dies),
  never a PID, which the system reuses. A collection waits 15 seconds for the lock, and a run
  or pull waits 60.
- **The index is written whole.** `index.lock` around read-modify-write, a temporary file,
  `fsync`, `rename`, then an `fsync` of the directory. A corrupt index is moved aside with a
  warning, and stops collection (it would otherwise think nothing is in use).
- **Pulls check what they're told.** A pinned digest must match the bytes. An index entry's
  manifest must be what it named. Every blob is bounded to its descriptor's size. A token
  realm must be HTTPS. Credentials go only to the registry and its token realm, never to
  where a redirect led. Media types are allow-listed, and the JSON body decides what a
  document is, not the `Content-Type` header.
- **Pulls survive a bad network.** Retries honour `Retry-After`. An attempt that made
  progress doesn't count against the limit. A `206` answer's `Content-Range` must start
  where asked, or the download starts over. A body waits at most five minutes while ranges
  work, and each blob has an overall time budget.
- **No threads when the container starts.** ureq's DNS timeout spawns a resolver thread that
  is never joined; with no resolve timeout set, it spawns none. The CLI forks and clones
  single-threaded, as the "code after fork" rules need.
- **Runs check before they pull.** `RunArgs::check` validates every flag, the process spec
  and the volume binds, with no side effects. Only then does the image come, and
  `Planned::finish` merges its defaults.
- **The overlay is mounted by the init, inside the container's namespaces.** An unprivileged
  overlay mount is only allowed inside a user namespace. The new mount API comes first
  (`fsopen`, then one `fsconfig(lowerdir+)` per layer, Linux 6.8+), with each path passed
  as `/proc/self/fd/N`, because `fsconfig` caps a string at 256 bytes. Any `EINVAL` there
  falls back to one `mount(2)`, after a `chdir` to the layers' shared parent so the paths
  are short and relative; past a page of options it refuses with the layer count, rather
  than let the kernel truncate them.
- **The working directory is made, as Docker does.** After `pivot_root` (so a path can
  only lead within the new root), before a read-only remount, with umask 0 (mode 0755).
  If it can't be made, the change into it fails and says so.

## Kernel checks

`scripts/probes/overlayfs.c`, run in the test container (privileged, `/tmp` a tmpfs), on
Linux 7.0 (OrbStack's VM):

| Question | Result |
|---|---|
| Overlay mounts as root | ok; a 0:0 whiteout device hides the lower file |
| Overlay mounts in a user namespace (Linux 5.11+), with and without `userxattr` | ok, both; the merged tree sees the lower files; a write copies up |
| `metacopy` and `redirect_dir` known | present |
| `fsopen("overlay")`; `fsconfig(lowerdir+)` (Linux 6.8) | ok, ok |
| 100 layers through the new API | ok |
| 100 layers in one `mount(2)` (10592 bytes of options) | `ENOENT`: the kernel cuts the options at a page and then can't find the cut path. Hence the explicit page check, which names the real cause |
| `fuse-overlayfs` on `PATH` | no (not used: see Decisions) |

Two more facts, from probing during the fixes:

- **An unprivileged user may make a 0:0 character device** (a whiteout) since Linux 5.8, on
  tmpfs and btrfs, but **overlayfs refuses** it in its upper layer. So the test container's
  `/tmp` is a tmpfs, not the container's own overlay root.
- **An overlay's upper layer can't be on overlayfs**, so a store under a container's root
  filesystem fails with `EINVAL`. The hint names `SHIPYARD_IMAGE_STORE`
  (`a_store_on_overlayfs_is_explained`).

## macOS and Linux differences

- `shipyard-image` is `cfg(unix)`. References, manifests, the registry client, credentials,
  the store, the tar reader and the index build and are tested on both. Extraction runs on
  macOS in tests (it's descriptor-based POSIX), but whiteout devices, opaque xattrs and
  capabilities are Linux-only.
- `run --image`, `--volume`, the storage helper and the overlay are Linux-only.
  On macOS they're refused by name ("--volume needs Linux"); the VM (stage 12) runs them.
- Cross-checking Linux code from macOS with `cargo clippy --target aarch64-unknown-linux-gnu`
  needs a Linux C cross-compiler for `ring`'s C code. Use the container instead
  ([DEVELOPMENT.md](../DEVELOPMENT.md)).

## Failure-mode register

What the first version got wrong, and what now stops it:

| Case | Handling | Test |
|---|---|---|
| A symlink `etc → /etc`, then `etc/passwd`: a write through it | descriptor walk with `O_NOFOLLOW`; a symlink where a directory must be is an error | `nothing_is_written_through_a_symlink` |
| A symlink, then `x/.wh.passwd` or an opaque marker under it: a host file deleted | whiteouts recorded, made at the end, by descriptor; never through a symlink | `a_whiteout_or_opaque_marker_never_reaches_through_a_symlink` |
| A hard link to `h/.ssh/id_ed25519` through a symlink, or to `../..` | a link's source must be a regular file this layer made | `a_hard_link_can_only_reach_what_the_layer_made` |
| `..` and absolute names | cleaned to the layer; climbing past its root is refused | `climbing_paths_are_refused_and_absolute_ones_kept_inside` |
| A registry serving other bytes for `image@sha256:…` | the pin is checked against the bytes | `a_pinned_digest_is_checked_against_the_bytes` |
| Two runs extracting the same new layer at once | each extracts into its own `.tmp-…`; the first rename wins, the second copy is discarded, never the first | `a_layer_placed_by_another_process_is_kept`, `two_first_runs_of_an_image_at_once_share_its_layer` |
| `rmi`/`prune` while a container runs, or during a pull | live areas' layers are kept; GC waits for the shared lock | `layers_running_containers_use_are_kept`, `an_image_in_use_is_kept_until_its_container_ends`, `a_prune_during_a_pull_waits_for_it` |
| A reused PID inheriting a previous run's writable layer | areas get unique names, are locked for their life, removed at the end, swept when stale | `each_run_has_its_own_writable_layer_and_removes_it`, `a_killed_runs_writable_layer_is_swept_by_the_next` |
| Lost owners, modes and capabilities (`/tmp` 0755, `ping` broken, setuid kept on root-owned files) | applied as the image says; setuid dropped where owners can't be | `as_root_owners_whiteouts_opaque_markers_and_capabilities_are_made`, `setuid_is_dropped_when_owners_are_not_applied` |
| `"Entrypoint": null` (hello-world) | `null` lists parse as empty | `docker_writes_unset_lists_as_null_and_strings_as_empty` |
| A 60-second total timeout on a layer download | per-attempt body deadlines, progress-aware retries, `Range` resume | `a_cut_transfer_goes_on_from_where_it_stopped`, `a_transfer_that_keeps_moving_isnt_counted_as_failing` |
| An image's `Env: ["GITHUB_TOKEN"]` copying the caller's token | an image entry without `=` is skipped with a warning, never looked up on the host | `an_images_env_never_reads_the_host` |
| A blob longer than its size; a 4 GiB PAX header | blobs bounded to their size; extension headers to 1 MiB | `a_blob_longer_than_its_size_is_refused_at_its_size`, `an_oversized_extension_is_refused_before_it_is_read` |
| A whiteout of a file the same layer adds | dropped: the layer's own file stays | `whiteouts_of_the_layers_own_files_are_dropped` |
| A layer carrying `trusted.overlay.*` xattrs | refused: only Shipyard sets overlay's attributes | `overlay_attributes_in_a_layer_are_refused` |
| A corrupt `index.json` | moved aside with a warning; collection stops rather than delete everything | `a_corrupt_index_is_moved_aside_and_stops_collection` |
| A volume removed while a container uses it | a shared `flock` held for the run's life; `rm` refuses, `prune` skips | `a_volume_in_use_is_never_removed` |
| A credential helper that hangs or floods | `poll(2)` timeout, output cap; killed as a process group and reaped | `a_helper_that_hangs_or_floods_is_stopped` |
| Credentials sent where a redirect led | only the registry and its token realm get them | `a_challenge_from_where_a_redirect_led_gets_no_credentials` |
| A loopback registry naming a plain-HTTP token realm elsewhere (found while writing the book) | a plain-HTTP realm must be on this machine too | `a_plain_http_realm_must_be_on_this_machine` |
| More than a page of layers in one `mount(2)` | the new API (no limit to 500); otherwise relative paths, then a refusal naming the layer count | `an_image_of_sixty_layers_runs`, `legacy_options_are_relative_and_bounded` |
| A `,` or `:` in the store path | refused before the mount, naming `SHIPYARD_IMAGE_STORE` | `an_overlay_is_checked_before_it_reaches_the_kernel` |
| A refused overlay mount | the errno explained: no overlay module, the store's filesystem, the kernel version, AppArmor's restriction | `an_overlay_refusal_says_why` |
| A missing program in an image (exit 127 hints) | looked up through the layers, upper first, whiteouts hiding | `an_image_programs_hints_look_through_its_layers` |
| A typo'd flag after a long download | flags checked before the pull | `a_bad_flag_costs_no_pull` |

**Fuzzing.** Five cargo-fuzz targets (`fuzz/`, nightly, its own workspace) cover what comes
from outside: `manifest`, `challenge`, `reference`, `tar`, and `unpack`, which extracts each
input beside a canary directory and fails if anything outside the layer changes. Their seeds
(`scripts/fuzz-seeds.py`) include the escapes above. `scripts/test-linux.sh --fuzz` runs
each for five minutes as a normal user. In the step-6 run (2026-10-08, Linux 7.0, aarch64)
the targets went through about 64 million inputs with no crash, after one fix: the
`reference` target found, within seconds, that the 255-byte name limit was checked before
Hub's `library/` was added, so a 248-byte name was accepted and its canonical form then
refused (`the_name_length_limit_counts_library`). A bug planted for the purpose (the
walk following symlinks) was caught by the `unpack` target at once.

## Known limitations (by design, for stage 6)

- **zstd and sparse layers are refused** by name; foreign (non-distributable) layers too.
- **No fuse-overlayfs:** a host that won't let a normal user mount overlay (Linux before
  5.11, AppArmor's `kernel.apparmor_restrict_unprivileged_userns`) runs images as root only.
- **`--uidmap` with an image needs Linux 6.8** (the parent-built overlay uses `lowerdir+`),
  and costs a full copy of the image's layers per distinct map.
- **A rootless store on tmpfs needs Linux 6.6** (user xattrs on tmpfs).
- **A blob's directory entry isn't fsynced**: a blob lost to a crash is downloaded again. A
  layer's data and renames are synced (`syncfs`, then an `fsync` of `layers/`).
- **No insecure registries or mirrors:** plain HTTP is for loopback only.
- **An index isn't recorded.** A multi-platform image is recorded by its platform's
  manifest; the index blob is stored, but `image prune` collects it, after which a run
  pinned to the index digest (`alpine@sha256:<index>`) pulls again. `shipyard pull`
  prints the platform manifest's digest, where Docker prints the index's.

## Carried forward

- **Stage 9:** `tag`, `inspect`, `history`, `save`/`load`, `import`/`export`; `--mount`
  (tmpfs, propagation); named containers' areas instead of per-run ones.
- **Later:**
  - fuse-overlayfs as a per-container helper outside the PID namespace;
  - zstd;
  - `shipyard login`;
  - idmapped mounts instead of shifted copies;
  - `push` and signatures.
- **Stage 14 (hardening):** run the fuzz targets in CI on a schedule.

## Tests

- **Unit tests** in every module of `shipyard-image` (digest, reference, manifest, registry,
  credentials, store, index, images, containers, tar reader, unpack, fsat), plus
  `shipyard-storage`'s volumes and the runtime's overlay pieces (`rootfs.rs`, `failure.rs`,
  `isolation.rs`). The unpack tests put a canary outside the layer for every escape. A
  property test checks the parsers never panic.
- **The fixture registry** (`shipyard-image/tests/pull.rs`): an in-process HTTP server with
  routes for tokens, Basic and Bearer challenges, redirects, ranges, cut transfers, rate
  limits and lies. 32 tests, on macOS and Linux.
- **End to end** (`shipyard-cli/tests/image.rs`, Linux): the real binary against the fixture
  registry, with an image built from the host's programs. 17 run as a normal user (with
  `SHIPYARD_TEST_ROOTLESS=required` in CI and `test-linux.sh`, a skip is a failure), and 19
  as root (`--uidmap`, and a store on overlayfs, are root's).
- **Mutation checks** on each step's critical lines: the descriptor walk, hard-link sources,
  whiteout handling, the digest checks, the GC's roots, the lock modes, the pin, the
  `Content-Range` check, credential precedence, the entrypoint rules and the overlay's layer
  order. Each mutant was caught.

## Review history

The stage was first built in one pass. A review of that version (four reviewers) found:

- four ways for an image to reach host files (escapes 1–3 and the unchecked pin);
- broken image semantics (owners, modes, `null` configs);
- no locking anywhere in the store;
- every run going to the network.

It was fixed in steps, each one planned, built, then reviewed twice (once for clean code,
once for what could go wrong), mutation-checked and run by hand on Linux:

| Step | What | A finding of its own reviews |
|---|---|---|
| 0 | the ROADMAP's premature "done" reverted; drafts marked | — |
| 1 | extraction by descriptor; own tar reader; owners and whiteouts; the rootless storage helper | (applied in the step; see the register) |
| 2 | `gc.lock`, the index's locking and durability, per-run areas | a storage helper orphaned by a killed CLI went on extracting with no lock (it now inherits `gc.lock`) |
| 3a | pin, sizes, retries, HTTPS realms, offline runs, `shipyard pull`, `--pull` | (applied in the step; see the register) |
| 3b | credentials; resumable downloads | a wrong blob committable in a partial race; credentials sent to redirect targets |
| 4 | `run` checked before the pull, Docker's rules, volumes in use, `system df` | a lock file named like trash, letting prune delete an in-use volume |
| 5 | the overlay: new mount API, fallback, hints; `--uidmap` with images | 5.2–6.4 kernels that never fell back; a `/tmp` bind source hidden by staging |
| 6 | the gap tests, fuzzing, these notes | the image end-to-end tests never ran as a normal user (now `required`); also, from the final runs: a plain-HTTP token realm off this machine allowed for a loopback registry, and (stage 5) a device program refused with `EAGAIN` when a signal arrived during verification, now retried as libbpf does |

# AGENTS.md

The operating manual for any coding agent working on Shipyard (Claude Code, Codex,
Cursor, Copilot, Gemini…). It holds the binding rules, the map, the workflow and the
lessons that cost real time to learn. Read it whole before your first change; come back
to the section you need before each step.

Where this file and the user disagree, **the user wins**. Where it and the code
disagree, **the code is the fact**: fix this file in the same change.

---

## 1. The project in one minute

Shipyard is a lightweight, **daemonless** container engine in Rust, an alternative to
Docker / Podman / OrbStack, for **macOS developers and Linux hosts** (the Linux host's
needs first; development and testing happen on macOS). Two goals carry equal weight:

1. **Production-grade software.**
2. **Learning systems programming**, built step by step with *The Linux Programming
   Interface* (TLPI) as the reference. Explaining the concept behind a change is part of
   the change.

When they conflict, **the product decides *what* gets built and learning decides *how***
(ROADMAP D19): the next stage follows the users' needs, and every concept is still
explained.

Work follows the staged plan in [`docs/ROADMAP.md`](docs/ROADMAP.md). Each stage builds on
the previous one. **Don't jump ahead or build a later stage early**; record what belongs
later as carried forward.

**Decisions already made (don't relitigate):**

- **Daemonless means nothing runs while Shipyard isn't being used** (D20): never a root daemon; one tiny monitor process per container; anything else (the API socket, the macOS VM) starts on first use and stops when idle.
- **macOS developers and Linux hosts, UNIX-only** (D18). The CLI, engine and UI run natively on macOS and Linux. Kernel isolation is Linux-only; on macOS it runs inside one Shipyard-managed Linux VM (Virtualization.framework through `objc2`, an Alpine guest running `shipyard`), Apple Silicon only (D29).
- **Docker-compatible, not Docker-reimplemented:** the API is a subset of Docker's Engine API (D25), the CLI a drop-in for everyday `docker` commands (D26), compose is Docker's own `docker compose` against that API (D27), images are built by BuildKit (D28).
- **The owned surface is frozen** (D19): no new hand-written protocol or format unless the static binary depends on it; use a maintained crate or tool instead.
- **OCI runtime:** `shipyard-runtime` implements the OCI runtime spec; the engine calls it like Podman calls runc.
- **Syscalls:** `std` first, then `nix`/`rustix`, and raw `libc` only when neither covers the call.
- **No release and no outside users until v1.0** (D21); v0.3–v0.5 are internal milestones.
- The rest are recorded in the ROADMAP's Decisions (D-numbers) and each stage's notes.

**Where it stands:** v0.2.0 is tagged (stage 9: containers by name, detached, restart
policies, health checks, events). The direction was reset on 2026-10-15 (ROADMAP
D18–D31). Stage 10, **foundations and the Mac path**, is done
([notes](docs/stages/10-foundations.md)):
- the refactor with no behaviour change (10.1, D23: one `take`, the `shipyard-sys` crate,
  `doctor` into the engine, the CLI split; checked against v0.2.0's binary);
- the F-rows homed in it (10.2);
- a minimal macOS path used daily (10.3, D24): every command forwarded to Shipyard's own
  Linux VM, terminals, the home shared, ports on the Mac's `localhost`, an idle stop,
  `doctor` checking the VM and its Linux.

Stage 11, **Docker compatibility**, is done ([notes](docs/stages/11-docker-compat.md)):
Docker's Engine API (a subset) served by `shipyard system service`, socket-activated and
gone when idle; the CLI a drop-in for everyday `docker` commands; `docker compose` and
Testcontainers against it, root and rootless; Docker's clients on the Mac through
`docker.sock` in Shipyard's VM (`shipyard vm install` for launchd). v0.3 is reached, not
tagged (D21).

Stage 12, **build and images**, is done ([notes](docs/stages/12-build-images.md)):
`build` through BuildKit in a container per build (a user namespace of its own, its
cache per context), `push` and `login`/`logout` (Docker's config; the Mac hands its
credential to its VM), `save`/`load`/`history`, `import`/`export`, on Linux and the Mac.
v0.4 is reached, not tagged (D21).

Stage 13, **macOS production**, is done ([notes](docs/stages/13-macos.md)): the VM
sized from the Mac (`vm set`), its clock kept after a sleep, TRIM at each stop, a hung or
rebooted guest stopped; amd64 images through Rosetta (`--platform`); the home, `/Volumes`,
`/tmp` and `/var/folders` shared, a bind outside them refused; UDP ports, each port dialled
at its container, the guest's own address answering none, its agent's port held from boot;
the forwarded command's signals on a connection of their own; `vm stop` over the host's
own socket, a start waiting by the host's phase, a login start for restart policies (opt-in);
a stable local signing identity and the macOS 13 floor. v0.5 is reached, not tagged (D21).

Stage 9's and stage 13's book chapters were skipped (the user's choice). Next: 14 the desktop UI, 15
hardening and v1.0 (defined in D30). Open findings with severity and home stage: the ROADMAP's
[Open findings](docs/ROADMAP.md#open-findings). CI is off (D31: GitHub
billing) until before v1.0, so **local verification is the check of record**
(section 7).

---

## 2. Hard rules (never break these)

| Rule | Why |
|---|---|
| **Never stage, commit, push, merge or tag unless the user asks** in this conversation. | The user owns history. |
| **No AI attribution, ever:** no `Co-Authored-By: <AI>` trailer, no "Generated with …" line, no AI as author or contributor, in commits, PRs, tags or releases. This overrides any tool's default attribution. | Commits are authored solely by the repository owner. |
| Use [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `docs:`, `chore:`…). | History reads as a changelog. |
| **`mv`, never `git mv`.** | It stages. |
| **Confirm before anything destructive or outward-facing:** deleting files or Docker volumes, changing global config, pushing, opening PRs, anything outside the repo. Docker cleanup touches only `shipyard-*` volumes and `shipyard-test:*` images. | Approval for one action isn't approval for the next. |
| **Never write to a real block device.** In the privileged test container `/dev/loop0` (7:0) is the VM's own disk. Open devices read-only at most (`head -c 0`), or `losetup` a fresh file. | A write once landed on the VM's live btrfs. |
| **Wrap every `docker run` in `timeout`.** | The Docker daemon once hung for ~40 minutes and stalled every agent. |
| **Never send a `limactl shell` command you haven't read line by line.** Write VM setup as a script, read it whole, then run it. | A stray `install … /dev/null` replaced a VM's `/dev/null`, twice. |
| **Docs are part of done** (section 8). | Users read `docs/user/`, not the code. |
| **Stay in the current stage.** Something that belongs later is recorded, not built. | The staged plan is the learning path. |
| **`docs/book/` is the user's and is gitignored:** no git copy exists. Copy a chapter to scratch before any scripted edit. | ~900 lines were once lost to a bad slice. |

---

## 3. Map

### Crates (`crates/`)

| Crate | What it is | May depend on |
|---|---|---|
| `shipyard-runtime` | Low-level primitives (processes, namespaces, mounts, cgroups, terminals; namespaces a new container joins instead of making, `process/joins.rs`: another's network and IPC by its init's PID and start time, entered around the clone and left, or, its user namespace joined too, by a forked process that never comes back, `process/enterer.rs`; another's `/dev/shm`, cloned by a helper of the setup that enters its mount namespace, `rootfs.rs`'s `clone_their_shm`: a mount can't be bound from a namespace the binder isn't in) and the OCI runtime's containers (`container/`: the per-container monitor, logs, attach, `exec`'s joiner, `cp`'s helper as internal processes); a helper whose root is a running container's root mount alone (`start_helper_alone`: `open_tree` without `AT_RECURSIVE`, made in `enter()` before its capabilities go; `HelperRoot` says if the kernel refused; a helper's `note` said back with its success, `wait_noting`: `export`, step 12.2b); the attach protocol and its client (`attach/client.rs`: a session, `settle`, `to_runtime`; `Welcomed::into_parts` for a caller that runs its own session over another stream), the logs' records and their reader (`logs/reader.rs`: tail, since, follow across rotations, `of_runtime`; its `Gone` hook a closure, so each reader asks after its own client); Docker's resource limits as cgroup limits (`resources::DockerLimits`, the CLI's flags and the API's create alike). | network |
| `shipyard-image` | Images: registry client, content-addressed store, safe layer unpacking; image archives (`archive/`, step 12.2a: `save`'s Docker 25 form, an OCI layout with `manifest.json`; `load`'s reader of that, BuildKit's OCI layout and Docker's legacy archive (`legacy.rs` makes it a manifest), hostile: members matched by name, never extracted, staged (`archive/staging.rs`, shared with `import`) in `blobs/.load-<pid>-<n>-<nanos>/` under a `flock` of its own (made as `….new` and renamed once locked), swept when its lock is free by the next load and the store's collection, bounded (`MAX_DOCUMENTS`, `MAX_ENTRIES`, `MAX_NAMES`, `MAX_LAYERS` at the parse), a cut archive told by its stream's end (`EndSeen`), what's left out a `LeftOut`); an image's `history`, read leniently (`history.rs`); `from_store_by_digest`, an image by its blobs alone; `import` (step 12.2b: a root filesystem's tar made a one-layer image in one pass, its `diff_id` hashed, gzipped into staging through `StageWriter`, read as a tar to its end marker) and `changes` (`--change`'s nine instructions, bounded); a push (step 12.4: `push.rs`, each blob opened under the store's lock, then sent with none held, by offset, each distinct blob once, the manifest last, refused if the tag names the index the image came through; `registry/upload.rs`: `HEAD`, `POST`/`PATCH`/`PUT ?digest=` as Docker's CLI, no redirect followed, each step to the last `Location` and only on the registry itself, a 401 or 500 starting the upload over, a lost commit found by `HEAD`; a push asks every token for `pull,push`: `Registry::for_push`; a 401 after a service's token is `Denied`, after a password `Refused`, `with_auth` for every method); sign-ins kept and removed where `docker login` keeps them (`credentials/write.rs`: `Credentials::keep`/`forget`, the helper's `store`/`erase`, else `auths`, every other key, the mode, owner and link kept). | — |
| `shipyard-network` | Shipyard's own netlink (rtnetlink, nf_tables, conntrack), networks and leases (a network's ID, labels and options in its `labels.json`, v1, step 11.2c2b), pasta, the DNS server, reading a container's sockets. | — |
| `shipyard-storage` | Named volumes (`volumes/<name>/_data`; Docker's API's labels in `labels.json` beside it, v1, step 11.2c2a; its making's time in `created.json`, v1, step 12.5a (F146: the directory's own time moves), the directory's for a volume made before; a copy-up's lock `.fill-locks/<name>` and its `filling` marker beside `_data`, which an unfinished fill leaves for the next to finish, step 11.4a2b). | — |
| `shipyard-sys` | The plumbing every crate shares (D10.1-4): state files written whole or not at all (`fs::write_atomically{,_at}`: `O_EXCL` temporary, `fsync`, `renameat`, the directory synced) and read with a bound (`fs::read_bounded{,_at,_from}`: no symlink, no FIFO wait, a regular file, a cap), lock files opened safely; locks waited for to a deadline (`lock::lock_until`: scoped, a nix `Flock`; `lock::try_lock_until` + `lock::HeldFile`: handed over to a monitor, given up, never unlocked by the giver); processes signalled through pidfds (`pidfd`, Linux; `pidfd::unsupported` says when to fall back to a checked PID: `ENOSYS`, a policy's `EPERM`, never `EMFILE`), descriptors passed over Unix sockets (`fdpass`), taken over across `exec` (`fd::take_inherited`); the bytes between two sockets, both ways (`relay::both_ways`: half-closes, a linger counted from the last byte, 10 minutes at most); a versioned JSON document read, a newer one refused (`versioned::read`, F107); directories walked and made below a descriptor without following a symlink (`fs::open_below`, `fs::make_below`: a bind source Shipyard made, step 11.7); a relay's end said (`relay::Ended`: its first side's peer gone, `peer_gone`'s zero-length send); a process's `/proc/PID/stat` (`proc::Stat`: parsed from the name's last `)`, `gone` for `Z` and `X`; `stat`, `stat_at` under a `/proc/PID` descriptor); a program on `PATH` (`path::find_program`: absolute directories only, an execute bit); bytes as hex (`hex::lower`); the time in seconds (`time::unix_seconds`, `unix_now`); systemd's socket activation, from the service's side (`listen::from_systemd`: descriptor 3 if `LISTEN_PID` is this process, one socket only, the variables cleared; `listen::passed`, the runtime's `--preserve-fds` reading too); a directory made and checked the caller's alone (`fs::private_dir`, `fs::unsafe_dir`: the runtime's state root, the API's socket directory); the mount a descriptor is on (`fs::mount_id`, `statx`'s `STX_MNT_ID`: `export`'s walk, where `st_dev` can't tell). Use it, never a copy of its own (a few `Flock`s in `image`, `network` and `events` still are: F107). | — (any crate may depend on it) |
| `shipyard-vm` | The macOS VM (step 10.3, D29): the guest's files and manifest (`Guest`, built by `scripts/guest/build.sh`), the VM through Virtualization.framework (`machine.rs`, `objc2-virtualization`: every call on its serial queue; Rosetta's share when the Mac has it, `rosetta()`, told to the guest by `forward::ROSETTA_WORD`, which registers it with `binfmt_misc` at boot: `guest/init.rs`'s `rosetta`), the `__shipyard-vm` host process (`host.rs`: the lock, the socket, each client joined to the guest's agent over vsock; `control.rs`, step 13.6b1: the stop signals blocked from its first line and taken on a thread of their own as a stop asked, `Stops`, which the main thread takes between phases (before the guest starts, once its agent answers, never cutting a disk check); its own `host.sock`, bound from its lock (then its PID written) to its end, answering `forward::HOST_PHASE_REQUEST` and `HOST_STOP_REQUEST` with its phase and what's left of its bound, `phase.rs`'s `PhaseAnswer`, by which `vm stop` and a start wait (`stop_wait`, `at_deadline`: never a signal to a PID, never a healthy host stopped); its own `host.log` kept short, `keep_log_small`), the guest's published ports on the Mac's loopback (`forwarding.rs`: the poll, `bind_alone`, the listeners, by protocol; `ports.rs`: `wanted`, `ports.json` v2; UDP's in `udp.rs`, step 13.5a: one stream to the guest a published address, `forward::FORWARD_UDP_REQUEST`, each Mac peer's datagrams tagged with an ID of its own, `Peers`; neither side ever waits on the stream, `forward::Outbox` and `Inbox`, 13.8c), the network card's address made once and kept (`network.rs`, `network.json`: one NAT lease, not one a boot), launchd (`launchd.rs`: `vm install`'s agent holding `docker.sock` (`launch_activate_socket`, the one FFI call), its label per VM directory; launchd runs it once at load only to make the socket, so the host exits then unless a connection waits, the CLI left its `start-asked` mark, or (`--at-login`) containers would come back; once loaded, the CLI starts the host by `launchctl kickstart`; a host launchd started never removes its socket, and answers clients waiting when it can't serve), the host's sockets served one way (`serving.rs`: made the user's alone, the peer's uid checked, counted for the idle rule, each accepted connection set blocking (`accepted`, F147), one `Budget` of 1024 open connections shared with the forwarded ports, which may take 768 of it, a refusal on its own thread), the login start (`comes_back.rs`, step 13.6b2: `comes-back.json` v1, how many containers the guest's v3 ports answer says would come back, kept across stops and written once more at a stop; `vm install --at-login` sets `SHIPYARD_VM_AT_LOGIN` and `RunAtLoad`, and the host's run at load boots the VM only while that count is above 0, never for `vm install`'s own run, its `just-installed` mark), Docker's API on the Mac (`api.rs`: `docker.sock`, each connection relayed raw to the guest's agent as `forward::API_REQUEST` through `forwarding::joined` (an older guest's command welcome refused), lingering by side (`API_LINGERS`: unlimited after the client's end, a minute after the service's); its own refusals Docker's 503s), the idle stop (`idle.rs`), the VM's size (`settings.rs`: `vm.json` v1, `vm set`'s; the default a quarter of the Mac's RAM, `resolve`), whether the guest is still the one booted (`liveness.rs`: its boot ID asked every 10 s, `forward::BOOT_REQUEST`; another ID or six misses stop the VM: F90), the Mac's `sysctlbyname` (`sysctl.rs`), `doctor`'s Mac checks (`doctor.rs`: the disk's size and the Mac's free space too), the VM directory (`paths.rs`: the guest found in it, else beside the binary's own directory, `guest_dir`; `peer.rs`), its small state files (`state_file.rs`: versioned, bounded, written whole), the tools it runs (`child.rs`: `output_within`, `launchctl`, `tmutil` and `codesign` killed past a bound), the places it shares (`shares.rs`, step 13.4: the home, `/Volumes`, `/private/tmp`, `/private/var/folders`, a virtiofs device each, the guest told on its command line by `forward::SHARES_KEY`), and the CLI's side (`client.rs`: start, status, stop; `client/`: the host started (`host.rs`), an internal request asked (`ask.rs`), a command forwarded and tried (`command.rs`), its session and signals (`session.rs`)). macOS-only but for its places, manifest, ports, idle rule and checks. The forwarded command's wire (`HelloRun`, `Request`, `Connection`) is the runtime's `attach::forward`; its refusals are a `forward::Refusal` (stopping, not ready, another build's (`Newer`, F167), other words), sent as `Frame::Declined`'s code from protocol version 5 (F99) and read from an older peer's words only in `Refusal::from_words`. From version 6 (step 13.6a, F94) a user's command's welcome is followed by `Frame::Session`, a random token (0: none) with which the Mac opens the command's control connection (`forward::CONTROL_REQUEST`, `client/session.rs`'s `open_control`): its signals, resizes and hang-up there, never stuck behind its input (`attach::client`'s `Welcomed::with_control`); `forward::is_internal` (every `__guest-…` ask) gets none. An API connection's welcome carries one too, which names it to `API_HANG_UP` (F155). What a host booted is in `booted.json` (v1, `booted.rs`: its PID, the guest's revision and sha256), read only while that host runs: `vm status`, `doctor` and each command say when it's another build's (F173b), or a host from before 0.5 (no `host.sock`, `OLDER_HOST`); `launchd::installed_by_older`, an agent whose `ExitTimeOut` is an older build's. A command naming a place shared beyond the home is `Request::beyond_home` (version 4, `BEYOND_HOME_VERSION`): a guest that shares only the home refuses it. A forwarded `push` or `pull` carries the Mac's credential in `Request::registry_auth` (version 3, written only then; the guest's agent hands it to the command on a pipe at descriptor 3, `REGISTRY_AUTH_FD`, never in an environment: step 12.4). | runtime, sys |
| `shipyard-engine` | **The engine (H1):** what the CLI's commands do, behind typed APIs that return data. `run/` (runs: the request, its validation, plan and finish, and the image's preparation, each in a module of its own, as are a container's create, start, foreground run, `cp` view, removal and `take`; stop, restart policy, recovery, holds, connect), `containers/` (records, IDs, names, Shipyard's own `inspect` view; the store's writers, lookups, locks, starts' leftovers; the writers are the engine's own, reached by its tests through the doc-hidden `containers::testing`), `docker/` (Docker's Engine API shapes of Shipyard's objects: `ContainerInspect`, built from the same read as Shipyard's view (`container/`: its `Config` and `HostConfig`, `config.rs`; its `NetworkSettings`, `network.rs`); `ContainerSummary` and its status words; a container's `standing` for the API's 304s and waits; `EventMessage`; `ImageSummary` and `ImageInspect`; `parse_time` for Docker's times; `filters`, Docker's list filters from its JSON or `KEY=VALUE` pairs, the API's and the CLI's `-f` alike, and `list_containers_where`; `system`, Docker's `SystemVersion` and `Info` and the API's versions, `Build`; `inspect_any`, an object by name as the first kind that has it), `doctor/` (the host's checks, as a report the CLI prints; its probe helper; `GuestDoctor`, the macOS guest's checks), `guest/` (the macOS guest's PID 1 (it binds the agent's vsock port first and holds it for the guest's life, handing it to each agent it starts at `forward::AGENT_FD`, 3: step 13.5b, F108; each share mounted at its path, shallowest first, the VM's own places hidden, then `/tmp` and `/var/folders` bound over: the guest's own temporary files in `/run/tmp`), `guest/shares.rs` (what a bind may name in the guest: a source judged by the mount it lands on, a share's virtiofs, and never the VM's own places in any letter case; called by the CLI's run and create and the API's create, and again at each start for a source in a share; never by the engine's own creates, the builder's; at the bind, `Shared::bind_of` splits a source given in a share at its share, and the start binds it from there by descriptor, `Mount::bind_made`, never through a link: 13.8c), its vsock agent (a command, a published port, or the guest's Docker API, its service started on demand; a hello waited for 10 s at most; `guest/control.rs`: a command's control connection, by its token, its signals and resizes applied from a thread of their own, never once it has ended), its `__guest-ports` (at the version asked, `3` this build's, `2` without what comes back, else v1: an older host reads it) and `__guest-doctor`; `guest/forwarded.rs`: a forwarded port dialled at its container's own address, `target_of` its lease, and UDP's relay, a connected socket a Mac peer, the address looked up again every 2 s), `api_service` (`shipyard system service` started when nothing answers its socket: `shipyard compose` and the guest's agent; its output to `service.log` beside the socket, moved aside past 4 MiB), `entry.rs` (every entry point, in order), `image/` (and the storage helper; `image/archive.rs`: `save`, `check_saved`, `load` and its `Loaded::lines`, each image by its own digest, every name recorded at once, `Index::record_many`), `build/` (step 12.3: `build`, a `--rm`-less BuildKit container per build, `BUILDKIT_IMAGE` pinned and found by digest (pulled before signals are held off), as root IDs from 2100000000 (`ROOT_FIRST`: no user's subordinate range, checked against `/etc/subuid`/`subgid` each build), `SYS_ADMIN`, seccomp unconfined; its context checked readable to it whole, its places under `builder/` (rootless, in the store) or `<store>-builder/` (root, beside it: the builder's root can't enter the store), a lock per context's state, a killed build's builder swept by the next; the store's images its Dockerfile names given to BuildKit as named contexts over an OCI layout of hard-linked blobs (`build/local.rs`, unless `--pull`); its progress the builder's logs followed, its image through `image::load_named` (its archive's names never taken); as root, `-o type=local`'s directory made without following a link (`output_dir`); `build::cache`, the `Build Cache` row and the prune, and the sweep of a state unused for a week (`sweep_unused`, its lock's time the last use)), `docker::image_history`, `image/push.rs` (step 12.4: `push`, each name's blobs opened under the store's lock; `names_a_tag`: a parsed reference fills in `latest`), `image/signing.rs` (`login_and_keep`, `logout`), `image/given.rs` (Docker's credential form, `X-Registry-Auth`, both ways; `docker_credential_for`, what a Mac hands its forwarded push or pull; `GivenCredential::or_docker`, the credentials a pull or push signs in with); `image::{tags_of_repository, record_digests, warn_index_reset, is_host_platform}`; `image::platforms_here` (step 13.3: the host's platform, then amd64 where Rosetta is registered; every read of the store takes it, `shipyard_image::PlatformChoice`), `named_in`/`platform_here` (`--platform`'s one rule), shared by save, push, load, import and the API's pull, `image/import.rs` (`import`: named, D12.2-9), `cp::export_by_name` (`run::Wanted::OwnFiles`: a running container's root cloned alone, a stopped one's view with no volume; the `export` helper, `Packer::within_mount` where the clone is refused), `network/` (and the DNS server), volumes, logs, attach and logs by name (over the runtime's client and reader), `exec`, `cp` (its helpers, and `volume-fill`: a create's empty volumes filled from the image's files in a view of the container's root alone, the volume's directory handed over `O_PATH` as its standard input, one fill a volume at a time, an unfinished one marked and finished by the next: `cp/fill.rs`, `run::volumes::fill_volumes`), events, health checks. **It never prints:** it reports to the caller's `Reporter`, fails with an `Error` that has an `ErrorKind`, and reads the environment only in `Paths::from_env` (a helper's entry point aside: it's a process of its own, `tests/boundaries.rs` lists them). | all of the above |
| `shipyard-cli` | The `shipyard` and `shipyard-runtime` binaries (clap, anyhow, tracing). `shipyard` parses flags into the engine's requests, calls the engine and prints what it reports: `cli.rs` (the flags; Docker's `container` and `image` groups are the same commands, flattened at once by `Command::flattened`; `Command::failure_code`, Docker's exit codes: 125 for `run`, 1 for the rest), `main.rs` (the dispatch, the pipe checks, tracing), `output.rs` (tables, `printable`, prompts), `format/` (`--format`: Docker's Go templates through `gotmpl`, `.Label "KEY"` by a rewrite of the parsed tree, Docker's rows (`rows.rs` and `rows/`, a module per kind: containers, images, resources, stats, and Docker's size `units`) and tables (Go's tabwriter widths)), `reporter.rs` (`CliReporter`, the engine), a module per command group, `run/`, `forward.rs` (a Mac's commands sent to the VM: what stays, the paths, the variables, the terminal), `vm.rs` (`shipyard vm`), `about.rs` (`version` and `info`, Docker's text), `stats.rs` (`top`, `stats`), `prune.rs` (`container`/`image`/`system prune`), `signing.rs` (`push`, `login`, `logout`: the prompt with echo and `ISIG` off; `take_handed`, the credential a Mac's agent handed on descriptor 3, read first thing in `main`), `compose.rs` (`shipyard compose`: Docker's `docker compose` `exec`'d against the API's socket, the service started when nothing serves it), `building.rs` (`build`, `builder prune`, `-o`'s forms), `build.rs` (the revision compiled in). `lib.rs` holds what both binaries share (`limits`, `logs_output`, `pipe`); `shipyard-runtime`'s own modules are in `src/bin/shipyard-runtime/`. | all (it and `shipyard-api` are the only crates that may use the engine: `deny.toml`) |
| `shipyard-api` | Docker's Engine API, a subset (stage 11, D25, D11-1): served by `shipyard system service` on a UNIX socket, hyper 1 on a current-thread tokio runtime, each engine call on a blocking thread with an engine of its own (`Shared::call`, its warnings collected). `routes.rs` (method and path, the `/v1.NN` prefix checked), `errors.rs` (an `ErrorKind` to Docker's status, `{"message"}`), `containers.rs` (the endpoints; `wait` polls the container's standing, so a client that goes ends it), `hijack.rs` (HTTP's upgrade: is it asked, the 101, the client taken back, a hang-up seen), `attach.rs` (the client taken back as its UNIX stream, a hang-up seen by `POLLHUP`, never a half-close; the monitor's frames over `attach_stream`, `HelloHeld`, the monitor's write half held for the session (dropped, it shuts the socket and the monitor lets the client go); an attach to a container not running waits in `Pending` and is welcomed by the API's start before the gate opens, `run::start_with`'s `between` (at most 16 at once, in parallel: F116); each session counts as a connection for the idle rule; `StdinOnce` kept with the container, `created.json` v5, F116), `exec.rs` (exec instances in the service's memory; the child and its streams in `exec/spawn.rs`, the session and its end in `exec/session.rs`; each start a child `shipyard exec` of `/proc/self/exe`, its values in `SHIPYARD_CALLER_ENV` and only the keys on its argv, a raw pty of its own with a terminal (`terminal::take_controlling_terminal`) or pipes, pumped on threads that poll and drain for 2 s after its end (a background process may hold them open); each attached session takes a stream permit; a hang-up hangs it up: SIGHUP to its group, one SIGKILL after 10 s; its code kept before the client's stream ends, as Docker's CLI asks for it at once; a terminal session's detach keys (`DetachKeys`) end the session and leave the exec running; a detached exec lets go of the idle count after 10 s (`DETACHED_HOLD`), or at once once detached from, and runs on past the service (`KillMode=process`); its command's PID read from `Paths::exec_pid_file`, `.exec-<id>.pid` in the runtime root, written by the child's `--pid-file` once it runs: F117), `images.rs` (`docker push`, step 12.4: `POST /images/{name}/push`, its lines as a pull's, then each tag's `aux`; `docker import`, step 12.2b: `/images/create` with `fromSrc=-` takes its body; `docker save`, `load` and `history` (step 12.2a: a save's names found first, `image::check_saved`, then the archive through `archive.rs`'s `ChannelWriter`; a load's body through its `BodyReader`, Docker's JSON lines, the archive's words `plain`); `docker pull`: progress as Docker's JSON lines through a reporter of its own, `Shared::call_reporting`; `X-Registry-Auth` decoded and handed to the pull alone, `image::pull_with`; list, inspect, tag, remove: the engine's `docker::{list_images, inspect_image}` and `image::{tag, remove}`; a name is a reference or an ID, `image::named`, the image ID the manifest's digest, D11.2c-2), `archive.rs` (`docker export`, step 12.2b; `pump_body` reads and drops what's left of a body its reader refused, up to 1 GiB, so the client reads the refusal; `docker cp`: the engine's `cp::{stat, copy_by_name}`; a `cp-stat` helper says what a path is, for `X-Docker-Container-Path-Stat`; a PUT's body read as it comes on the copy's blocking thread, gzip decoded), `networks.rs` (Docker's network endpoints: the engine's `docker::{list_networks, inspect_network, find_network, create_network, check_options}` and `network::{remove, prune_where}`, `run::{connect, disconnect}`; each network's ID made at its making and kept in its `labels.json` (v1, before its `config.json`, with its labels and options), one made before derived (`docker::network_id`); the pre-defined (`bridge`, `host`, `none`, `pasta`) refused 403 in the API; built-ins listed, D11.2c2-3; options honoured or refused, D11.2c2b-1), `volumes.rs` (Docker's volume endpoints: the engine's `volumes::{create_with, in_use, prune_where}` and `docker::{list_volumes, inspect_volume}`; labels in each volume's `labels.json`, v1, written once before its `_data` under its lock), `own.rs` (Shipyard's own views under `/shipyard/…`: a container's `containers::inspect`, a network's `network::inspect`), `streams.rs` (logs in Docker's frames or raw, events as Docker's JSON lines: through a bounded channel body, the reader's `logs::Gone` closure ending it when the client goes or the service stops), `create.rs` and `create/` (`body.rs`; `mounts.rs`, `host.rs`, `network.rs`: each part of Docker's create body to `RunRequest`, by the CLI flags' own parsers; what Shipyard can't do refused), `filters.rs` (the query's JSON to the engine's `docker::filters`), `query.rs`, `fuzzing` (`#[cfg(fuzzing)]`: what the `api_request` fuzz target drives), `system.rs` (`_ping`, and the engine's `version` and `info`), `idle.rs` (when it ends: idle, or stopped and its engine calls ended; connections and calls counted under one lock, no call started once stopping), `socket.rs` (its own socket: a private directory, a lock, a served socket never taken, removed at the end only if it's still the one it bound; at a self-bound service's idle end its name removed and its lock let go before the last look at the backlog, `Bound::unlink`, so a successor binds at once: F149). The peer's UID is checked on every connection (`SO_PEERCRED`). The stop signals are blocked on every thread of the service and taken by one thread's `sigwait`: the engine's `HeldOff` is per thread, so a SIGTERM to a multithreaded process would otherwise kill it mid-create. | engine, runtime, network, sys |
| `shipyard-acceptance` | Acceptance tests (step 11.4b1): Docker's own clients against the API. Testcontainers for Rust (`tests/testcontainers.rs`, default features off) against `shipyard system service` on a socket of its own; it finds the `shipyard` cargo built beside it (or `SHIPYARD_TEST_BIN_DIR`'s). Dev-only, never published; nothing of it is in Shipyard. | (dev) test-support |
| `shipyard-test-support` | Helpers for end-to-end tests: running the binary, hostile parents, the pseudo-terminal harness, network namespaces, fixture registry, a registry that takes pushes (`push_registry`: uploads with their state in each `Location`, Basic or Bearer with scoped tokens, a request log, misbehaviours on demand), `harness::select`, a raw HTTP client over a UNIX socket (`api`), one test's stores and fixture image (`stores::Stores`, which a test file wraps for its own extras), waits (`eventually`, `eventually_every`, `eventually_some`). Dev-only, never published. | — |

### Everything else

- `docs/ROADMAP.md`: stages, TLPI chapter map, recorded decisions, **open findings** (F-rows), the hardening track (H1–H9).
- `docs/FEATURES.md`: the feature list per stage, the production-quality bar, what's out of scope.
- `docs/DEVELOPMENT.md`: setup, commands, running Linux-only code from macOS (container or Lima VM).
- `docs/stages/NN-*.md`: the engineering record of each finished stage. **Read the last one or two before starting a stage**: they say what was carried forward to you.
- `docs/user/`: user docs, published as-is (plain Markdown). `cli.md` is **generated**; never edit it by hand. Describe only what works in the current version.
- `docs/design/`: the UI concept (`shipyard-ui.html` mock, `README.md`: concepts, interaction rules, tokens, the data the UI needs). Read it before any UI or engine-API work.
- `contrib/`: what users install beside the binaries: `apparmor/` (the profile), `systemd/` (the boot units: `system recover` / `system stop`), `macos/` (the entitlements `shipyard` is signed with).
- `scripts/`: `test-linux.sh` (section 7), `fuzz-targets.sh` (the one list of fuzz targets), `ci/`, `probes/` (C and shell programs that check the kernel behaviour a stage relies on), `conformance/`, `guest/` (the macOS guest's build: `build.sh`, pinned in `alpine.lock`), `macos/` (`sign.sh`: with `SHIPYARD_SIGN_IDENTITY`, else the local identity `make-signing-identity.sh` makes, else ad hoc; `test-sign.sh`, its check in a throwaway keychain, `verify.sh --mac`'s; `notarize.sh`, a release's, never run by a check; and cargo's runner on a Mac, `sign-and-run.sh`, set in `.cargo/config.toml` with `MACOSX_DEPLOYMENT_TARGET` 13.0).
- `fuzz/`: cargo-fuzz targets for every parser of untrusted input (`api_request`, `attach_frames`, `challenge`, `cp_unpack`, `cri_record`, `dns`, `import_change`, `load_archive`, `logs_dir`, `manifest`, `netlink`, `ports`, `proc_net`, `process_json`, `reference`, `tar`, `udp_frames`, `unpack`). A new one goes in `fuzz/Cargo.toml` only: `scripts/fuzz-targets.sh` reads it.
- `.claude/skills/`: **the procedures and tools of record**, usable by any agent (each `SKILL.md` is plain Markdown):
  - `shipyard-dev`: start here; routes to the others.
  - `shipyard-step`: one step of a stage, with `references/plan-template.md`.
  - `shipyard-verify`: `scripts/verify.sh` (the whole check of record in one command), `scripts/run-in-linux.sh` (one script in the test container, as `tester` or root), `scripts/linux-lints.sh`, `scripts/guest-crash-rate.sh` (how often the macOS guest crashes, N boots P at a time: F132).
  - `shipyard-mutants`: `scripts/mutation-runner.py` (chosen mutants, controls first).
  - `shipyard-review`: the reviewers' briefs (clean-code, Murphy, the five-area release review).
  - `shipyard-stage-close`: closing a stage, with `references/stage-notes-template.md`.
  - `shipyard-book`: the learning book, with the chapter agents' brief.
  - `shipyard-release`: commit, PR, merge, tag.
  - `review` and `guideline`: general, for any repository.
  - **Vendored** (third-party, read in full before vendoring; each keeps its `LICENSE` and has a "Shipyard notes" block at the top of its `SKILL.md` that overrides its defaults): `systematic-debugging` (obra/superpowers, MIT), `rust-review`, `variant-analysis`, `cargo-fuzz`, `harness-writing`, `coverage-analysis`, `fuzzing-dictionary` (Trail of Bits, CC-BY-SA-4.0), `skill-creator` (Anthropic, Apache-2.0), `grill-me` and `grilling` (Matt Pocock, MIT). Update one by re-copying its files from a newer commit and re-inserting its notes; never edit its upstream text in place.
- `.cargo/mutants.toml`: cargo-mutants' configuration.

### Where things live at run time

| | Root | Anyone else |
|---|---|---|
| Runtime state (`shipyard-runtime --root`) | `/run/shipyard` | `$XDG_RUNTIME_DIR/shipyard` |
| Images | `/var/lib/shipyard/images` | `$XDG_DATA_HOME/shipyard/images` (`~/.local/share/…`) |
| Networks, leases, DNS logs | `/var/lib/shipyard/networks/` | (rootless has no shared networks: pasta) |
| Events journal | `/var/lib/shipyard/events/` | beside the containers' records |
| cgroups | `/sys/fs/cgroup/shipyard/{runtime,monitors,dns}/…` | the delegated `user@UID.service` subtree |

Every one of these is overridable for tests (`SHIPYARD_IMAGE_STORE`, `SHIPYARD_CONTAINER_DIR`, `SHIPYARD_NETWORK_DIR`, `SHIPYARD_EVENTS_DIR`, and the API's socket `SHIPYARD_API_SOCKET`), read only in `Paths::from_env`.

---

## 4. Architecture you must keep in your head

### The process tree of a detached container

```
shipyard run -d …            (exits once the container is started)
└─ __shipyard-monitor        double-forked, its own session, its own cgroup
   │                         (shipyard/monitors/<id>); holds the container's
   │                         sockets, leases, volume and area locks (SCM_RIGHTS)
   ├─ init (container)       born into shipyard/runtime/<id> (CLONE_INTO_CGROUP)
   ├─ __health               companion: runs each __health-check, writes health.json
   └─ __cleanup              the exit command, after the end: records the end,
                             runs the restart policy (waits, then starts again)
```

`shipyard system service` (the Docker API, step 11.2a) isn't a helper: it's a command
of its own, started by systemd's `shipyard.socket` (descriptor 3, `LISTEN_PID`:
`shipyard_sys::listen`) and gone once idle; the containers it starts are made as the
CLI's are (`run::create`, `run::start`: never `run::detached`, whose signals held off
are the process's), so their monitors leave it at once. Its socket is `api.sock` in the
runtime's state root (`Paths::api_socket`).

The DNS server (`shipyard/dns/<net>-…`, a watchdog plus a worker), pasta (rootless
networking) and the rootless storage helper are internal processes too. **All of them are
the same binary, re-executed**, so every binary that runs containers must call these entry
points first thing in `main`, in this order. `shipyard_engine::entry_points(Hooks { init_logging,
refuse_privileged_start })` (`crates/shipyard-engine/src/entry.rs`) runs them all:
`shipyard` and the engine's test binary call it, so the order lives in one place.

1. `process::init_entry_point()`: isolated runs are refused without it.
2. `shipyard_engine::run::cleanup_entry_point()`, right after: `__cleanup` takes the descriptors it inherits before anything opens one.
3. `shipyard_engine::run::health_entry_point()`.
4. `container::monitor_entry_point()`.
5. `shipyard_engine::cp::helper_entry_point()`.
6. `shipyard_engine::image::storage_helper_entry_point(…)`.
7. `shipyard_engine::network::dns_entry_point()`.
8. `shipyard_engine::doctor::probe_entry_point()` (`doctor`'s probe: it makes a user namespace, single-threaded).
9. `shipyard_network::pasta::entry_point(…, shipyard_engine::network::pasta_lost)`.
10. `shipyard_engine::guest::init_entry_point()`, `agent_entry_point()`, `ports_entry_point()` and `doctor_entry_point()`: the macOS guest's PID 1, its vsock agent, the ports its containers publish (`__guest-ports`) and its own checks (`__guest-doctor`), each a process of its own (only ever in Shipyard's own VM).

`shipyard`'s `main` also calls `shipyard_vm::host_entry_point()` (the macOS VM's host process) right after them. The host is started by the CLI (a process of its own), or by launchd once `shipyard vm install` has loaded its agent (step 11.5b2a): then launchd holds Docker's socket, starts the host at its first connection, and the CLI starts it with `launchctl kickstart`.

A binary that doesn't use the whole engine calls only the ones for what it does (the
`shipyard-runtime` binary, the runtime's container tests): one that creates containers with
`container::create` needs the monitor's, one that copies with the engine's `cp` needs the
helper's, one that runs through `engine::run` needs the storage helper's, DNS and pasta's,
one that makes detached containers needs `__cleanup`'s, `__health`'s and the monitor's.
The full set is `entry.rs`; the partial ones: `crates/shipyard-cli/src/bin/shipyard-runtime/main.rs`, `crates/shipyard-runtime/tests/containers.rs`.

### State and locks (stage 9)

- **Records** (`containers/`): `record.json` v3, `deny_unknown_fields`, its config kept as `StoredConfig` (`containers/config/stored.rs`: frozen types of the record's own, converted to and from the engine's `Config` at the edge; `Config` has no serde, so no engine change alters a released format; a v4 adds its own frozen types beside them, never edits these), written atomically (temp file `O_EXCL`, fsync, rename, fsync the directory). Containers appear and disappear whole (`.new-`/`.gone-` renames, swept under the store lock). Readers take no lock.
- **Lock order** (`containers/mod.rs`'s doc, the locks in `containers/locks.rs`; any deviation is a deadlock or a race):
  0. a build's context lock (`<builder dir>/state-….lock`: held for the build, outermost: the builder's container is made and removed under it);
  1. a container's `op.lock` (`start`, `rm`, `stop`, `rename`, `update`, `connect`, `cp`);
  2. its `running` (held by a start's monitor and `__cleanup` until that start is let go);
  3. networks' locks, one at a time / in name order (`connect`, `rename`);
  4. the image store's lock (a load's staging lock is taken before it, with no other held, and kept across it);
  5. a volume's (held shared by a run; `volume rm`/`prune` hold `.lock` and only *try* a volume's); then, innermost, its fill lock (`.fill-locks/<name>`: a create's copy-up, taken while the volume is held shared, nothing taken under it);
  6. the container store's `.lock` (briefly; `volume rm`/`prune` check and trash under it, F56).

  `restarting` is outside the order: its holder only *tries* the others, and a holder of the others only tries it. `__cleanup` takes none of a container's locks, but by *trying* (as the restart waiter does) to retry an end its store refused (F45: `cleanup::retry_end`). The recovery pass and the restart waiter only *try*, so they never wait on a command.
- **"Find, then lock, then read again by ID"**: `containers::lock_again` (`containers/locks.rs`) takes `op.lock`, then `read_one`s the record; `lock_found` finds by reference first. `take` (`run/take.rs`, for start, stop and removal) builds on it. Anything you act on must be read again under the lock. A container's end is waited for with `containers::settled`/`wait_settled` (its `running` file free).
- **A start writes ahead, and undoes:** before it rewrites the record, it keeps what the record said in `previous-start.json` (v2: the start, end, exit and boot, a user's stop, the restart count and backoff, the next start, the ports). A start that doesn't happen, even killed, puts all of it back (F55). A start that never ran leaves no start, no end and no exit (`Record::mark_never_started`, one meaning on every path: F64).
- **What a create settles beside the record** (step 11.3b1): `created.json` (v1: the ports it exposes, `--expose` and the API's `ExposedPorts`; whether its starts give it the default process limit, F62), written inside its directory before it's placed (`containers::write_created`), read by each start (`planned_start`: `-P`'s ports with the image's, `default_pids` where the `pids` controller reaches) and by `inspect`. v2 (11.3e2) adds its tmpfs mounts, v3 (11.4a2a-b) the anonymous volumes its create made (`rm -v` and `--rm` take only those, read under the removal's lock; never a volume given by name) and the bind sources each start makes, v4 (11.4b2b1–2) the containers whose network and IPC namespace it joins (`network_from`, `ipc_from`, by ID: `run/joined.rs` finds it at the create and its init at each start, by PID and start time, and warns those that join a container that stops or goes), v5 (11.6a1) `stdin_once`, v6 (11.7) the bind sources its create made and the directory each was made below (`run::MadeSource`: each start makes and binds them again by descriptor, the runtime's bind `made_below`); each written at the oldest version that holds it. Record v3's config is frozen: F67's rule. The default is applied at each start, never stored as the container's limit, so a start from where `pids` isn't delegated never fails for it (D11.3b-2).
- **An end is said once** (F88): a start leaves `end-unsaid` beside the record, holding the start's time, before it clears the exit (`containers::mark_end_unsaid`); `__cleanup` says the `die` and removes it (`end_said`); `take`'s `reconcile`, and after a reboot the recovery pass (`settle_unrecorded_end`), say an end that's recorded but whose mark names this start. A mark naming another start (one killed before it recorded itself) says nothing; no mark (a container an older binary started) is the old rule: an end is said only when the record has none. Said, then unmarked: a kill between the two repeats a `die`, never loses one (F122).
- **A container's writable layer is mounted once at a time** by its area's exclusive lock (`image::prepare_owned`, for a start and a stopped container's `cp` view alike): overlay is mounted with `index=off`, so the kernel won't refuse a second mount itself (F60).
- **Liveness is a lock, never a PID.** PIDs are reused; a `flock` held by a live process isn't. Signals go through pidfds, with a start-time check.
- **Every on-disk format has a version from its first write (H2)**, and a test that reads the previous version. Newer versions are refused, never guessed at.
- **A record is rewritten at the version it was read at** unless a field set in it needs a newer one (`containers/record.rs`'s upgrade rule, F43): an older binary's monitor still running must still read what a newer CLI rewrote. A new field the engine sets on a possibly running container goes in a file of its own beside the record, never in a new record version (F67).
- **Failure leaves the store sound.** Each SIGKILL point (CLI, monitor, `__cleanup`) is settled by `take`/`reconcile`/the recovery pass, and has a test in `crates/shipyard-engine/tests/detached.rs`. A new step that adds a kill point adds its test.

---

## 5. Code rules

- **Platform gating:** `#[cfg(unix)]` for POSIX code (must work on macOS and Linux); `#[cfg(target_os = "linux")]` for namespaces, mounts and cgroups, returning `Error::LinuxOnly` elsewhere.
  An item that exists everywhere but only Linux code calls isn't gated: it carries `#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "…"))]` (or the lint it trips there), so it still compiles on macOS (and any tests it has run there). `#[cfg(target_os = "linux")]` is for what only Linux has, or what would be a stub nothing tests off it.
- **Exit codes:** `u8` where a process's status becomes the CLI's exit code (`run::Finished.code`, `exec::Ran::Exited`, `exec::exit_code`); `i32` where the monitor's protocol or a record carries one (`attach`'s `Ended` and `Settled`, a record's `Exit.code`, `run::wait`, health), carried as it came. An `i32` becomes the CLI's `u8` with `u8::try_from(code).unwrap_or(EXIT_ENGINE_ERROR)` (125), never `as` (256 would be 0): `run/foreground.rs` and the CLI's `containers.rs` do it.
- **Syscalls:** `std` first, then `nix`/`rustix`, raw `libc` only when neither covers the call. The one raw call of `shipyard-sys`, `pidfd::send_signal`: rustix's `Signal` can't be 0 and, by its contract, mustn't send a real-time signal, and Shipyard sends both. Low-level plumbing (atomic writes, bounded reads, locks, pidfds, `SCM_RIGHTS`, inherited descriptors) is `shipyard-sys`'s, never a crate's own copy.
- **`unsafe`:** every block needs a `// SAFETY:` comment (clippy denies otherwise) that argues *why*, not what.
- **Lints:** clippy `pedantic`, CI runs `-D warnings`. Fix lints rather than `#[allow]` them; a needed allow carries a `reason`. `too_many_lines` (100) and `too_many_arguments` (7) are real limits: split the function (a struct of what travels together), don't allow.
- **Errors:** `thiserror` enums in libraries, `anyhow` in the CLI. No `unwrap()` outside tests. **Tests match an error's kind, not its wording (H6)**; a new error gets a variant (or an `ErrorKind`).
- **CLI output:** `writeln!` to a locked stdout, never `println!`/`eprintln!` (a closed pipe must be an error, not a panic). **Text that came from outside** (container names, logs, labels, a registry's or a layer's words) is passed through `printable()` (tables, events) or `printable_message()` (stderr) before it reaches a terminal.
- **A broken pipe is quiet only when it's stdout's own reader that's gone** (`broken_pipe` asks the kernel with `poll`). Any other `EPIPE` is a failure to report.
- **Dependencies:** versions once in `[workspace.dependencies]`; members use `.workspace = true`. `cargo deny check` must pass (licences, bans, advisories).
- **Every public item has a doc comment** (`missing_docs`). Write comments the way the surrounding code does: short, plain, saying why.
- **New engine logic goes in a library crate behind a typed API (H1)**, never in the CLI. Prefer a function that returns data the CLI prints.

### Code that runs after `fork`

`crates/shipyard-runtime/src/process/child.rs` (and anything it calls), and every
`pre_exec` closure, runs in the child between `fork` and `exec`. Locks other threads held
stay locked forever there. That code must:

- **never allocate, panic or take a lock**: no `Vec`/`String`/`format!`, no `println!`, no `tracing`. The module denies indexing, arithmetic side effects, `unwrap`, `expect` and `panic`.
- **call only async-signal-safe functions**, or nix wrappers that are thin calls to them (nix's `execve` allocates: use `libc::execve`).
- **after a raw `clone` (isolated runs), call no libc function with process-wide state or an internal lock** (glibc's `setuid` family, `raise`, `pthread_*`): use the raw system call (see `switch_credentials`).
- **report failures as numbers** (step, errno, detail) over the report pipe, then `_exit`; the parent turns them into messages (`failure.rs`).

Everything the child needs is prepared before `fork`, in `plan.rs`. The clean image
(`process/init.rs`) is an ordinary single-threaded process; the command it forks runs
under these rules again.

**The terminal tests** (`crates/shipyard-cli/tests/terminal.rs`, `harness = false`) fork a
fake shell that keeps running Rust: never start a thread in that binary.

---

## 6. The workflow (every step)

A stage is split into steps in its plan; a step too big for one round is split further
after asking.

1. **Orient.** Read the stage's ROADMAP/FEATURES entries, the previous stage notes' "Carried forward", the open findings with this stage as home, and the code you'll touch. Read before you write.
2. **Plan, then get it approved.** Context; decisions (dated); a **threat model** (who is hostile: the container, its image, a registry, a local user, the network; each invariant they must never break, with a test); design per file; tests (units, Linux end-to-end root and rootless, fuzz targets for any parser); mutation checks; docs; the TLPI concepts. Real decisions (a default, a trade-off, a split) go to the user as 2–4 options, the recommended first, each with its consequence. Don't ask what the code or a sensible default answers.
3. **Build it with its tests.** `cargo fmt`, macOS clippy and tests as you go. **Linux-only code isn't done until it has run on Linux** (section 7).
4. **Two independent reviews**, report-only, in parallel: a *clean-code* review (this file's conventions, naming, duplication, size) and a *Murphy's-law* review (concrete failure scenarios, state → wrong outcome, with file:line: processes killed at each point, reboots, upgrades from the previous format, races between two CLIs, hostile local users, resource exhaustion, what the host itself loses).
5. **Verify every finding against the code before acting.** Reviewers are wrong sometimes (the v0.2 review claimed the bridge had no anti-spoofing; `nft/spoofing.rs` says otherwise). Fix what's real, each fix with a test; reject or carry forward the rest, saying why.
6. **Mutation checks** on the critical lines (section 9). A survivor is a missing or vacuous test.
7. **Full verification** (section 7), then **manual checks** as a user would run them, with real images.
8. **Record it:** the plan's "built and reviewed" section, the docs (section 8), the F-rows.
9. **Report** (section 11).

### Closing a stage

1. Gap and regression tests, the milestone end to end.
2. A whole-stage clean-code review and Murphy review: interactions across steps, upgrades, reboots, other programs on the host.
3. Stage notes `docs/stages/NN-name.md` in the established format: Outcome (requirement → where → tested-by; a "moved out" table), Architecture, Decisions and why, Kernel checks, macOS and Linux differences, Failure-mode register (case → handling → test), Fuzzing, Known limitations, Carried forward, Tests, Review history (one row per step).
4. ROADMAP (stage done, decisions, milestone), FEATURES, `docs/user/README.md`'s status and limitations, this file's map, the CHANGELOG.
5. Fuzz every target, run the full verification, do the manual milestone on real hosts.

### The hardening track (applies to every step)

- **H1:** engine logic in a library behind a typed API.
- **H2:** every on-disk format versioned on first write, with a previous-version test.
- **H3:** a threat model in every plan.
- **H5/H8:** anything that replaces a standard tool gets a differential test against it, and a line in the ROADMAP's H8 record saying why it's ours.
- **H6:** test the kind of an error, not its words.
- **H7:** a timing budget for anything on a run's path (an end-to-end test that fails if it gets slower than a generous bound).
- **H4:** CI's Ubuntu isn't the test container (AppArmor, systemd-resolved's `127.0.0.53`, another cgroup layout): say so when a change touches them.

---

## 7. Verify (the check of record)

**All of it, in one command:** `.claude/skills/shipyard-verify/scripts/verify.sh`
(`--mac` for minutes, `--vm` for the macOS VM's tests with the guest built from this
tree, no flag for everything including both Linux passes and, on a Mac, the VM's part,
about an hour: run it in the background). For a change that must change nothing, `differential.sh mac tester root` beside it compares v0.2.0's binaries with this tree's (stdout, stderr, exit codes, files left behind). Logs in `target/verify/`, a summary with the totals at
the end, non-zero if anything failed. The pieces it runs:

**At night,** `scripts/watch.sh` (a launchd agent once installed) checks `origin/main` the
same way, plus 10 minutes of fuzzing per target and cargo-mutants on what changed: its
report is `~/Library/Caches/shipyard-watch/runs/latest/report.md`. Read it at the start of
a session; a regression it found comes before new work.

### Every change, on macOS

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
cargo deny check
UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference   # after any flag or help change
```

### Linux (any change that touches Linux code, so: almost every change)

macOS can't cross-check Linux (`ring` needs a Linux C compiler). Use the container:

```sh
timeout 7200 scripts/test-linux.sh            # clippy, then all tests as `tester`, then as root
scripts/test-linux.sh --shell | --root-shell  # debug in the same container
scripts/test-linux.sh --fuzz [SECONDS]        # every fuzz target (do it at a stage's end)
scripts/test-linux.sh --mutants [BASE|FILE.diff]  # cargo-mutants sweep (a report)
scripts/test-linux.sh --conformance [NAME…]   # the OCI conformance suite
```

Both passes run with `--no-fail-fast`, and the root pass runs even if the first failed;
the script fails at the end if either did. Read the totals (`test result:` lines), not just
the exit code.

**Also run in the container** (`.claude/skills/shipyard-verify/scripts/linux-lints.sh`, as root through `run-in-linux.sh`):

```sh
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --document-private-items
apt-get install -y musl-tools && rustup target add aarch64-unknown-linux-musl
CC_aarch64_unknown_linux_musl=musl-gcc cargo clippy --locked --workspace --all-targets \
  --target aarch64-unknown-linux-musl -- -D warnings
```

musl catches what glibc hides (libc types differ: `statfs`'s filesystem type is `u64`;
`time_t` is deprecated there). Prefer nix's typed constants to raw libc types.

### The test container's facts

- Its root is overlayfs, and `/tmp` is a tmpfs: an image store must be on `/tmp` (`SHIPYARD_IMAGE_STORE=/tmp/…`). Unprivileged overlay whiteouts work on tmpfs but not on overlayfs: for whiteout failures, check the filesystem first (`stat -f -c %T`).
- Each run is a fresh container: images pulled in one aren't in the next.
- `tester` gets a delegated cgroup (`user.slice/tester/session`), as a real login has.
- The image is tagged by `scripts/test-image/Dockerfile`'s hash: after changing it, build the new image at once, or every runner fails to find it.
- Docker volumes: `shipyard-linux-cargo` and `shipyard-linux-rustup` (the toolchain: keep), `shipyard-linux-target` (rebuildable, grows past 50 GB: offer to delete after a stage), `shipyard-mutation-target` (per-mutant builds).

### Real distributions (Lima)

The container has no AppArmor or systemd. A milestone is checked by hand on Lima VMs
(Ubuntu 24.04 arm64 with AppArmor's restriction on, Fedora with SELinux enforcing, Ubuntu
x86-64 under QEMU), each through a reboot, with a static musl build. See
`docs/DEVELOPMENT.md`. Copy files in under distinct names (`shipyard-bin`,
`apparmor-profile`: the profile once overwrote the binary), and after setup check
`stat -c %F /dev/null` says "character special file".

---

## 8. Docs are part of done

If a change affects what users see (commands, flags, behaviour, exit codes, messages):

- update the matching page in `docs/user/` (only what works in the current version);
- regenerate `cli.md`: `UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference` (a test fails while it's stale);
- add a CHANGELOG line under the unreleased version (Added / Changed / Fixed / Security / Known limitations);
- **when a message changes, grep everything for the old wording**: `crates/*/src` (unit tests there are often Linux-only and invisible on macOS), `crates/*/tests`, `docs/user`, `docs/book`.

A finding you don't fix becomes an F-row in the ROADMAP's open findings (severity, the
finding with its source, the fix, its home stage). Remove the row when its fix lands.

The learning book (`docs/book/`, gitignored, the user's) is updated after a step or stage
lands: the agent reads the code first, describes only real behaviour, uses short plain
sentences, and verifies every "Try it" command by running it (never writing a device).
Brief book agents with `.claude/skills/shipyard-book/references/book-agent-brief.md`.

---

## 9. Testing: what makes a test count

- **A negative check needs a positive control in the same test.** "Can't reach X" passes vacuously if X is unreachable anyway (lo down, no route back, wrong port).
- **Pin messages precisely** (`contains("is internal: nothing on it can be reached")`, not `contains("internal")`), or better, the error's kind.
- **No sleeps for synchronisation.** Wait on the condition (`eventually(…)`), the lock, the pidfd. A sleep followed by a negative assertion passes on a slow host.
- **A fix against a race needs a test that loses the race without it.** Check the mutant fails *for the right reason* (read its panic line).
- **Root network tests never touch the host's networking:** `in_new_netns`, `NetworkDir`, `Upstream` (a namespace standing in for the world). A new namespace has `lo` down and inherits the host's `ip_forward`.
- **CI's root job runs each test binary in a network namespace of its own** (`scripts/ci-netns.sh`), so root tests can make bridges without touching the runner's network.
- **CI's runner isn't the test container:** Ubuntu's `/etc/resolv.conf` names systemd-resolved's `127.0.0.53` (runs with a network of their own warn about DNS; `test-linux.sh` binds a loopback `resolv.conf` to match). A test that checks stderr looks for its own line, never assumes it's the only one.
- **Rootless from root:** `TestUser` (with its own `XDG_RUNTIME_DIR`; `delegated_cgroup` for `--memory`).
- Root-only tests are `#[ignore = "needs root"]`; run them with `--include-ignored`. Tests that can't run here return early unless `SHIPYARD_TEST_ROOTLESS`/`…DELEGATED=required` (as `test-linux.sh` sets), so **a green run without those variables proves less than it looks**.
- Shared helpers live in `shipyard-test-support`; extend it instead of copying a helper.
- Test limits off record boundaries, and assert the path you meant was taken (`len < limit`).

### Mutation checks (the check of record)

Use `.claude/skills/shipyard-mutants/scripts/mutation-runner.py`: copy it to a scratch
directory, fill in `MUTANTS` (`name: (where, file, exact text, replacement, test command)`,
where `where` is `mac`, `tester` or `root`), then:

```sh
python3 mutation-runner.py --control   # each mutant's test on unchanged code: must all pass
python3 mutation-runner.py             # CAUGHT / SURVIVED / NO-RESULT per mutant
```

- **Controls first.** A test that fails anyway "catches" every mutant and proves nothing.
- **One scratch copy and one cargo target dir per mutant.** A shared target dir runs the wrong binary (two mutants with identical failure lists are suspect).
- SURVIVED needs `test result: ok` with a test run; anything else without a failure is NO-RESULT. A control that runs no test fails: a test path that names none passes with `0 passed` (12.5c1: a mutant's filter doubled `tests::`, ran nothing, and "survived"; the runner checks since).
- Run a mutant where its test runs: a Linux-only test on macOS runs zero tests and "survives".
- `scripts/test-linux.sh --mutants` is a sweep beside it, not instead: it runs unprivileged, so mutants only root tests catch show as missed.

---

## 10. Gotchas that already cost hours

- **Stale cargo cache:** names that plainly exist are "not found". Compare source mtimes with `target/debug/deps/lib<crate>-*.rmeta`; `touch` the crate's sources (never `cp -p`/`rsync -t` older copies back in).
- **`cargo test` stops at the first failing binary:** always `--no-fail-fast`, or a count is incomplete.
- **Scripted edits:** assert every anchor occurs exactly once *before* writing anything; regex refactors catch more than meant (`at(` matched `Shipyard::at(`): grep the result.
- **A file any user can open is a file any user can `flock`:** keep liveness lock files out of other users' reach.
- **`flock` locks belong to the open file description, and nix's `Flock` unlocks on drop** (`LOCK_UN`), even if a dup is still open. A lock whose copy goes to another process is a std lock held in `shipyard_sys::lock::HeldFile` and given up, never a `Flock` (`shipyard_sys::lock`'s doc says which is which).
- **The run's subreaper reaps leftover children:** code waiting for its own child afterwards must treat `ECHILD` as already reaped. Start long-lived helpers before `Supervisor::install`.
- **Networking (stage 7):** set a bridge's MAC (unset, it changes when a port leaves); `accept_dad=0` on bridges and containers; set `accept_ra=2` before turning IPv6 forwarding on; writing `ip_forward=1` resets every interface's `forwarding`; forwarding must never make the host a router for anyone else.
- **`MSG_CTRUNC`:** on `EMFILE` the kernel installs some descriptors, truncates, and still reports those it installed; nix's `cmsgs()` then returns none, and they leak. `shipyard_sys::fdpass::receive` reads with rustix's `recvmsg` on Linux, whose walk yields them (F54). rustix's `cmsg_space!` adds room for alignment: room for one descriptor holds four.
- **A test that spawns a process holds a copy of every descriptor in the test process from its `fork` to its `exec`**, close-on-exec ones too (they close only at the `exec`): a test beside it that drops a socket and binds its port again can find it still in use. Wait for the port with a bound; don't assume it's free at once (10.2d2: a pasta test made a ports test fail 2 runs in 5).
- **`fcntl(fd, F_DUPFD, n)` returns the lowest free number from `n`, not `n` itself:** use what it returns. A test helper assumed 200 and `dup2`'d whatever was there onto a hold once another test filled that range (10.2c1).
- **A process's descriptor limit bounds numbers, not counts:** a helper handed N descriptors at 3 and up, under a soft limit below N + 3, can't open a single file (the lowest free number is past it). The monitor raises its own limit (F49), and its exit command gets room for its holds.
- **A mutant's copy has no `.git`:** a script on its test path (cargo's runner on a Mac, `scripts/macos/sign-and-run.sh`) that finds the repository with `git rev-parse` fails there, and every mutant's control "fails" (exit 128) before its test runs. Find the repository by the script's own place (10.3b2).
- **macOS sockets (BSD), checked in 10.3c1:** with `SO_REUSEADDR` (std's `TcpListener` always sets it), a bind of `127.0.0.1:P` succeeds beside another program's `0.0.0.0:P` and takes its local connections (the more specific wins): bind without it to be refused (`EADDRINUSE`). Without it, a port held only by a `TIME_WAIT` can't be bound for ~30 s either (`shipyard-vm`'s `bind_alone` tells the two apart by connecting). An accepted socket inherits its listener's `O_NONBLOCK` (Linux's `accept4` doesn't): set it blocking before `io::copy`. And the reverse holds only on BSD: a wildcard bind with `SO_REUSEADDR` beside a specific one bound without it succeeds there and is refused on Linux (which wants it on both), so a test of F98's probe (`ports::held_on_every_address`) runs on macOS alone (13.5b: it failed in the Linux pass).
- **macOS: `setsockopt(SO_RCVTIMEO)` on a Unix socket whose peer has already closed fails with `EINVAL`** (Linux accepts it): don't fail a read on it, the read ends at once anyway (`client::ask`). And a test's fake peer that answers before reading the hello races the client's write (`EPIPE`): read what the real peer reads first (stage 10's close: a retry test failed 4 runs in 20).
- **The macOS VM tests boot a 4 GiB VM each:** a dozen at once (cargo's test threads) wedged a guest (its agent stopped answering) and made a stop miss its bound. `tests/vm_support` lets two run at once (`AT_ONCE`; four ran until 11.5a) (stage 10's close).
- **A wait for "the VM stopped" is a wait for *that* host's PID to be gone:** another command may start a new host the moment the old one lets its lock go, and `status` then says "running" again. `vm stop` waited for "stopped" and failed after 120 s (stage 10's close).
- **An internal entry point is reachable by anyone who types its argument:** the binary is the same everywhere, so one meant for one place (the macOS guest's `__guest-init`, which moves `/` into memory and removes the rest) must check it's there (`guest::in_the_guest`: the kernel command line Shipyard boots with, PID 1) before it does anything. The fresh review after stage 10 found it unguarded on Linux hosts; its test never runs as root. And the guest's PID 1 starts with no `/proc` (the initramfs has none mounted): the first guard read an empty command line and refused the guest's own init, which only the VM tests showed (every one timed out at boot).
- **The macOS VM is "up" when its agent answers, not when its guest is ready:** since 10.4 (D10.4-4) the agent starts before the data disk's check and refuses with `forward::NOT_READY` until it's done, so every client (`client::until_ready`) waits on that refusal, not on a bound. 10.4's VM tests passed by timing; 11.1's run lost the race (the first command after `vm start` refused).
- **Docker's CLI waits for `wait`'s headers before it starts a container** (`docker run`: `POST /containers/{id}/wait?condition=next-exit`, then start): Docker's daemon sends 200 and its headers at once and the body at the end. An API that holds the whole answer back hangs every `docker run` (11.2a's first Linux run: the CLI blocked on one connection, the service idle in `epoll`). An endpoint that answers late answers its headers first (`reply::channel`).
- **The umask is the process's, not the thread's:** the API's `socket::bind` narrows it for a moment (sound in the service, one thread then), and a test beside it in the same binary that made a directory meanwhile got `0600` instead of `0700` (11.2b2a's root pass). Tests that change process-wide state (umask, the working directory, the environment, signal dispositions) run one at a time, under a lock.
- **A terminal's hang-up can send its foreground group SIGHUP twice:** once as the terminal closes, and again when the shell that owned it (the session leader) dies of its own SIGHUP. A process that handles the first and then puts SIGHUP's default back (a `SignalWatch` dropped) can die of the second before it exits: the Mac's `shipyard` ended `Signaled(1)` instead of 129 in 1 run of 6 (11.2b2b's verify). Keep the handlers until the exit (`client.rs`'s `HungUp`).
- **A process's `/proc/PID/cmdline` empties before its files close** (the kernel's `do_exit`: `exit_mm`, then `exit_files`): a scan that takes an empty command line for "gone" can see a process gone while its locks are still held. Wait on the lock (or a pidfd), never on `/proc` (11.3a's verify: `a_killed_servers_worker_is_started_again` read `DNSRunning: true` after the servers were "gone").
- **`poll` on a pipe's write end with no readers:** `POLLERR` on Linux, `POLLHUP` on macOS.
- **`system stop` holds off every restart in its runtime root** (`HOST_STOPPING`) until a `system recover`, and root's (`/run/shipyard`) is every root test's: a root test that runs it makes another's restart policy never fire (`a_monitor_lives_in_a_cgroup_of_its_own` failed beside 11.3a's partial-recovery test, alone 20/20). Run such tests rootless (each has its own runtime directory), as the stage-8 test does.
- **`dup2(fd, fd)` does nothing, close-on-exec included:** a `pre_exec` that moves a descriptor to 3 with `dup2(fd, 3)` leaves it closed at the `exec` when it *is* 3 already (std opens everything close-on-exec). Clear the flag with `fcntl(3, F_SETFD, 0)` then. `it_serves_the_socket_systemd_passes` failed now and then for this, as root and as tester, wherever the listener happened to get 3 (11.3b1; 11.3a had blamed the idle time).
- **A command's output that changes shape breaks the tests that read it through `shipyard-test-support`:** grep the helpers, not only `tests/`, and run the whole Linux pass, not the tests you chose. 11.3c2 made `network inspect` Docker's; `NetworkDir::inspect` read it for Shipyard's own view, and 35 root tests in six files failed in the full pass only.
- **The macOS VM tests fail under a heavy load beside them:** run beside a Linux pass (Docker's VM busy with root tests), three failed (an idle stop past 30 s, a forwarded `run -d` hung until stopped, even an overlay mount refused `EINVAL` in the guest) and passed 18/18 alone after (11.3c2's verify). Run `verify.sh --vm` alone, never beside `--linux` or mutants.
- **A test's pipe made without close-on-exec leaks into the command it spawns:** `nix::unistd::pipe()` sets no `O_CLOEXEC`, so the child holds the read end too, the pipe never loses its reader, and a write blocks for good (a 34-minute hang in 10.2a; `/proc/PID/wchan` said `anon_pipe_write`). Use `std::io::pipe()`.
- **Code under `#[cfg(fuzzing)]` is compiled by the fuzz build alone:** no `cargo build`, clippy or test sees it, so a signature it calls can change under it unnoticed until a fuzz run, which then fails to build every target and reports them all "crashed" (11.6c: the API's `fuzzing` module still called `exec::child_command` with two arguments). `verify.sh --mac` checks it (`RUSTFLAGS="--cfg fuzzing" cargo check --workspace`); read a fuzz run's log before believing its crashes.
- **The test image keeps no apt package lists** (its Dockerfile removes them): a script that `apt-get install`s in it must `apt-get update` first. `linux-lints.sh` installed `musl-tools` silently and failed only once the image was rebuilt from scratch (11.4a1: the user's cleanup pruned the old image, whose container had them).
- **A unit test runs with the repository as its working directory, read-only in the test container:** a file it makes "here" fails only on Linux. Make it in the temporary directory (11.4a1's `docker_is_found_on_the_path`).
- **A macOS guest that crashes takes its evidence with it:** each VM test removes its directory, `console.log` included, and a test whose guest died once waited for ever (since 13.8a every command is bounded: `vm_support::COMMAND_LIMIT`). Copy the consoles while the suite runs (a loop that keeps any showing `Oops`), and measure a rate before blaming anything: `guest-crash-rate.sh` boots N guests P at a time and gives the rate with its bounds. 11.4b1 blamed this tree's guest on 0 crashes in 2 runs against 4 in 6; 11.5a's ~550 boots showed the crashes come with four VMs at once (6/227) and not one alone (0/157), on 6.12 and 6.18 alike (F132); 13.8a's 360 (6.18.58, macOS 27.0.1) had none, four at once included. Count a failed pull apart (`REGISTRY`): a mirror that lost a blob for a minute once read as six hangs.
- **Virtualization.framework gives a network card without an address a new random one at each boot,** and the Mac's NAT (`bootpd`) leases 253 addresses for an hour each: ~400 boots in a few hours (F132's harness) used them all, and every guest booted after had no network ("failed to lookup address information", which first passed for a network blip). The VM keeps its card's address (`network.rs`); a script that boots many VMs reuses a few addresses (`guest-crash-rate.sh`'s slots). When guests get no DHCP lease, count `/var/db/dhcpd_leases` first (11.5a).
- **A mount can't be bound or cloned from a mount namespace the caller isn't in** (the kernel's `check_mnt`: `mount(MS_BIND)` and `open_tree(OPEN_TREE_CLONE)` of `/proc/PID/root/...` both fail with `EINVAL`, even on 7.2): a process that enters that namespace can clone it, and the detached mount, which belongs to no namespace, attaches anywhere (`move_mount`). `--ipc container:`'s `/dev/shm` works so (`scripts/probes/foreign-shm.c`, 11.4b2b2).
- **launchd (macOS 27) runs a socket job once as it loads it, and makes its socket only when the job checks in** (`launch_activate_socket`): a job that never does gets no socket at all, and a connection starts a job only 10 s after its last start (its throttle). A socket job must check in at each start, and exit at once when nothing waits (11.5b2a: the host first booted the VM at install; a `/bin/sleep` job's socket never appeared).
- **A program launchd starts reads the folders macOS guards (Desktop, Documents, Downloads, iCloud Drive) only with the user's permission**, asked of the program itself (one started from a terminal has the terminal's): a host under `~/Desktop/shipyard/target/debug` hung in `dyld`'s `open` of its own binary, before its first line, each rebuild a new ad-hoc identity to be asked about. `vm install` refuses a binary or guest there; `vm_launchd.rs` installs copies in the temporary directory (11.5b2a, found with `sample`).
- **A shell starts a background job (`… &`, non-interactive) with SIGINT and SIGQUIT ignored, and an ignored signal stays ignored across `exec`:** a test run from one passes that on to the `shipyard` it starts, which rightly keeps ignoring a SIGINT (as a background job must). F131's two "flaky under load" signal tests, and 11.5b2b's VM Ctrl-C test, failed for this alone, in every `verify.sh` started with `&`, never alone. `Shipyard::command` (test-support) puts them back to their defaults; `vm.rs`'s `with_sigint` does the same for a VM test. Before blaming load for a signal test, run it from `bash -c '… & wait'`.
- **A peer that closed isn't always `POLLHUP`:** macOS's UNIX sockets and Linux's vsock ones report nothing to `poll` for a peer that has closed (Linux's UNIX ones do); a zero-length `send` fails `EPIPE` on all three once it has, and succeeds while it has only half-closed (`shipyard_sys::relay`'s `peer_gone`, probed in 11.7). And Virtualization.framework passes the host's close of a vsock on to the guest as a half-close alone, so the guest can't tell a Mac client that died from one that finished sending: the host says so (`forward::API_HANG_UP`). 11.7's first fix stopped at the Mac's side; only the VM test of a killed quiet `docker exec` showed the guest never learned.
- **A tokio current-thread runtime's timers run only while it's driven:** a blocking thread that `Handle::block_on`s a `sleep` gets nothing once the runtime stops (and a panic is an abort in release). Bridge blocking threads and async code with a condvar the async side bumps (`reply::Progress`), never a fixed sleep: 11.7 measured a 50 ms sleep-poll holding every API stream to ~320 chunks a second.
- **A file written whole (a temporary beside it, then `rename`) replaces whatever has that name:** given a user's path (`save -o`), that's `/dev/null` or a FIFO as easily as a file, and as root a rename over `/dev/null` breaks the host. Write into anything that isn't a regular file as it is (12.2a's Murphy review; `cli/output.rs`'s `write_archive_to`, `save -o` and `export -o`). Docker's CLI refuses a device for `-o` itself, so a test of the API's `save` gives it a file.
- **BuildKit copies a context file it can't read as an empty one, silently** (a wrong image, exit 0): as root its builder is another user (from 2100000000), so a 0600 file of root's came out empty. `build` checks the context whole first (12.3's tests found it). And root's builder can't enter the image store (root's 0700): its places live beside it (`<store>-builder/`), as 12.1's probe had found for volumes.
- **`--uidmap 0:100000:65536` is the first user's own subordinate range on Ubuntu, Debian and Fedora** (`useradd` gives `alice:100000:65536`): a namespace Shipyard maps there is one that user can become (`newuidmap`, or her own rootless container). The builder uses a range no user is given and checks `/etc/subuid` (12.3's Murphy review).
- **`test-linux.sh`'s passes have no DNS for a rootless container:** they bind a loopback `resolv.conf` (`127.0.0.53`, as CI's Ubuntu) that nothing serves there, and pasta forwards a container's DNS to it. The build tests (a builder pulls by name) run in steps of their own after both passes, with the container's resolver and a fresh `/tmp` each (`SHIPYARD_TEST_BUILD=later` skips them before): in the passes, the tester ones failed on DNS and the root ones filled `/tmp`'s tmpfs, BuildKit images beside what the passes left, rootless stores' subordinate-owned files no test can remove among it (12.3's verify).
- **`std::io::copy` from a socket into a pipe can hold bytes until the socket's end:** on Linux std uses `splice` when one end is a pipe, and from a UNIX socket into a child's stdin it gave a typed line to the command only once the client closed. An interactive stream copies with a plain read-then-write loop (the API's exec input, `pass_on`; 12.5a's probe found it: `docker exec -i` through the API answered 15 s late).
- **`ImageReference::parse` fills in `latest`:** a parsed reference can't tell `app` from `app:latest`, so "did the user give a tag" is read from the text (`image::names_a_tag`: a `:` after the last `/`). `push -a` refused every name for it, and the API's all-tags push could never be asked, until 12.4's Linux run.
- **A stage's work stays uncommitted until it closes, so git holds none of it:** `git checkout -- FILE` puts back the last commit and loses every step's changes to that file since. Copy the tree aside before a refactor (12.5b2 kept one in the job's scratch directory) and restore from the copy, never from git; the copy is also `differential.sh --base-dir`'s baseline.
- **A network can drop TLS 1.3 hellos that carry the post-quantum key share** (`X25519MLKEM768`: OpenSSL 3.5+ and Go 1.24+ send it, so BuildKit and Alpine's `apk` do; the hello no longer fits one packet): this Mac's connection did (13.0–13.2), to Docker Hub and Alpine's CDN alike, while the Mac's own `curl` (LibreSSL, no such share) and browsers worked, so it passed for an OrbStack fault through two restarts. Test it before blaming anything: `openssl s_client -connect registry-1.docker.io:443 -groups X25519MLKEM768` against `-groups X25519` (Homebrew's `openssl@3`). The guest builds over plain HTTP then (`SHIPYARD_GUEST_MIRROR`: `apk` checks signatures); the build tests wait for another network.
- **The guest's view of a share is cached:** a directory the Mac removes and replaces with a symlink is still a (deleted, empty) directory to the guest until its entry lapses, so a test of what the guest makes of a change on the Mac must make it through the guest (a container), as a hostile one would (13.4: the start's re-check "missed" a link the Mac made, and bound the stale directory). A share of `/Volumes` reaches the volumes mounted under it, those attached after the boot too; and a hiding tmpfs holds for another case of its path (`~/library/…` on APFS): a directory has one dentry in the guest, whatever name finds it (both checked in 13.4).
- **The fuzz build is nightly's, so a deprecation shows there first:** stable clippy passed `AtomicUsize::fetch_update` while the fuzz logs warned it's renamed `try_update` (13.5a), a name stable may not have yet. Use what both take (a `fetch_add`, then a check), and grep a fuzz run's log for `deprecated` as well as for crashes; `cargo +nightly check --workspace --all-targets` in the test container says it at once.
- **`run-in-linux.sh tester` gives the test user no delegated cgroup** (only its `root` mode makes `user.slice/tester/session`): a rootless build there fails in BuildKit's `runc` (`no cgroup mount found in mountinfo`: F174), which passed for a network fault at first. Run rootless build tests as `test-linux.sh` does: `root`, then `setpriv --reuid tester` with the shell's PID written into that cgroup first (13.5a).
- **A listening vsock socket completes connections in the kernel, accepted or not:** once the guest's PID 1 held the agent's port (13.5b), a host's bare `connect` succeeded with no agent behind it, so "the agent is up" became an answer asked for (`wait_for_agent` asks the boot ID), never a connect; and what connects while the agent restarts waits in the backlog (a command stamped more than a minute ago is refused, not run late).
- **macOS's `MSG_DONTWAIT` doesn't keep a UNIX stream socket's `send` from waiting:** a
  `send(…, MSG_DONTWAIT)` of more than the socket has room for blocked in `sendto` (13.8c's
  first test run of the UDP `Outbox` hung there; Linux returns `EAGAIN` or a part). A
  socket that must never wait is set `O_NONBLOCK` (its own connection's: the flag is on the
  file description, shared with every `dup`).
- **systemd:** `ExecStop` runs only for a unit that started; session scopes are killed at shutdown in parallel with units' `ExecStop`; a monitor in a session scope dies first (hence `shipyard/monitors/<id>`).

---

## 11. Working with the user

- **Explain the systems concept** behind each step: which syscall, what the kernel does, which TLPI chapter (e.g. `flock` §55.2, `SCM_RIGHTS` §61.13.3, `poll` §63.2). In the plan and in the report.
- **Report plainly, with numbers:** what was built; what the reviews found, fixed, rejected (and why); verification (tests passed/failed per pass, mutants caught/total with controls, fuzz time); what has no test and why; what's carried forward. If something failed or was skipped, say so. Don't call a thing done that hasn't run.
- **Keep them posted** on long work: a line when a long job starts, its result when it ends.
- A score or a verdict is backed by evidence you checked, not by a reviewer's say-so.

---

## 12. Agent superpowers (how to work at full strength)

**Parallelism.** Independent reads, searches and checks go in one batch. Long runs
(`test-linux.sh`, mutants, fuzzing) go in the background while you work on what doesn't
depend on them (docs, the next review).

**Fan-out reviews.** For a stage close or a release, run independent fresh-context
reviewers in parallel, one per area, each told to be skeptical, read-only, and to report
severity / file:line / concrete scenario / confidence plus a score:

| Reviewer | Scope |
|---|---|
| Runtime | `shipyard-runtime`: post-fork code, `unsafe`, namespaces and mounts, seccomp, cgroups, the monitor and its protocol, fd passing, PID reuse |
| Engine | `shipyard-engine`: records, lock order, crash consistency at every kill point, restart/recovery, health, events, name resolution |
| Network / image / storage | netlink parsing, firewall rules, leases, pasta, DNS (untrusted input), registry, unpacking, volumes |
| CLI / tests / release | exit codes, escapes, prompts, test quality (vacuous, flaky, root-only), CI, packaging, docs accuracy vs code |
| Security (adversarial) | a hostile container, image, registry, local user; rootless limits; terminal injection |

**Brief every sub-agent with the rules that bind it:** read-only unless told otherwise; no
builds in the shared `target/` while another build runs (give it its own
`CARGO_TARGET_DIR`); `timeout` on every `docker run`; never write a device; never
`limactl shell` unread commands; never commit. **Then verify its claims** before you
repeat them: read the code it cites.

**Self-check before "done":** fmt, clippy (macOS, glibc, musl), rustdoc as CI, macOS
tests, both Linux passes, `cargo deny`, mutants with controls, docs and CHANGELOG, F-rows
for what's left, then the report.

**Keep this file alive.** When you learn a lesson the hard way, add it here (section 10)
in the same change, with the one-line why. When the map changes, update section 3.

# Roadmap

Each stage builds directly on the previous one and ends with something runnable and tested.
What each stage contains in detail is in [FEATURES.md](FEATURES.md).

Chapter numbers refer to *The Linux Programming Interface* (TLPI). TLPI predates mature
namespaces and cgroups, so those stages also use the man pages (`namespaces(7)`,
`cgroups(7)`, …) and Michael Kerrisk's LWN series
["Namespaces in operation"](https://lwn.net/Articles/531114/).

## Stages

| # | Stage | Outcome | Reference | Status |
|---|-------|---------|-----------|--------|
| 0 | **Foundation** | Workspace, lints, CI, docs, UI concept | — | done |
| 1 | **Process lifecycle** *(POSIX: runs natively on macOS)* | `shipyard run -- /bin/sh` spawns, waits, forwards signals, stops gracefully, returns the exit code; reaps and cleans up everything the command started | TLPI 6, 20–22, 24–27, 34 | [done](stages/01-process-lifecycle.md) |
| 2 | **Namespaces** | PID, mount, UTS and IPC namespaces (`clone3`); own session; a minimal PID-1 init; `setns` to join a running container. The other namespaces moved to stages 4, 5 and 7 | TLPI 26, 28, 34; `namespaces(7)`, `pid_namespaces(7)`, `setns(2)` | [done](stages/02-namespaces.md) |
| 3 | **Filesystem isolation & OCI bundles** | `pivot_root`, mount propagation, `/proc` `/sys` `/dev`, masked and read-only paths; OCI `config.json` and `create / start / kill / delete / state`; user/group names; domain name and namespaced sysctls | TLPI 8, 14, 18, 44, 55; `mount_namespaces(7)`, `pivot_root(2)`, `mount_setattr(2)`; OCI runtime spec | [done](stages/03-filesystem.md) |
| 4 | **Users & security** | Rootless (user namespaces, UID/GID maps), capabilities, `no_new_privs`, seccomp. Device access control moved to stage 5, with cgroups | TLPI 8, 9, 39; `user_namespaces(7)`, `capabilities(7)`, `seccomp(2)` | [done](stages/04-security.md) |
| 5 | **Resource control** | cgroups v2 limits and cgroup namespaces, OOM detection, live stats, pause/resume, device access control (eBPF), OCI hooks; `shipyard`'s own `stats`/`top`/`pause`/`update` by name come with stage 11 (Docker compatibility) | TLPI 36; `cgroups(7)`; OCI runtime spec (hooks) | [done](stages/05-resources.md) |
| 6 | **Images & storage** | Registry pulls (private registries, resumable), OCI/Docker manifests, safe layer unpacking (fuzzed), content-addressed store shared by concurrent runs, overlayfs (rootless too), volumes. `tag`/`inspect`/`save`… come with stage 12 (build and images), `--mount` with stage 11; the `fuse-overlayfs` fallback later | OCI image & distribution specs; `overlayfs` docs; TLPI 14, 18, 55 | [done](stages/06-images.md) |
| 7 | **Networking** | Network namespaces, bridge + veth over Shipyard's own netlink, IPAM, NAT and published ports with its own nf_tables client, rootless networking with pasta, a DNS server per network, several networks per container, IPv6, reading a container's sockets. `network connect`/`disconnect` and the sockets command came with stage 9 (9.7b2, 9.7c1) | TLPI 56–61; `netlink(7)`, `rtnetlink(7)`, `network_namespaces(7)` | [done](stages/07-networking.md) |
| 8 | **Terminals, I/O & logs** | Per-container monitor, PTYs, attach/detach, `exec`, log files with rotation, `cp`; also `--preserve-fds` and socket activation, other firewalls (F14), and the OCI conformance suite (H5) | TLPI 44, 62–64 | done ([notes](stages/08-terminals.md)) |
| 9 | **Engine state** | On-disk state, locking, crash recovery, restart policies, health checks, events | TLPI 37, 55 | done ([notes](stages/09-engine-state.md)) |
| 10 | **Foundations & the Mac path** | The refactor with no behaviour change (D23: one `take`, one safe-file module with rustix, `doctor` into the engine, attach's and logs' halves into the runtime (`cp`'s: F69), `run/detached.rs` split; F48: done as 10.1a–e, its leftovers F72–F74); stage 9's book chapter; **a minimal macOS path used daily** (D24): Virtualization.framework from Rust, an Alpine guest running `shipyard`, commands forwarded over vsock, the project directory shared, ports on `localhost` | Virtualization.framework, vsock, virtiofs; the kernel probes in the guest | done ([notes](stages/10-foundations.md)) |
| 11 | **Docker compatibility** | The persisted record split from `Config` and the API's types (D10.1d-1, before any record v4); the Docker Engine API subset, socket-activated (over vsock on macOS), with `/shipyard/…` extensions; the CLI a drop-in for everyday `docker` commands; `stats`, `top`, `pause`, live `update`; compose is `docker compose` against the API (`shipyard compose` passes through); `docker compose` and Testcontainers as acceptance tests (D25–D27) | Docker Engine API; TLPI 57 | [done](stages/11-docker-compat.md) |
| 12 | **Build & images** | `shipyard build` driving BuildKit started on demand (D28; in a user-namespaced container per build, D12-2); `push`, `tag`, `login`/`logout` (Docker's config, the Mac's hand-over), image `inspect` / `history` / `save` / `load`, `import`/`export` | BuildKit, OCI image & distribution specs | done ([notes](stages/12-build-images.md)) |
| 13 | **macOS production** | The Mac path made production-grade: Rosetta, the VM's size from the Mac (the balloon tried, not kept: D13.2-2, F168), clock sync after sleep, file sharing beyond the home (virtiofs as it is: D13.4-2), UDP ports and the guest's guard, start on use and stop when idle (an opt-in start at login), the forwarded CLI's wire fixed (D13-1), a stable local signing identity (notarization scripted for stage 15: D13-3); Apple Silicon only (D29) | Virtualization.framework, vsock, virtiofs | **done** ([notes](stages/13-macos.md)) |
| 14 | **Desktop UI** | The [design](design/README.md), built on the stage 11 API; the observed-traffic map first; macOS first, then Linux (D14-1..6) | Tauri 2, Svelte | **done** ([notes](stages/14-ui.md)) |
| 15 | **Hardening & v1.0** | OCI conformance as expected (it runs since stage 8), fuzzing and mutation checks on every parser and state machine, benchmarks, signed reproducible releases, packages (Homebrew, `.deb`, `.rpm`), a docs site; green CI; **the first public release** (D21, D30) | [FEATURES.md §16](FEATURES.md#16-production-quality--ongoing-from-stage-1-finalized-in-stage-15) | in progress |

Each finished stage gets **stage notes** in [`stages/`](stages/): what was built and why,
macOS and Linux differences, the failure modes and how they're handled, known limitations,
and what was carried forward to later stages.

**Ongoing from stage 1:**

- tests for every stage
- the CLI growing with each stage
- the production-quality items in FEATURES.md §16, adopted as soon as they apply
- the [hardening track](#hardening-track), each item from its home stage

Stages 2–9 are Linux-only. Until stage 10's macOS path lands, develop them on macOS inside a Lima VM,
and run their tests in a Linux container with `scripts/test-linux.sh` (see
[DEVELOPMENT.md](DEVELOPMENT.md)).

## Hardening track

What looking back at stages 0–7 says to do differently (decided 2026-10-09). Each item
has a home stage; none waits for stage 15. The first two get more expensive with every
stage, so they come first.

| # | Item | Why | Home |
|---|---|---|---|
| H1 | **An engine crate** (`shipyard-engine`): the logic now in the CLI (`run.rs`, `network.rs`, …) behind a typed API; the CLI a thin client of it, with `--json` output | stage 11's API and stage 14's UI need it as a library; the move grows with every stage | stage 9 (start in stage 8 for what it adds). **Done in 9.1** (no behaviour change, checked against the previous binary): runs, images, volumes, networks and the DNS server are the engine's; the CLI parses, calls and prints. `--json` comes with the container record (9.2/9.4). **9.4a:** `ps --format json` and `inspect` (a JSON array) print the engine's container views (`containers::summaries`, `inspect`). **Done in stage 9.** A container's published ports are bounded by its caller's descriptor limit (the caller binds them; 9.9a refuses early, with a `ulimit -n` hint); the monitor raises its own limit for holds `network connect` adds later (F49, fixed in 10.2c1) |
| H2 | **One state model, versioned:** one container record and state machine, one liveness rule and one recovery path in place of each stage's own lock-and-sweep; a schema version on every on-disk format from its first write, and a test reading the previous version (the first: stage 8's `log.json`, its version refused when unknown) | today's records (run areas, leases, run directories, volume holds, `dns.lock`, cgroup records) each work alone; upgrades rest on `serde` defaults. **Started in 9.2a:** the runtime's `state.json` and `exit` versioned, stage 8's read as v0. **9.2b:** the engine's container record (v1 from its first write, its format in types of its own, pinned by a literal record), its derived state. **9.3b1:** a planned container's `plan.json` (v1; read only by the monitor its own `create` starts, so another version is refused rather than read). **9.3b2a:** a detached container's resources held by its monitor (the run's `flock`s handed over, not re-made) and let go by its `__cleanup`, which records its end in the engine's record; liveness stays one rule, a held lock. **9.3b2b:** a detached container's pasta follows its init (the init's pidfd, its start time checked), never its caller: the launcher and the monitor double-forked, so no caller keeps a child of theirs. **9.4a:** containers by name (`create`, `start`, `run -d`, `ps`, `inspect`, `rm`): the record v2 (the image's digest, `--rm`, the ports bound; v1 read), the exit record v2 (whether the command started); a per-container operation lock (`op.lock`) and a `running` lock its `__cleanup` frees last, so a start or a removal sees one rule; the leftovers of a killed start reconciled by the next start or `rm`; a container's own writable layer, kept until `rm`, and its image pinned. **9.4b1:** `stop`, `kill`, `restart`, `wait` across processes: the attach protocol v2 has control clients (waiters and stoppers, capped apart), and the monitor owns a stop (its signal, the earliest SIGKILL), so a stop outlives its caller. **9.4b2:** `rename`, a running container's DNS name too: leases record their container, and are rewritten in place under an OFD byte-range lock every lease reader takes (the run's `flock` on the inode stays its liveness). **9.9a:** a container stage 8's runtime made, still running, is driven by stage 9's binaries (checked against stage 8's real ones, 21/21; `a_container_stage_8s_runtime_made_is_left_to_it`). **Done in stage 9**; the rule for the next record version is AGENTS §4's (F43, done in 10.1d; F67) | stage 9 |
| H3 | **A threat model in every plan:** who is hostile (the container, a local user, the network, a registry, the image) and what each may never do, written as invariants with a test each; the Murphy review run against the plan too | stage 6's tar escapes and stage 7's router advertisements, open router and lockable leases were all found after building | from stage 8 |
| H4 | **Real hosts in CI:** VM jobs on the distributions users run (Ubuntu with AppArmor, Fedora with SELinux, an older LTS kernel), the probes on each | the test container is one privileged Debian on one kernel; stage 7's last failures appeared only on Ubuntu | stage 8 (one VM job), widened by stage 13 (macOS); Ubuntu 24.04 checked by hand for v0.1 (rootless with the AppArmor restriction on, root bridge networks). **Built in 8.1b:** CI's `apparmor` job (Ubuntu, the restriction on, the installed binaries and profile, a control without it) and `fedora` job (a QEMU/KVM VM, SELinux enforcing, both passes, the terminal probe). *Not yet run on stage 8's code:* CI is manual-only (billing off) and its Ubuntu `check`/`root` jobs are off; stage 8 was checked by `test-linux.sh` and by hand on Ubuntu and Fedora VMs, arm64 only |
| H5 | **Quality tools in CI:** `cargo-mutants` on changed files, the fuzz targets on a schedule, the OCI runtime conformance suite, differential tests of Shipyard's own netlink and nf_tables against `ip` and `nft` | the method works but runs by hand; conformance was planned for stage 14 though the runtime exists since stage 3 | conformance and differential tests in stage 8 (differential tests built in 8.1b: see [Owned components](#owned-components); the conformance suite in 8.7a: `scripts/test-linux.sh --conformance`, gated by `scripts/conformance/expected.txt`, and a CI job, not yet run: amd64 unchecked); mutants and fuzzing **in 9.8b**: the `Quality` workflow (`fuzz`, every target from `scripts/fuzz-targets.sh`; `mutants`, cargo-mutants on a branch's changes, a report), manual until billing is back, its schedule written; locally `scripts/test-linux.sh --fuzz` and `--mutants` |
| H6 | **Errors as a designed surface:** stable error kinds (and codes) in one module per crate; tests assert the kind, not the wording; messages stay free to improve | wording drifted across steps, and tests matching on wording broke CI twice | with H1 (stage 9), and each new error from stage 8. **Started in 9.1:** the engine's `Error` has an `ErrorKind` (`InvalidRequest`, `NotFound`, `InUse`, `NeedsRoot`, `Unsupported`…), other crates' errors are mapped to kinds, warnings have a `WarningKind`, and engine tests assert kinds; the CLI's own tests still match some wording. **9.9a:** one kind per situation (`WrongStatus` of an ended container is `NotRunning`, `MonitorTooOld` `Unsupported`, `BadHoldLabel` `InvalidRequest`, attach's refusal `Busy`). **Done for the engine in stage 9**; the CLI tests' wording matches go as they're next touched |
| H7 | **Performance budgets as tests:** run start, run exit, a cached pull, a network attach; a regression fails | pasta's one-second exit delay was found by accident | stage 8 (built in 8.1b: `tests/budgets.rs`, whose documentation lists every budget). **9.4a:** `ps -a` over 100 containers under 200 ms. **9.4b1:** `stop` of a container that ends on its signal under 1 s, of one that ignores it its timeout and under 2 s more. **9.9a:** `run -d` back at the prompt (`run_d_returns_quickly`) |
| H8 | **Owned components decided explicitly:** for each piece Shipyard writes itself instead of using a crate or a tool (netlink, nf_tables, seccomp, eBPF, tar, DNS), the trade-off recorded here, and a differential test against a reference | each is a large, security-relevant surface Shipyard maintains alone | recorded in stage 8 ([below](#owned-components)); tests with H5 |
| H9 | **Build hygiene:** small debug info (done: `line-tables-only`), CI caches, documented clean-up of build volumes | builds reached 30 GB on disk and 65 GB in Docker before anyone looked | done in stage 7; kept |

## Owned components

What Shipyard writes itself instead of using a crate or a tool (H8), why, and what it's
checked against (8.1b). Each differential test runs a second, independent
implementation on the same input; where they differ, one of them is wrong.

| Component | Why it's Shipyard's own | Checked against | Tests |
|---|---|---|---|
| **netlink** (rtnetlink: links, addresses, routes) | No `ip` binary run as root, no C library, and no netlink crate whose dumps and errors would be another surface to audit; it's small and its parser is bounded and fuzzed | `ip -j`: what Shipyard makes, `ip` reads the same, and the reverse (links, addresses, routes and their types; a route's scope is checked by a unit test of what Shipyard sends) | `route/linux_tests.rs`: `ip_reads_what_shipyard_makes`, `shipyard_reads_what_ip_makes` (root) |
| **nf_tables** (`inet shipyard`) | The same: no `nft` run as root; one batch, atomic, from Rust | `nft`: Shipyard's table lists with nothing unknown, its key rules read as meant, nft rebuilds it from its own listing to the same (a fixed point, the same rules per chain and the same elements), and `nft -j` gives each chain the type, hook, priority and policy, and each set the types and flags, Shipyard made them with. Not byte for byte: nft encodes some rules differently but equivalently | `nft/linux_tests.rs`: `nft_reads_and_rebuilds_shipyards_table`, `nft_json_describes_shipyards_chains_and_sets` (root) |
| **seccomp** (a BPF compiler for Docker's profile) | No libseccomp (a C library): the binary stays static, and the filter's every instruction is Shipyard's | The kernel: the filter loads, and the e2e tests see what it allows and refuses; an interpreter runs it as the kernel would | `security/seccomp/`, `tests/security.rs`. A libseccomp differential: stage 15 (moved from 9, D9.9-5: it belongs with the seccomp tooling's release checks) |
| **eBPF** (device programs) | No libbpf or clang: a small assembler for one kind of program | The kernel's verifier (it loads them), the e2e device tests, and an interpreter as strict as the verifier | `devices/ebpf.rs`, `tests/devices.rs`. A bpftool differential: stage 15 (moved from 9, D9.9-5) |
| **tar** (reading layers) | A layer is untrusted: the reader bounds what an archive can make it hold, and nothing is unpacked by path (the `tar` crate's `unpack` isn't used) | The `tar` crate: generated GNU and ustar archives, and PAX ones, read the same (for a PAX `mtime`, owner or extended attribute, which the crate hands over without applying, the reference is the test's own reading). Shipyard reads an empty number field as 0, as GNU tar does; the crate refuses it | `tar_reader/differential.rs` (proptest); the `tar` fuzz target fuzzes Shipyard's reader alone |
| **tar** (`cp`'s: walking a tree, and extracting a copy) | The tree may be a hostile container's: walked by descriptor (`O_PATH`, never followed; only files and directories opened, by reopening what was checked), each file written at its header's size; the `tar` crate only writes the headers (no `append_path`, which follows paths). Extraction is the layer unpacker's, with a copy's rules (no whiteouts, nothing implied, one top-level name) | GNU `tar`: it extracts `cp`'s stream to the same tree `cp` makes (contents, modes, links, hard links), and `cp` extracts GNU tar's to the same | `tests/runtime.rs`: `cp_streams_agree_with_gnu_tar` (root); `cp/pack.rs`; fuzzed (`cp_unpack`) |
| **Timestamps** (RFC 3339, for CRI logs and `--since`) | A few dozen lines of calendar arithmetic (Howard Hinnant's days-to-date) instead of a time crate: UTC only, no time zones to get wrong | GNU `date`: 200 instants from year 0000 to 9999, the same text | `logs/time.rs`: `the_same_as_date_says` (Linux), plus round trips and known instants |
| **CRI logs** (the writer and reader) | The format is a line per record; writing it is the monitor's job (conmon's), and reading it the engine's: no Go tools, no daemon | kubelet's own test lines (read the same), and the round trip of any text; `crictl` or fluent-bit by hand | `logs/cri.rs`, `logs/writer.rs`, the engine's `logs.rs`; fuzzed (`cri_record`, `logs_dir`) |
| **DNS** (a server per network) | No daemon beside the container (as Podman's aardvark-dns or dnsmasq): one small server, started per network, as nobody | `dig` (BIND's library): names, aliases, case, other types, reverse, TCP, upstream, refusal | `tests/dns.rs`: `dig_reads_the_servers_answers_as_meant` (root) |

**The owned surface is frozen** (D19, 2026-10-15): nothing is added to this table unless the static binary depends on it; a maintained crate or tool is used instead.

What the tests found when first run (8.1b): the set `masquerade` was a keyword of nft's
language, so `nft -f` couldn't read back a listing of Shipyard's table (renamed
`masquerade4`, ruleset 6); and `Rtnetlink::add_route` wrote every route as unicast (it
now writes the route's type).

## Open findings

A review of the whole codebase after stage 7 (2026-10-09; five reviewers, one per area)
found these; each was then re-checked against the code by independent agents, which
corrected several (the severities and findings below are the corrected ones). Stage 7.5
(hardening) fixed the first group, and 7.6 the stage 1–7 findings the review had left (F18's pins, F19–F23, F25–F28); the rest have their stage. Each fix
comes with a test that reproduces the problem first. Remove a row when its fix lands.

| ID | Severity | Finding | Fix | Home |
|---|---|---|---|---|
| F111 | low | Stage 10's fresh review, network (2026-10-18): a DNS TCP reply's write isn't held to the connection's 30 s lifetime (a reader taking a byte every few seconds keeps its slot); a lease is bounded when read (64 KiB) but not when written, so enough `--network-alias`es make a lease every reader refuses (names unresolved, taken by another); F65's fallback leaves out every bridge, so a host whose uplink is a bridge (`br0`) isn't closed when the routing record can't be read; interfaces that appear after forwarding was turned on can be routed to and from | the write against the lifetime; the lease refused at its write; the uplink kept; a decision recorded (stage 7's trade-off) (15.1: the TCP reply under the connection's lifetime, the lease's size bounded at its write; the uplink-bridge case decided and recorded) | 15.2 |
| F125 | low | 11.3d1 (2026-10-20): `update --pids-limit -1` (Docker's "no limit"), and the API's `CpuQuota` of `-1`, are refused: the record keeps a process limit as a number, and none there means a start applies the default (F62), so lifting one needs a third value; device rates aren't taken by `update` either (Docker's `update` has none) | Keep "none asked" beside the record (`created.json`'s `default_pids` cleared, as F67's rule has it), then write `pids.max` `max` live (15.1: later, a missing Docker feature, refused cleanly) | after v1.0 |
| F136 | low | 11.4b2b1's Murphy review (2026-10-21): (a) a container whose network another joins, restarted by its policy (or a crash and restart), leaves the joiner on its old, empty network, shown "Up" with nothing said (Docker's too; the user's own `stop`/`restart`/`rm` say it); (b) the host's stop stops both at once, so the joiner may lose its network mid-shutdown; (c) at boot, a joiner with a restart policy whose container won't start fails once and stays down; (d) no rootless end-to-end test of joining a pasta container's network (the test container's rootless runs have no pasta) | (a) the joiner's start keeps the other's init (PID, start time) beside its record; views say "(no network)" when it's changed, and an event; (b) joiners stopped first; (c) such a joiner skipped at boot, said; (d) a pasta-capable test environment (15.1: (a) only (the joiners told in the view and an event); (b)–(d) accepted at v1.0) | 15.2 |
| F156 | low | 12.1's probe (2026-10-23, D12-2): Docker's own `docker build` / `docker buildx` through Shipyard's API don't work: buildx's `docker-container` driver asks for `Privileged: true` (and probes `DeviceRequests`), which the API refuses, and the API has no `/build` (Docker's daemon embeds BuildKit). `shipyard build` is the way to build (D12-2) | the API takes buildx's request for BuildKit's own image and runs it with `shipyard build`'s settings (a user namespace, `SYS_ADMIN` in it), said in `docs/user/api.md`; or `/build` relayed to the engine's BuildKit (15.1: later, a feature: `shipyard build` is the documented way) | after v1.0 |
| F157 | low | 12.2a (D12.2-3, 2026-10-23): `load` refuses an image the archive gives no name (one `save`d by ID, or an OCI layout without names), said, where Docker loads it as a dangling `<none>` image; Shipyard has no dangling images (removing a name that was an image's last removes the image), so `images`, `rmi`, `prune` and the API's `dangling` filter have nothing to show for one | dangling images in the store's index (a record keyed by its ID), listed as `<none>`, pruned by `image prune` (15.1: later, a feature, refused saying what to do) | after v1.0 |
| F159 | low | 12.3: `build` takes no context from standard input or a URL, no `--ssh`, no `--cache-from`/`--cache-to`, no other platform, and no credentials for a private base image (BuildKit's pulls are anonymous) | each through `buildctl`'s own options (a context tar unpacked first; the caller's credentials copied in as the secrets are) (15.1: later, build features) | after v1.0 |
| F165 | low | 12.5's readiness review (image): a `manifest.json` that lists one image twice fails half way (`ENOENT` for the second's manifest, already moved into the store), said as the store's error; a load past its 64 GiB bound is said as a cut archive; `MAX_ENTRIES` is checked after the whole 16 MiB document is parsed (~130 MB at most); `import --change` with many `ENV` pairs is quadratic, and `EXPOSE` ranges multiply (seconds of CPU, the caller's own); the `load_archive` fuzz target can't get past the blobs' digests to the staged manifests and inner indexes | merge entries by manifest; `too large` from the bound; count entries streaming; bound pairs and ports in all; a structured harness that fixes up digests, and a multi-platform seed (15.1: only an archive naming an image twice (merged); the rest accepted at v1.0) | 15.2 |
| F166 | low | 12.5's readiness review (engine, security): `buildkitd`'s readiness wait is bounded by tries (300 × up to 5.1 s, ~25 min), not the 30 s it says; a Ctrl-C while a build's output is loaded or unpacked acts only once it's done (its image named, then exit 130); every build writes and reads its whole image (BuildKit's `docker` exporter, then `load`); a context past 1,000,000 entries is refused, `.dockerignore`d trees counted; the store images a build is given are found by their repository's last part as a substring and kept to the first 256, so a short common name can fill the cap and the image meant be pulled, unsaid (12.5c3's clean-code review); a `FROM --platform=` other than the host's may be given the store's host image (a named context is one manifest; unverified whether BuildKit checks it), a killed build's `layout/` keeps its hard links (and their bytes in `Build Cache`) until that context builds again or a prune, and a state's lock file is never removed, one a context (a CI host's, one a job: 0 bytes each) (12.5c3's Murphy review); as root, the Dockerfile's check and its read, and the context's bind, are by path (a user whose context root builds can swap a directory between them; the builder reads only what every user can); root's ID check reads `/etc/subuid` and `/etc/subgid` alone, not ranges `subid:` in `nsswitch.conf` serves (SSSD, FreeIPA) | a deadline by the clock; a stop hook in the load; only the new layers out (a registry exporter into the store); `.dockerignore` read (F158's fix); walk and bind by descriptor (`run::MadeSource`'s way); ask libsubid (`getsubids`) (15.1: only the readiness deadline (~25 min, meant 30 s) and the store images' substring match; the rest accepted at v1.0) | 15.2 |
| F170 | low | 13.3's Murphy review (2026-10-26): a container made from an amd64 image on a VM that has since lost Rosetta (uninstalled, or its share or registration failed) can't start, said with Rosetta's hint, but its restart policy retries it about once a minute for good (`restart_policy`'s waiter, `system recover` at boot), a `LeftForLater` warning each time; and an amd64 program built against glibc (`apt`, `python3`) under Shipyard's default seccomp profile through Rosetta is untested (13.3's VM test runs a static busybox; this network couldn't pull `debian` for amd64) | the restart policy gives up on a start refused for its image's platform (said once, the container left ended); a VM test with a glibc amd64 image once a network passes the pull (15.1: the VM test only; the endless retry is every restart's rule, accepted) | 15.3 |
| F171 | low | 13.4 (2026-10-26, D13.4-3): on a Mac, a bind of Docker's socket (`-v /var/run/docker.sock:…`, Testcontainers' Ryuk, a CI agent's Docker-in-Docker) is refused ("mounts denied"): the guest has no `/var/run/docker.sock`, and the Mac's `docker.sock` is in the VM's directory, hidden; Docker Desktop gives such a bind the VM's own socket. Ryuk needs `TESTCONTAINERS_RYUK_DISABLED=true` | bind the guest's API socket (a directory of its own, the service kept up while a container holds it) for `/var/run/docker.sock` and the `DOCKER_HOST` path (15.1 (asked): fixed, so D30's Testcontainers holds as written: `/var/run/docker.sock` in the guest reaches the API, with a VM test) | 15.2 |
| F174 | low | 13.5a's verify (2026-10-26): a rootless `shipyard build` whose process isn't in a delegated cgroup (no systemd user scope: a login without `systemd-run --user`, an SSH session on some hosts) fails at its first `RUN` with BuildKit's own `runc run failed: no cgroup mount found in mountinfo`, after the run's earlier warning that the cgroup mount is left out; nothing ties the two | refuse such a build up front, naming the fix (`systemd-run --user --scope shipyard build …`), or run BuildKit's `runc` without cgroups there (`--no-cgroups`-like) if BuildKit allows it | 15.2 |
| F175 | low | 13.5b's plan review (2026-10-26): the macOS guest's firewall refuses only published ports from its NAT link (`outside`): a container on `--network host` listening in the guest, or a port published on one of the guest's own addresses (its holder's socket: a TCP reset, UDP queued unread), is still reached on 192.168.64.x by other VMs on the Mac's network; a container reaches the Mac's own services at 192.168.64.1 (Docker Desktop's `host.docker.internal`, kept); and, Linux being a weak host, another VM on that network that routes a bridge's subnet through the guest reaches the guest's own services on a gateway address (its DNS servers) (13.5b's Murphy review) | an input rule in the guest, `iifname @outside ct state new drop`, letting DHCP's replies (udp 67→68) and IPv6's neighbour discovery through, with a VM test | 15.2 |
| F178 | low | 13.6b2 (2026-10-27): a Mac has no install path but a checkout: the guest is found beside a `shipyard` built in the tree (`target/guest`) or in the VM's directory, and nothing puts it there for an installed binary; a login start that finds nothing running after its boot, or a VM kept up only by containers waiting to restart, isn't said in `host.log`; and the login start rests on launchd's `RunAtLoad`, checked by a `bootout`/`bootstrap` in the VM tests, not yet through a real logout and reboot (the milestone's); a guest from before 13.6b2 never updates `comes-back.json`, so an old count above 0 boots the VM at each login; a running `on-failure` container that exits 0 at the host's stop is counted once (one login boot for nothing, corrected at the next stop); a `docker` connection refused for the budget gets no 503 (13.6b2's Murphy review) | an install step (the guest beside the binary, `vm install` copying it); the two log lines; the count cleared when the guest is older; the milestone's real logout and reboot (15.1: the install place, with the packages (15.5); the rest accepted at v1.0) | 15.5 |
| F180 | low | Stage 13's whole-stage Murphy review (2026-10-27): (a) since 13.5b PID 1 holds the agent's port, so the host's stop ask is taken with no agent behind it: a hung agent's guest is waited for the stop's whole bound (90 s, a repair's while checking) before it's forced, never at once; (b) a `network.json` adopted as v2 can't be read by a 0.4 host (documented: remove it), and the card's directory is told by its device number, which a macOS update renumbering the Data volume would change (one more NAT lease, logged as a copy: unverified); (c) the local signing key's partition list lets any of the user's processes sign as `Shipyard Local Signing` without a prompt, so the folder permission macOS keyed to it can be carried to another binary (the launchd VM already reads those folders for any of the user's processes through `docker.sock`: said in SECURITY.md); (d) Docker's API through an older guest (a 0.4 VM still running under a 0.5 host) binds the guest's own `/tmp`, as the CLI no longer can. The close's fresh reviews: (e) `vm install` checks the VM is stopped only before its seconds of unload and bootstrap: a host a command spawns meanwhile leaves `docker.sock` to launchd, whose hosts are then refused (503) until it stops; (f) a forwarded TCP connection whose Mac client half-closed is cut 10 minutes after that, bytes still coming (`relay::LINGER_BOUND`, 13.6a's bound on a side that never closes); (g) `--platform linux/amd64/v3` matches the variant-less `linux/amd64` and pulls whichever amd64 build the index lists first, said nowhere; (h) in an agent's first minute, a guest clock more than 60 s ahead of the Mac's refuses fresh commands as waited too long (it clears after the minute); (i) Rosetta is registered with `C` (Apple's `--credentials yes`): a set-user-ID amd64 program runs the interpreter with its credentials, and whether Rosetta honours `AT_SECURE` is Apple's, unchecked; (j) a container given `SYS_ADMIN`, `--privileged` or a device can mount a share whole or uncover the VM's files (said in SECURITY.md; Docker Desktop's parity). | (a) wait about 5 s for the stop's `Exited` and force at once without it, keeping a told PID 1's ordered stop; (b) a one-way note kept; (c) a key ACL for `codesign` alone, if macOS keeps the requirement; (d) the API's relay refusing another build's guest (`booted.json`); (e) the status asked again after the bootstrap; (f) no bound while bytes move; (g) the asked variant kept, or one this host doesn't know refused; (h) stamps judged against the agent's start; (i) checked against Apple's docs, `C` dropped if no image needs it; (j) a mount of `virtiofs` refused in a container on a Mac, or a warning at create. (15.1: (e) and (f) only; the rest accepted at v1.0 ((g) is safe: `Platform::fitness` never takes a newer variant)) | 15.2 |
| F182 | low | 13.8c's readiness review (Mac host, 2026-10-27): one failed or slow `launchctl print` (`launchd::loaded`) makes the CLI spawn its own host while the agent is loaded; that host leaves Docker's socket to launchd, whose own host then finds the lock taken and answers every `docker` connection 503 until the first idles out | when the plist is installed, take a failed `print` as loaded and kick; or a launchd host that finds the lock held hands its clients to the running one | 15.2 |
| F183 | low | 13.8c's readiness review (Mac host): a launchd start's wait runs `launchctl print` on each 100 ms pass (`client::connect_watching`, `client/host.rs`'s `ended`), hundreds a boot, more with parallel CLIs; `taken_fresh` takes a mark dated in the future as fresh (a clock set back); `vm set` replaces a newer or unreadable `vm.json` without saying so | ask `ended` at most once a second; a future mark is stale; `vm set` refuses a newer `vm.json` (15.1: only `vm set` over a newer `vm.json` (H2: refused); the rest accepted at v1.0) | 15.2 |
| F185 | low | 13.8c's readiness review (UDP): a forwarded UDP port's new peer costs the guest a lease-store scan (`recheck(true)` in `send_on`), and 256 peers evict the oldest: a local process spraying datagrams from random ports evicts real peers and burns guest CPU; the relay's `Guest::join` blocks its stop for its bounded ~35 s | use the cached address for a new peer within `RECHECK`; a rate for new peers; a join on a thread of its own (15.1: only the scan per new peer (the address cached within `RECHECK`); eviction at the bound accepted) | 15.2 |
| F186 | low | 13.8c's readiness review (guest): the clock follows any stamped request back without bound after the agent's first minute (a buggy Mac caller sets it back days); `fold` lowercases rather than case-folds (APFS takes `ſ` for `s`: the mount-ID check still holds); a failed agent spawn at boot powers the VM off (`init.rs`'s `start_agent()?`); `musl-tools` and the `rust:1-trixie` base of the guest's build aren't pinned | bound a step back (or two polls agreeing); Unicode case folding; the agent's spawn retried; pin the build image by digest (15.1: the guest's build image and `musl-tools` pinned, with the reproducible builds (15.5); the rest accepted at v1.0) | 15.5 |
| F187 | low | 13.8c's readiness review (platforms, tests): a variant asked of another platform is dropped (`--platform linux/amd64/v3` read as `linux/amd64`); an image stored for another platform is pulled again under `missing` and its pull's failure says nothing of it; `comes_back` counts an `on-failure` container up as one that ends 143; the Rosetta VM test skips unless `SHIPYARD_TEST_ROSETTA=required`, by `doctor`'s wording; `an_accepted_connection_is_blocking` has no control | keep the asked variant; the other-platform error kept and said with a failed pull; the recorded exit; `verify.sh --vm` sets the variable on a Mac with Rosetta; a control that a plain accept is non-blocking (15.1: the tests only; the variant item is F180(g)'s, accepted) | 15.3 |
| F192 | low | 14.4b's Murphy review (desktop app): the API's pull says a layer once it's fetched and nothing while a big layer downloads or unpacks, so the Run sheet's progress stands still for minutes on a large image, and its client gives up after 10 quiet minutes (`run::PULL_QUIET`) while the engine's pull may go on | the engine's pull reports each layer's bytes as they come (Docker's `progressDetail`), the API passes them on; then the quiet bound can shrink | 15.2 |
| F193 | low | The terminal tests failed under a full Linux pass's load, never alone (14.5, 14.6a, 14.6b's verifies). 14.8a: `a_stalled_terminal_doesnt_keep_shipyard_after_a_stop_request` slept a fixed 3 s after its XOFF: it now waits for its conditions (the small output's `sh` gone; the large one's `tr` there, its `wchar` still), 10/10 as root and 5/5 as tester under every CPU busy. Left: `a_terminal_container_run_is_raw_and_detaches` (root: Ctrl-C sent right after `stty size`'s answer, `caught-int` never seen in 20 s), not reproduced: 12/12 under every CPU busy, 16/16 alone; 14.7's full verify passed both. Its cause unknown: a full pass's load is I/O and hundreds of containers, not CPU alone. 15.0: GitHub's rootless runner lost the stalled test once in two runs (2026-10-29, run 40933453265: the 100000-byte case's `sh` never ended in 60 s after the XOFF, its output not held by the buffers in between), so it isn't this machine's | reproduce under a full pass's own load (a second test binary beside it), the transcript and the shell's `/proc/PID/status` (SigCgt, SigBlk) kept at the failure; fix the cause | 15.3 |
| F194 | low | 14.6b's Murphy review: `/shipyard/volumes` walks every volume against one 3 s deadline, in the list's order (`volumes/listing.rs`): one large volume early in the order leaves every later one unwalked (size 0, "at least") at every read, as long as it's there | a share of the deadline each, or each read starting where the last one stopped | 15.2 |
| F195 | low | 14.6b's Murphy review: a volume's deletion removes its whole tree inside the request (`volumes::remove`: moved to the trash under the store's lock, then deleted before answering). A volume of millions of files outlasts the client's `ACTION_WAIT` (130 s): the window says the deletion timed out though the volume is gone, and "Delete N unused" stops at it, the rest left | answer once the volume is in the trash and delete its tree after (the next delete or prune sweeps a leftover already), or take a timeout after the trash step as done | 15.2 |
| F196 | low | 14.7's Murphy review: the desktop client takes any failed connect for "nothing answers" (`ErrorKind::Unreachable`), `EAGAIN` from a live service's full backlog among them, and on Linux runs `system service --ensure`, whose blocking `connect` (`api_service::answers`) then waits unbounded until the app's 20 s bound kills it: the window says a start timed out while the service runs | only `ENOENT` and `ECONNREFUSED` start a service; `answers` bounded | 15.2 |
| F198 | low | 14.8a's whole-stage Murphy review: on Linux the API service the desktop app starts (`system service --ensure`) inherits the app's session (its resource limits, its `PATH`) until it ends idle, so a container made through it meanwhile, from the app or a terminal, gets that session's limits (a desktop's `RLIMIT_RTTIME`), and a pull's credential helper is looked for on the GUI's `PATH`. Said in `desktop.md`; an exec given `--process` above its caller's limits is still refused (`within_own` covers the plain exec) | the service started with set limits and environment (as systemd's unit gives it), whoever starts it | 15.2 |
| F199 | low | 15.0's first full Fedora VM run in CI (2026-10-30, run 41058343828), the job rebuilt for stages 8–14: two failures left besides F200's, neither seen in the local Fedora 44 VM: the build tests as tester all fail making their scratch directory in `/var/tmp` (`registry.rs`'s `Scratch::new`: `EACCES`; the root build tests pass); `failing_checks_make_it_unhealthy` (root) sees no `unhealthy` in time, as `a_passing_check_is_healthy` didn't once before (CI's VM is slow: nested KVM) | from the nightly job's logs (Quality's `fedora`, D15-5): the build tests' directory (SELinux labels, `TMPDIR` under `systemd-run --user`); the health checks' waits on a slow host | 15.3 |
| F200 | medium | 15.0's CI (2026-10-30: Fedora's VM, both passes, and the rootless Ubuntu runner once in five runs): `docker start -a` of a container that has ended printed nothing, exit 0 (`start_attached_and_attach_are_docker_s`'s last step; 40/40 locally, idle and with every CPU busy). The attach is registered before its 101 (`attach.rs`), so the likely way: the start's handover to the waiting attach fails (`an attach wasn't let in at its container's start`, a debug line), the attach falls back to looking, and a command that ends at once (`echo`) has ended again by then: `ended() != before`, nothing joined, its output lost to the client | the test's service now logs at debug and prints its log when it fails: the next CI hit says why the handover failed; fix that cause (a handover must never fail silently for a start the attach waited for) | 15.2 |
| F202 | low | 15.2a's Murphy review: the API's stop gives up its settle wait when its client goes (F123), but not its wait for the container's operation lock (`lock_again`, up to 120 s): behind another long command, an abandoned stop holds the service up to that bound | wait for the operation lock in slices that ask `gone` too | 15 |
| F203 | low | 15.2a's Murphy review: F130's networks are kept for what the boot will start until `system recover` has run this boot; on a root host without the boot units (`contrib/systemd`, which `doctor` warns of), that's until the container is started or removed. The refusal says so, and how to clear it | none needed beyond the message; if hosts without the units turn out common, release the networks after a while, or when `doctor` sees no unit | 15 |
| F204 | low | 15.2b1's Murphy review: a PID-joined container's init ends its processes within 2 s when its monitor dies (`kill_leftovers`' deadline); one that outlasts it (a fork bomb, a process stuck in `D`) becomes the joined container's init's, and stays until the container is removed (its cgroup goes then) | when the monitor is gone, the init also writes its cgroup's `cgroup.kill` (it would need its cgroup reachable and writable after confining itself) | 15 |
| F205 | low | 15.2b2's Murphy review: a container joining another's PID namespace is entered before its start's go by a clone whose memory is the host's user namespace's (it re-execs in the joined one after the go): rootless, what the runtime writes in its `/proc` before the go fails there (`--oom-score-adj` with `--pid container:` refused by the kernel, EACCES); the network is set up after (the engine's paths), so pasta is unaffected. And a debugger (`--cap-add SYS_PTRACE`) that joins a PID namespace while another container's set-up is under way there could trace that set-up: the refusal looks at the namespace as each join starts, not at one starting beside it | the init re-execs before the go (a second handshake after its exec); a lock per PID namespace held across a join's set-up | 15 |
| F206 | low | 15.2b2's Murphy review: Shipyard's own isolation view (`/shipyard/containers/{id}/isolation`, the desktop app's) says a joined PID namespace is the container's own (`own`: not the service's), as it does a joined network or IPC namespace | say a joined namespace as the other container's (its ID), for all three kinds | 15 |

### Accepted at v1.0 (15.1, 2026-10-31)

Triaged in 15.1 (D15-3): none of these lets a user or a hostile container, image, registry or local user reach a wrong result, a leak or a hang they couldn't already cause. Each keeps its finding as recorded, with why it's accepted. Two Dependabot alerts are accepted with them: `glib` 0.18 (moderate, GHSA-wrw7-89jp-8q8g: it comes through Tauri's GTK on Linux, and Shipyard never uses `VariantStrIter`; 0.20 fixes it once Tauri moves) and `deepmerge` 4.3.1 (low, GHSA-gcqx-w48m-w2h4: the page build's tool, never fed untrusted input; no fixed release yet).

| ID | Severity | Finding | Why accepted |
|---|---|---|---|
| F24 | low | An OCI bind source is checked at load but resolved again by path at start: swapped for a symlink in between, another host path is bound (runc shares it) | An OCI bind source swapped after its check needs write access above the bundle's own source: the bundle's author, already the caller (runc has the same gap). |
| F29 | low | For a program embedding the runtime library (narrowed by 11.2c3b, which restores the subreaper setting on drop and documents the main-thread and SIGPIPE rules): while a `Supervisor` runs a command it reaps every child of the process, not only the command's tree (documented on `Supervisor`; nothing long-lived embeds it: the API runs exec as a child `shipyard exec`); a leftover still alive after `kill_leftovers`' 2 s stays its child, unreaped once the subreaper is off | The subreaper setting is put back on drop; reaping every child happens only in short-lived processes. |
| F47 | low | 9.9a clean-code review (D1, kept as is): `kill` with SIGKILL or the stop signal marks the container stopped by its user before signalling (so its `__cleanup` never restarts what its user killed); a container that ends on its own in the instant between `kill`'s status check and its signal stays down (the mark also cleared a pending restart). Marking only after the signal would reopen the worse race | The reverse order (signal, then mark) opens a worse race; a known limitation. |
| F51 | low | 9.9b's manual milestone: a rootless container made from an SSH login, where the user has no delegated cgroup subtree, has its monitor in the session's scope: a shutdown ends it before the user unit's `system stop` (it comes back at boot, its end settled, but without its stop signal). Root's are moved out (`shipyard/monitors`, 9.9b) | Documented workaround; a real fix needs systemd over D-Bus, which Shipyard doesn't use. |
| F59 | low | The v0.2 review (network): the DNS server runs as, and leases are shared with, uid/gid 65534, which every other `nobody` process on the host has: one can hold a lease's lock shared (its sweep never reclaims it) and read containers' names and addresses | Defence in depth: abusing it needs control of a process already running as `nobody` (borderline, asked). |
| F62 | low | The v0.2 review (security): the default network can't have `--isolate-host`, so its containers reach the host's services and the cloud's metadata (documented in `SECURITY.md`). (The default process limit: 2,048, in 11.3b1, D11.3-3.) | Refused and documented (SECURITY.md), as Docker's default bridge; `--isolate-host` on a network of your own. |
| F67 | low | Found by 10.1d's Murphy review (2026-10-16), F43's remainder: 10.1d's rule (a record is rewritten at the version it was read at unless a field set in it needs a newer one) protects fields a user asks for, not one the engine itself sets on a running container (as `stopped_by_user`, `restart_count`, `boot_id`, `next_start` were for version 3): such a field in version 4 would make every routine rewrite (`stop`, a start, the restart policy, the recovery pass) write 4, which an older binary's monitor still running refuses, and its container's end goes unrecorded | No trigger until a record v4; the rule is written in AGENTS.md section 4. |
| F69 | low | Left by 10.1e4a (2026-10-16, D10.1e-2): the `shipyard-runtime` binary still uses the engine for `cp` (by runtime ID: `cp_output.rs`, `cp::helper_entry_point`), so `deny.toml` keeps its exception for `shipyard-cli`'s runtime binary; attach and logs are the runtime's own since 10.1e4a, but the shared `logs_output::printed` still names the engine's `Error` to find a gone reader inside it (`shipyard logs`'s path), so a binary of its own needs that check split too. Moving `cp` would pull `shipyard-image` and `tar` (with `ureq`, `flate2`, `sha2`) into the runtime crate, and dropping the engine needs the binary in a package of its own (dependencies are per package) | Packaging-internal; no user sees it. |
| F71 | low | 10.1e4a's Murphy review (2026-10-16), from before stage 10: an attach session's own resize watch (`process::ResizeWatch`, 10.2f2) is one per process, as every signal watch is (the runtime's `Signals`): a second terminal session at once (threads of an API or UI that embeds the engine) fails to start, where before 10.2f2 it overwrote the first's pipe and the first's drop left the second without resizes. The CLI never runs two | Only the CLI's terminal session runs a resize watch; the API and the desktop app never run two sessions in one process. |
| F76 | low | 10.1's whole-step clean-code review (2026-10-16): seams stage 11's API will meet. `shipyard-sys`'s locks say "deadline passed" two ways (`lock_until`'s `EWOULDBLOCK`, `try_lock_until`'s `Ok(false)`) and its errors are `nix::Result` in some modules, `io::Result` in others; "settle" means three things (`attach::client::settle`, `containers::settled`, the recovery pass's); a run's end is two types (`run::Outcome`, `run::Finished`, beside `exec::Ran` and `attach::Settled`); `report::Collect::events()` returns messages; `health::Health`'s and `events::Event`'s writer-side methods are public; `cp::copy_in`/`copy_out` by runtime ID are public beside `copy_by_name` (named unlike `to_container`, `of_container`, `in_container`); the CLI's `*_output` modules beside its command groups, with the time parsers in `logs_output` | Internal naming and visibility. |
| F77 | low | Left by 10.2a's F53 fix (2026-10-16): the port-reservation lock is a file in the caller's runtime directory, but without one (a rootless run with no `XDG_RUNTIME_DIR`, a root whose `/run` isn't writable) it's the abstract socket name `@shipyard/port-reservations` any local user can hold, as before; and two users' Shipyards (or a v0.2.0 restart waiter beside a newer one) don't share a lock, so two asking for one port in `TIME_WAIT` at the same instant can both hold it (Linux shares `SO_REUSEADDR` across users), root's DNAT then shadowing the user's pasta port | Needs a host with no runtime directory or two users racing one TIME_WAIT port; at worst a port said in use; documented. |
| F79 | low | Seen once by 10.2a's verification (2026-10-16), macOS, during `verify.sh`'s full run: `run.rs`'s `shipyard_dies_from_sigint_when_the_command_did` (its assertion at `tests/run.rs:353`) and `signals.rs`'s `signals_sent_to_shipyard_are_forwarded` (a 30 s wait in `shipyard-test-support`'s `lib.rs:691`) failed together; each passed 3/3 alone right after, and the whole macOS suite passed again (1,076 / 0). Code 10.2a didn't touch **Not reproduced in 50 runs under load (10.2e2, 2026-10-17)** | Seen once, not reproduced in 50 runs under load: closed. |
| F87 | low | After F63's fix (10.2e1, 2026-10-17): the root `image` tests failed once in 30 runs (10 under load, 10 alone, 10 under load: 9+10+10), where they had failed 1 in 3 to 5; the one failure's cause wasn't captured (that loop kept only pass or fail). F63's row had noted a second, unconfirmed cause in `a_missing_working_directory_is_made` **Not reproduced in 50 runs under load (10.2e2, 2026-10-17)** | Not reproduced in 50 runs: closed. |
| F89 | low | Left from F74 (10.2f3, 2026-10-17, D10.2-20: only the row's named modules were split): 25 library modules are still past ≈1,000 lines, most with their tests inside; the largest are `network/store.rs` (3,415), `container/lifecycle.rs` (2,946), `image/unpack.rs` (2,930), `oci/mod.rs` (2,457), `process/failure.rs` (2,022), `network/pasta.rs` (2,008), `isolation.rs` (1,993) | Internal size (43 files past 1,000 lines by 15.1); split when next changed. |
| F107 | low | Stage 10's whole-stage clean-code review (2026-10-18): helpers that grew a copy per step (the versioned read: one `shipyard_sys::versioned::read` since 11.1, for the six formats alike): "128 + signal" from an `ExitStatus` in 4 places; three doctor `Level` enums (engine, `shipyard-vm`, `shipyard-runtime`'s binary); the guest's process names in two crates; `shipyard-vm`'s `pub` items with no outside user; a `Flock` held outside `shipyard_sys::lock` in seven places (`image`, `network`, `events`; 12.2a's load staging and its sweep in `image/archive/load.rs`) and raw `libc::flock` in the DNS server | Internal duplication. |
| F113 | low | 10.4b's Murphy review (2026-10-18): after a repair of the Mac VM's data disk (F104's fix), what `e2fsck` couldn't place is in `lost+found` on that disk, which nothing reaches from the Mac (a bind of `/var/lib/shipyard` is refused: outside the home); the repair is said only by the guest's `doctor` (ten boots), not by `vm status` | The guest's `doctor` reports a repair's `lost+found` count; copying files out is a feature. |
| F115 | low | 11.2a's Murphy review (2026-10-18): release builds abort on a panic (`panic = "abort"`), so one engine call that panics in the API service ends the service and every call under way in it, where a debug build answers 500 | A panic aborts the service like a SIGKILL, which every kill point settles; socket activation starts it again. |
| F122 | low | 11.2c3b's Murphy review (2026-10-19): F88's `die` is said before its mark goes (`run/cleanup.rs`, `run/take.rs`, `run/recover.rs`), so a process killed between the two (microseconds) leaves a mark the next settling says again: two `die`s, never none. Said-then-unmarked is the choice (a lost end is worse than a repeated one) | At worst a repeated `die` in a microsecond window, never a lost one: the documented trade-off. |
| F124 | low | 11.3c2's verify (2026-10-20): `firewall.rs`' `a_port_a_killed_run_left_on_another_network_is_reclaimed` failed once as root ("the container of shipyard N never got its network", `shipyard-test-support/src/net.rs:121`) in about 14 runs: alone 10/10, its whole file 3/3 after. Nothing 11.3c2 changed is on its path; the wait for a run's network (a bound in the helper) may be too short beside other root tests' bridges | A test flake; reopened if CI shows it. |
| F127 | low | 11.3d2's design (D11.3d-2) and Murphy review (2026-10-20): `top` reads `/proc` itself in `ps -ef`'s columns, so `ps`'s options (`top c -o pid,comm`, the API's `ps_args` other than `-ef`) are refused and `STIME` is UTC; a PID that ends and is reused between the cgroup's list and its `/proc` read shows as one of the container's (what its user could read anyway); every row is kept in memory (a cmdline up to 64 KiB each). On the Mac, `stats` refreshes in place only when stdin is a terminal too (the guest gets a terminal for both or neither) | A documented subset of `ps`; other options are refused loudly. |
| F132 | low | 11.4a2b and 11.4b1's verify (2026-10-21), measured in 11.5a (2026-10-22): the macOS guest's kernel sometimes found its memory corrupted a few seconds after boot, then oopsed and hung: 6 crashes in 227 boots with four VMs at once on a Mac already swapping, 0 in 157 alone. 13.8a (2026-10-27, `guest-crash-rate.sh`, the guest's kernel 6.18.58, macOS 27.0.1): **not reproduced**, 0 kernel crashes in 360 boots (four at once with 4 GiB each, the Mac swapping, 0/119; four at once with 1 GiB, 0/114; two at once with 4 GiB, the VM tests' setting, 0/120; the rest the registry's failed pulls, not counted): a rate still 11.5a's would give that about one time in 70. The VM tests' every command is bounded now (`vm_support::COMMAND_LIMIT`, `finish_within`), so a guest that dies fails its test | 0 guest kernel crashes in 360 boots (13.8a); every VM-test command is bounded. |
| F135 | low | 11.4b2a's reviews (2026-10-21): (a) a monitor's failure reaches `create` as `ContainerError::Setup(String)` (`Answer::Failed`), so the join tests match its words, not its kind (H6); (b) an enterer killed after its clone and before its word leaves the new container's child, the monitor's own, a zombie holding the clean image's descriptors until the monitor ends (invisible to the other container: its PID namespace is new); (c) the join test compares namespace links only, not a SysV segment or a listener seen across; (d) rootless, an OCI path join isn't refused up front (the kernel lets one user join its own namespaces) | Error-kind and test gaps, a microsecond zombie; a user joins only their own namespaces. |
| F137 | low | 11.4b2b2's reviews (2026-10-21): (a) the shared `/dev/shm`'s own rules (`nosuid,nodev,noexec`, private: `rootfs.rs` `confine_shared`) and the tmpfs check have no end-to-end test: only a privileged other container (`SYS_ADMIN` and a `mount` tool) could change its own; (b) the helper's 30 s bound has no test (a lookup that never ends needs a FUSE mount in the other); (c) a container's error for its `/dev/shm` clone is `MountDev` for every cause (`setns` refused, not a tmpfs) | Test and error-kind gaps only. |
| F141 | low | 11.6a2's Murphy review (2026-10-23): `POST /auth` against a registry whose `/v2/` redirects to another host that answers 401 says `500 signing in: …`, not Docker's `401 unauthorized` (no credentials go to another host, so no sign-in is tried there); and a slow or failing registry holds a sign-in for minutes (the pull's 4 attempts, each up to its connect and response bounds) | A rare redirect case and its error mapping; bounded by the retry rule. |
| F142 | low | 11.6b's API review (2026-10-23): the service's stop waits for its engine calls, but a call that never ends quickly (a big pull, `stop?t=300`) meets systemd's stop timeout (90 s by default: SIGKILL, which the stores' crash safety absorbs); a stop can drop the answer to a call that has just succeeded (its connection's task isn't drained); a request's body has no time bound (a same-user client that stalls one holds the service up); idle keep-alive connections close after 30 s (hyper's header timeout), where Docker's have none | Only the socket's own user can connect (`SO_PEERCRED`); the stores' crash safety absorbs a kill. Merged with F149. |
| F149 | low | 11.7's API review (2026-10-23), its (a)-(c) fixed in 12.5a: (d) no cap on the API service's connections, and a request whose body stalls after its headers holds its connection (only the socket's owner can connect, `SO_PEERCRED`: its own programs alone could flood or stall it; D12.5-2) | Only the socket's own user can connect (D12.5-2). Merged with F142. |
| F153 | low | 11.7's bind-source fix (D11.7-4), what it leaves: a source first made by a later start (missing at the create, made at a start) is bound by descriptor at that start only (its `created.json` is written once); and a made source's directory, the deepest one that was there, is opened as Docker resolves it, so a container that can replace that directory itself (not what's below it) still redirects it, as with Docker | A link above a made source is followed, as Docker resolves it; reaching it needs write access to its grandparent. |
| F154 | low | 11.7's CLI/tests and security reviews (2026-10-23): (a) the dormant CI (D31) would run stage 11's Docker-driven tests against the runner's own Docker CLI 28, not the pinned 24.0.9 (`scripts/test-image/Dockerfile`); (b) on Linux, no API, compose or Testcontainers test sends traffic through a port the API published (the CLI's port tests cover the same `RunRequest`); (c) `api_request`'s fuzzing reaches create, network and exec bodies only as valid JSON, with no dictionary, and never the upgrade headers, `parse_time`, `DetachKeys::parse` or the exec start body; (d) fields the API's create doesn't know are ignored, as Docker's are: a future restrictive field would weaken a container unsaid | Test and fuzz quality. |
| F158 | low | 12.3 (D12.3-3, 2026-10-24): as root, a build's context must be readable to every user (the builder's root is another user, IDs from 2100000000), and every file is checked, `.dockerignore` or not (BuildKit copies an unreadable file empty, silently: checked first); a context with one private file anywhere is refused | A loud refusal with its fix, never a wrong image; an ID-mapped mount is later work. |
| F160 | low | 12.4: a push uploads every blob the registry lacks whole (no cross-repository mount, `?mount=&from=`, and no resume of a broken upload: it starts over), and pushes one platform's manifest (Shipyard keeps no multi-platform index of its own); two sign-ins at once (`shipyard login` beside `docker login`) can lose one, each rewriting Docker's config whole (as two `docker login`s can); the guard against flattening a multi-platform tag covers only the index the image came from (a tag CI has since moved to another index is replaced, as Docker's classic store does), and a stalled API client's progress holds the upload threads (as a pull's) (12.4's Murphy review) | Performance only; the rest behaves as Docker's. |
| F161 | low | 12.5c1 (the stage close's security review): every build as root runs as one host ID range (from 2100000000), so per-user kernel accounting (`RLIMIT_NPROC`, counted by real UID) is shared by every root build at once: a fork-heavy `RUN` of one project can starve another's (a denial of service only; their files stay apart, by mount namespace and each state's 0700) | The builder already gets the default 2,048-process limit. |
| F163 | low | 12.5c2 (the stage-12 book's chapter 54): a build refused because its Dockerfile is a link leading out of the context (`read_within`, 12.5c1) still leaves an empty `state-<hex>/` and its `.lock`: the inputs are copied after the context's lock and places are made, so `system df` counts an empty cache until a prune | Leaves an empty state and a lock, which the unused-state sweep clears. |
| F168 | low | 13.1's probe and 13.2 (2026-10-26, D13.2-2): the macOS VM's memory isn't given back to macOS while it runs: Virtualization.framework's traditional balloon (`VZVirtioTraditionalMemoryBalloonDevice`) inflated in the guest (MemFree 3.9 GB → 0.74 GB at a 1 GiB target) but the VM's footprint never fell (with the guest's `init_on_alloc` it rose by the balloon's size; with `init_on_alloc=0` it stayed), three runs on macOS 27.0.1 while the Mac had memory to spare; under real memory pressure untested (simulating it needs root). The VM holds what its containers touched until it stops | An Apple framework limit, documented; `vm set` is the lever. |
| F169 | low | 13.1's probe and 13.3 (2026-10-26): Rosetta translates each amd64 program at its start (~3 ms an exec in 13.1, against ~0.15 ms native); its translation cache (`rosettad`, which the share carries, and Virtualization.framework's `VZLinuxRosettaCachingOptions`, macOS 14) isn't used, so a build or a script that runs many short amd64 programs pays it each time | Performance (~3 ms an amd64 exec), documented. |
| F172 | low | 13.1's probe and 13.4 (2026-10-26, D13.4-2): making and removing many small files through a share is ~3.6× and ~2.3× OrbStack's time (10,000 files: 2.45 s and 1.15 s against 0.68 s and 0.49 s); reads, stats and large writes are on par. Virtualization.framework's virtiofs has no cache mode or DAX, and the guest's takes no cache options: nothing to tune. The VM test holds 2,000 files under 2 s (H7) | virtiofs performance, measured and documented. |
| F173 | low | 13.4's Murphy review (2026-10-26): ((a), the bind by path, closed in 13.8c: a source in a share is bound by descriptor from its share) (b) a newer `shipyard` CLI meeting a VM that an older one started (before 13.4, still up until its idle stop) lets a `/tmp` path through that the old guest binds as its own (since 13.6a each command says the VM is another build's, from `booted.json`, and goes on); (c) `/Volumes` reaches Time Machine's local snapshots, which may hold old copies of the VM's `data.img` (read-only); (d) `doctor`'s try of a guarded folder may leave an `ls` waiting in the kernel on macOS's prompt until it's answered | (a) closed; (b)–(d) an upgrade window, your own old disk, a docs line for `doctor`'s wait. |
| F176 | low | 13.5b's reviews (2026-10-26): nothing tests end to end that the macOS guest's PID 1 keeps the agent's port across an agent's restart (F108's fix): no unprivileged way to kill the agent from a test; the guest `doctor`'s `agent port` line proves only that commands don't inherit it, and a PID 1 that dropped its copy after the first start would pass every test until the agent died | A test gap; the debug request it suggests would add attack surface. |
| F177 | low | 13.6a's plan review (2026-10-26): the macOS guest's agent takes every connection on a thread of its own with no bound (forwarded ports' and UDP relays' among them), so a flood of connections from the Mac's side (a load test on a published port) can leave no thread for the host's own asks (its boot ID, its stop, a hang-up); and a control connection's signal goes to the command's process group, whose number another process may take between the command's reap and the session's end (a window of microseconds, `ended` set just after the reap) | Bounded since 13.6b2: the host caps connections at 1,024. |
| F179 | low | Stage 13's whole-stage clean-code review (2026-10-27): what grew across its steps wants consolidating: `forwarding.rs` (1,096 lines) is the host's way to the agent as well as its port forwarding (`request`, `output`, `joined`, `ask_boot`, `count_to_keep`); the guest's `agent.rs` (1,406) holds its clock and its API connections beside its dispatch; the kernel command line is written once (`host::cmdline`) and parsed by hand in five places; a refusal becomes words (`VmError::Refused(String)`, a test of words: H6); `forward.rs` carries the guest's boot contract beside the wire; `host::settings` is near clippy's 100 lines; F98's shadowed-port line is worded twice; version constants are named two ways | Internal structure. |
| F181 | low | Stage 13's milestone (2026-10-27): on a Mac, `shipyard build` from a context in a share beyond the home (`$TMPDIR` under `/var/folders`, `/tmp`, `/Volumes`) is refused ("the builder (as root, another user, ID 2100000000) can't enter"): the readability check is skipped on the home's share alone (13.4's review: elsewhere other Mac users' files sit, and their modes in the guest don't say what the builder reads, so an unreadable one would be copied empty), and through virtiofs those modes refuse the builder. Said in `macos.md`; a context in the home works. | Refused and documented, with a working path (a context in the home). |
| F184 | low | 13.8c's readiness review (Mac host): the host's main thread waits on Virtualization.framework's queue with no bound while booting (`machine.rs`'s `on_queue`): a wedged queue leaves the host in `Booting` with a stop recorded and never acted on, until `kill -9`; and a host that fails after binding `host.sock` leaves it, so a later pre-0.5 host can't be stopped by `vm stop` | No trigger has ever wedged the framework's queue. |
| F188 | low | 13.8c (2026-10-27): a host killed, or the Mac's power lost, within the second between the guest's ready mark and the host's next ask, after a command made something on a first boot's disk, loses what it made (the disk is made anew at the next start) | Needs a host kill within ~1 s of a first boot's readiness; the fix is a new wire frame (borderline, asked). |
| F189 | low | 14.2's Murphy review (desktop app): the app reads only the default VM directory (a GUI app has launchd's environment, not `SHIPYARD_VM_DIR`); `desktop.json` keeps a `vm_dir`, but nothing in the window sets it, so a VM run from another directory looks stopped and the app may start a second one in the default place | The app uses the VM's default place; documented. |
| F191 | low | 14.2's verify (2026-10-27), the first VM run on the guest's Linux 6.18.59: `a_build_in_the_home_runs` ended 139 (a SIGSEGV) once, right after BuildKit's layers were pulled, two VMs running at once; 3/3 alone after. Which process crashed isn't known (the test's VM directory, its console, is removed) | One SIGSEGV, never reproduced: folded into F132. |
| F197 | low | 14.8a's whole-stage clean-code review: the desktop app's code drifted across stage 14's steps: `store.ts` (~800 lines, one closure of ~550), `App.svelte` (~480) and the core's `lib.rs` (~730, 31 commands) want splitting (connection, section readers, selection, actions; Rail, Tally, PhasePanel, Settings, StatusBar; containers, sections, stats); two models of a container (`rows` and `MapNode`) from the same `links.containers` (typed `unknown[]`); the `/shipyard/…` answers other than the network view passed through as JSON (isolation, volumes, links: their keys unchecked where the client could type them); rules kept in Rust and TypeScript with nothing tying them (`MEMORY_CHOICES`, the logs' 2,000 lines, `ErrorKind`'s names); the inline confirm written three times and the bulk one twice; one `actionError` for every section; the rail's counts from reads beside the sections' own; `/shipyard/containers/{id}` the one answer with no `version` | Internal structure. |
| F31 | low | Stages 7.5 and 7.6's residuals: link-local IPv6 isn't tied to its container; a store root that's a symlink is only warned about; a root filesystem's own device nodes open where no device program guards; a symlink could steer a volume onto Shipyard's tmpfs covers in `/sys`, `/dev`'s tmpfs, or securityfs, debugfs or the like below a bundle's own recursive `/sys` bind (closed in 9.8a with F36 for mounts, refused by place; a device node steered there by a symlink isn't checked yet); a container that hit its memory limit, then lost a process to the host's OOM killer, reads "out of memory"; `--blkio-weight` doesn't set BFQ's `io.bfq.weight` (runc does); `update --cpu-period` alone clears the quota (runc keeps it); a `delete` of a killed `create` can remove another `--root`'s same-ID cgroup in the moment before its init joins it (that `create` fails) | 15.2a fixed `shipyard-runtime update --cpu-period` alone clearing the quota; the rest (BFQ weights, the other residuals) needs root or a rare race. |
| F150 | low | 11.7's engine review (2026-10-23): (a) a create places its record, then takes its `op.lock` (`run/create.rs`): a `start` in that window runs before its volumes are filled; (b) `restart_pending` looks are zero-wait tries of `restarting`, so the recovery pass (`recover.rs`) and a plain `rm` (and `container prune`) beside another client's look skip or refuse for an instant; (c) `system prune -a` beside a `run` can remove the image between its pull and its pin; (d) `.locks/<name>` and `.fill-locks/<name>` files are never removed (one per anonymous volume ever made); (e) 11.7's Murphy review: a `nocopy` create now takes its volume's fill lock for a moment, so it waits (up to 600 s, and through the API with no client's hang-up to end it) behind another container's fill of that volume; (f) a create without an image (`--rootfs`) takes no fill lock, so one placed while another's first start finishes a fill isn't seen by that start's guard | 15.2a fixed (d), the lock files left behind; (a), (b), (c), (e), (f) are a brief refusal, a bounded wait, or a create a lost race undoes cleanly. |
| F164 | low | 12.5's readiness review (CLI/API): the API's `save` never raises its descriptor limit as the CLI's does (and the units set no `LimitNOFILE`), so `docker save` of ~1000 blobs is refused where `shipyard save` works, and two saves share one budget; a self-bound service's idle end unlinks its name before it serves a last queued connection, so a client's next connection (an exec's start after its create) can reach a new service (`404 No such exec instance`); a broken pipe on `save`/`export`/`build -o -` exits 0, where Docker's CLI dies of SIGPIPE; a malformed handed credential exits 125 for `push`/`pull` | 15.2a showed (a) fixed already (the service raises its own descriptor limit, F114) and tested it through the API; (b) is F149's design (an exec spanning the idle end gets a 404), (c) and (d) have no reach beyond the caller. |

**Direction decisions (2026-10-15):** after the v0.2 reviews (code, security, strategy,
architecture, tests, Rust craft), decided in a grilling session, round by round.

- **D18 — Two users:** macOS developers and Linux hosts. The Linux host's needs are
  served first (stages 10–11), but development and testing happen on macOS, so a
  minimal macOS path comes early (D24).
- **D19 — The product decides what is built; learning decides how.** Every concept is still
  explained (the book, the stage notes), but the next stage is chosen by the users' needs,
  not by TLPI's chapter order. The owned surface is frozen: no new hand-written protocol
  or format unless the static binary depends on it.
- **D20 — "Daemonless" means nothing runs while Shipyard isn't being used.** Never a root
  daemon; one monitor per container; anything else (the API socket, the macOS VM) starts
  on first use and stops when idle. This replaces the stricter "no long-running process"
  reading, which would rule out the Docker API and macOS.
- **D21 — No release and no outside users until v1.0** (no testers either). v0.3–v0.5 are
  internal milestones; the first public release is v1.0.
- **D22 — The learning book stays part of each stage's "done";** stage 9's chapter is
  written in stage 10.
- **D23 — Stage 10 starts with a refactor that changes no behaviour,** checked against the
  v0.2 binary as 9.1 was: the persisted record split from `Config` and the API's types
  (before any record v4; 10.1d did F43's rewrite rule, and the split itself moved to
  stage 11, where `inspect` becomes Docker's: D10.1d-1), one `take` ("find, lock, read again"), one safe-file module
  (atomic writes, capped no-follow reads, locks, one `pidfd` helper) using `rustix`
  beside `nix` for what nix lacks, `doctor`'s checks into the engine, `cp`'s helper into
  the runtime (so `shipyard-runtime` no longer links the engine), and `run/detached.rs`
  split. Includes F48 and the v0.2 architecture review's findings.
  **Done as 10.1a–e (2026-10-16),** with two changes of plan: the record's split from
  `Config` moved to stage 11 (D10.1d-1: 10.1d did F43's rewrite rule), and attach's and
  logs' halves moved into the runtime (10.1e4a) but `cp`'s stays in the engine, so
  `shipyard-runtime` still links it (D10.1e-2; F69, stage 15: D13-4). F48 closed; its leftovers
  are F72–F74.
- **D24 — A minimal macOS path, used daily, right after the refactor:** not a throwaway
  spike. The Mac CLI runs the guest's `shipyard` over vsock (stdio and the terminal
  forwarded), the project directory shared, published ports on `localhost`; the stage 11
  API replaces the forwarding. *Revised 2026-10-22 (D11.5b-1):* the Mac's Docker clients
  (`docker`, compose, Testcontainers) use the API, served on a Mac socket and relayed to
  the guest (11.5b); the Mac's `shipyard` keeps its forwarding (its terminals, signals and
  Shipyard's own flags, which Docker's API lacks) until stage 13. *Revised 2026-10-26
  (D13-1):* it keeps it for good; stage 13 fixes the forwarded wire instead.
- **D25 — Stage 11's API is a subset of Docker's Engine API,** socket-activated (systemd on
  Linux; forwarded from the VM on macOS), with Shipyard's own data under `/shipyard/…`.
  The subset is defined and tested by what `docker compose` and Testcontainers use.
- **D26 — The CLI is a drop-in for `docker` on the everyday commands** (`run`, `ps`, `logs`,
  `exec`, `build`, `compose`, `images`, `pull`, `push`, `rm`, `stop`…), with Docker's flags,
  short forms and output formats; Shipyard-only fields only in its own formats or behind
  flags.
- **D27 — Compose is `docker compose` against Shipyard's API;** `shipyard compose` passes
  through to it. No compose engine of Shipyard's own.
- **D28 — Images are built by BuildKit, started on demand in a container,** driven by
  `shipyard build`, its cache in a volume (built in stage 12 as a directory per build
  context instead, D12.3-5: root's builder can't reach a volume). No Dockerfile builder of
  Shipyard's own.
- **D29 — macOS:** Apple's Virtualization.framework driven in-process from Rust through
  `objc2-virtualization`, in a VM-host process that starts on first use and exits when
  idle (the VM lives as long as that process); one shared VM running the real Linux
  engine (as OrbStack, Docker Desktop and Podman machine do), not a VM per container; a
  minimal Alpine root filesystem with Alpine's `linux-virt` kernel, built reproducibly,
  `shipyard` as the guest agent; Apple Silicon only (Rosetta exists only there). Chosen
  over libkrun (no Rosetta), Apple's Containerization (macOS 26, Swift-only, a VM per
  container), and vfkit or Lima (a Go binary run as a subprocess).
- **D30 — v1.0 is:** Linux (x86-64 and arm64, cgroup v2, kernel ≥ 5.11) and macOS on Apple
  Silicon; the developer loop through `alias docker=shipyard` (`build`, `run`, `ps`,
  `logs`, `exec`, `push`, `pull`, `compose`), Testcontainers and devcontainers against
  the API, x86 images through Rosetta; the Linux host rootless by default with restart
  policies, boot units and packages; the desktop UI with its observed-traffic map; green
  CI, OCI conformance as expected, fuzzing and mutation checks on every parser and state
  machine, signed reproducible releases, every F-row fixed or explicitly accepted. The
  performance targets (FEATURES §16) are measured and reported, not blocking.
- **D31 — CI stays off for now** (superseded by D15-1, 2026-10-29: the repository goes public and CI runs on GitHub's free runners) (GitHub billing). It must run again before v1.0 (D30);
  until then local verification is the check of record. Accepted trade-off: with no
  outside users either (D21), every check before v1.0 runs on the author's machines.

**Stage 15 decisions (2026-10-29), hardening and v1.0:**
- **D15-1 — The repository goes public early, for CI** (supersedes D31): GitHub's standard
  runners are free for public repositories, so `ci.yml` runs on every push to `main` and
  every pull request, and `quality.yml` nightly. The history was scanned first (no secrets,
  keys or private files; the macOS username taken out of the tree; the commit email kept, the
  user's choice). Nothing is released or announced until stage 15's last step: D21 is about
  users, not visibility.
- **D15-2 — No Apple Developer ID:** the Mac binaries and `Shipyard.app` ship signed with the
  local identity (or ad hoc), not notarized; the docs say how to allow them once. FEATURES
  §16's "notarized" is an accepted limitation of v1.0; `scripts/macos/notarize.sh` stays for
  later.
- **D15-3 — Open findings: the real ones fixed, the rest accepted.** F134 and every low a
  user or a hostile container can hit are fixed; the others go in an "Accepted at v1.0"
  table, each with its reason. The triage goes to the user before any fixing. Done 2026-10-31
  (approved as proposed): 32 to fix (15.2; tests 15.3; packaging 15.5), 44 accepted, 4 later,
  F95 already fixed (its leftover F201).
- **D15-4 — The docs site is mdBook on GitHub Pages,** built from `docs/user/` as it is.
- **D15-5 — The Fedora VM job runs nightly, not on pull requests** (2026-10-30, asked):
  it takes one to two and a half hours (a nested QEMU/KVM VM), every other job under ten
  minutes. It's Quality's `fedora` job (nightly on `main` and by hand).

**Stage 14 decisions (2026-10-27), the desktop UI:**
- **D14-1 — Tauri 2:** a Rust core and the UI in the system's web view, so the design's
  mock (HTML and SVG) carries over. The 14.0 probe: a release `.app` of Tauri 2.x with
  Svelte 5 and Vite 8 is 4 MB, built in about 2 minutes from cold.
- **D14-2 — macOS first, Linux after,** both by the stage's close (D30). Windows stays out
  (D18), but the core reaches the API through one transport (a UNIX socket today), so a
  Windows host path later is a transport and its VM, not a rewrite of the UI.
- **D14-3 — TypeScript and Svelte** (Vite), a maintained framework (D19).
- **D14-4 — The "add to `depends_on`" fix shows the YAML to copy:** the service's lines
  and its compose file's path (compose's `com.docker.compose.project.config_files` label);
  Shipyard never edits a user's file.
- **D14-5 — The map's live traffic is connections, polled:** each edge's open connections
  from the containers' socket tables every 1–2 s, beside each container's rx/tx rate from
  `stats`. No conntrack accounting (root-only, a host sysctl).
- **D14-6 — The UI is an API client, never the engine in-process:** Docker's endpoints
  through `bollard` (0.21: it negotiates down to the API's 1.43, the probe found), Shipyard's
  own `/shipyard/…` endpoints over the same socket. Declared links come from compose's
  `com.docker.compose.depends_on` label (`service:condition:restart`, comma-separated: the
  14.0 probe, compose 5.5), which the API keeps as it's given.

**Stage 13 decisions (2026-10-26), macOS production:**
- **D13-1 — The Mac's `shipyard` keeps forwarding its commands; the forwarded wire is
  fixed.** Moving it onto the API (D24, D11.5b-1) is dropped: one code path for both
  platforms (the guest runs the same `shipyard`) beats a second CLI backend made from
  Docker's JSON, which would need about six endpoints of Shipyard's own (build, `system
  df`, sockets, an exec's signals, run's own flags, the guest's `doctor`) and every
  command's output made again on the Mac. Docker's clients already use the API on the Mac
  (11.5b). Stage 13 fixes the wire: signals, resizes and hang-ups on a channel of their own
  (F94, back from 15), an old guest's refusal said plainly (F167), F155, F147.
- **D13-2 — Rosetta runs an amd64 image when it's asked for, or when the image has no
  arm64 variant** (Docker Desktop's rule): `--platform linux/amd64` on pull, run, create and
  build; an amd64-only image pulled as amd64, with a warning. The store records each
  image's platform.
- **D13-3 — Signing: a stable local identity now, notarization at release.** `sign.sh` uses
  a self-signed code-signing certificate from the keychain when there is one, so macOS sees
  one identity across rebuilds and asks its privacy question once (F140). Developer ID
  signing and notarization are scripted in stage 13, run in stage 15 (they need a paid
  Apple Developer account). A deployment target of macOS 13 (D10.3-4).
- **D13-4 — F59, F69 and F77 move to stage 15:** not Mac work (F59's identity of Shipyard's
  own comes with packaging's system user).

**Step 10.3 decisions (2026-10-17), the minimal Mac path:**
- **D10.3-1:** the step is split in three: the guest and the VM host; forwarding; then
  ports, idle and docs.
- **D10.3-2:** the guest's root is an initramfs rebuilt per version (an upgrade replaces
  two files); only `/var/lib/shipyard` is on a persistent disk, which the guest formats on
  its first boot.
- **D10.3-3:** the VM shares the user's home, at the same path, read-write
  (Virtualization.framework fixes shares at boot, so one share serves every project; the
  guest can then read and write the home, as Docker Desktop's file sharing can).
- **D10.3-4:** the macOS floor is 13 Ventura (what Rosetta in Virtualization.framework
  needs, so stage 13 doesn't move it).
- **D10.3-5:** forwarding comes in two parts: every command without a terminal first,
  then terminals.
- **D10.3-6:** a forwarded command whose wire the guest doesn't speak is refused ("rebuild
  the guest"); a guest of another build that speaks it is warned about once per VM start.
- **D10.3-7:** a forwarded command gets a pseudo-terminal in the guest when the Mac's
  stdin and stdout are both terminals, and pipes otherwise.
- **D10.3-8:** `SHIPYARD_VM=off` runs a Mac's commands on the Mac itself, as before the
  VM (no isolation, no images): the macOS tests and the differential's Mac pass use it
  (`.cargo/config.toml` sets it for cargo's runs), so the native macOS path stays
  tested.
- **D10.3-9:** 10.3c in two parts: c1 ports on `localhost`; c2 the idle stop, `doctor`
  on macOS, the docs and stage 10's Mac milestone.
- **D10.3-10:** a published port listens on the Mac's loopback only (`127.0.0.1` and
  `::1`; `127.0.0.1` alone for `-p 127.0.0.1:…`): nothing on the Mac's network reaches a
  container. Exposing one is a later opt-in.
- **D10.3-11:** the VM stops after 5 minutes with no command and no container active
  (running, paused, stopping or waiting to restart); `SHIPYARD_VM_IDLE` (seconds)
  overrides it.
- **D10-close-1 (2026-10-18):** a Mac's `shipyard doctor` shows the VM's own Linux checks
  (the engine's, run in the guest by `__guest-doctor`) when the VM runs; it never starts
  it to check. This keeps FEATURES §10's promise of the engine's kernel needs checked in
  the guest.
- **D10-close-2:** the stage's close in two parts: a (the guest's checks, three
  whole-stage reviews, their fixes), b (the notes, the docs sweep, fuzzing, verification,
  the Linux check by hand).
- **D10-close-3:** the Linux check by hand is the v0.2 milestone on Lima's Ubuntu 24.04
  arm64 (AppArmor's restriction on), rootless and root, through a reboot.
- **D10-close-4:** stage 9's book chapter is moved out, skipped at the user's request.

**Stage 8 decisions (2026-10-09):**
- **D13:** stage 8 builds the pieces (terminals, a confined `exec`, a monitor that
  captures output and serves attach, `logs`/`attach`/`cp` by runtime ID); the
  user-facing commands by name come with stage 9's state model.
- **D14:** real-host CI: a rootless Ubuntu job with AppArmor's restriction on and
  the shipped profile, and a Fedora VM job (SELinux, firewalld).
- **D15:** logs in the CRI format, rotated by size and count, versioned in a
  `log.json` beside them (the log files stay pure CRI).
- **D16:** an isolated command never holds the user's terminal: where Shipyard's
  stdio is a terminal, it gets a pipe Shipyard relays (closes F17: a container
  can't change the terminal's settings). `--no-isolation` keeps the terminal.
- **D17:** an isolated command's stdin is `/dev/null` without `-i`, as Docker's.

**Stage 7.5 decisions (2026-10-09):**
- **D1:** no device program for a rootful run → drop `CAP_MKNOD` (in every set) and warn; `/dev` is unchanged (a `nodev` `/dev` with made nodes wouldn't open them: probe 10); refusing would break cgroup-v1 hosts.
- **D2:** a memfd that can't be exec'd → today's descriptor, with a warning.
- **D3:** host-`/` rootful runs warn; a required flag waits for a major version.
- **D4:** inherited metadata kept; chain-ID names only for extractions that inherited.
- **D5:** 128 layers per image.
- **D6:** fix the built-in profile's data (bundles mean exactly what they say).
- **D7:** anti-spoofing in a `bridge`-family table.
- **D8, D9:** warn, don't refuse.
- **D10:** a per-source conntrack limit becomes a stage 9 option.
- **D11:** F16 closed with a bound test.
- **D12:** F12 closed without deadlines: each wait is bounded by its other end (shipyard
  itself, end-of-file if it dies; each hook's OCI `timeout`, and a hook without one may
  run as long as it likes, as runc's), or behaves as runc's (`create` waits as long as a
  setup hung in the kernel, ended by `delete --force`). A deadline would end hooks the
  spec lets run. Each wait says so in a comment.

## Milestones

| Version | After stage | What works | Status |
|---|---|---|---|
| **v0.1** | 7 | `shipyard run --image alpine sh` rootless on Linux, on the caller's own terminal: pulled from Docker Hub, isolated, resource-limited, networked. Docker's spelling, `run -it alpine sh`, comes with a pseudo-terminal of the container's own (stage 8: `run -it --image alpine sh` works since step 8.2) and the image as the first argument (stage 9) | reached; release-ready after the readiness pass (doctor, the AppArmor profile, release files; checked by hand on Ubuntu 24.04) |
| **v0.2** | 9 | Containers run detached and survive the CLI exiting; logs, `exec`, restart policies | **reached (0.2.0)**: containers by name, detached, with `logs`, `attach`, `exec`, `cp`, restart policies (and after a reboot, with `contrib/systemd`), health checks and `events`. The milestone test (`the_v0_2_milestone_a_named_container_end_to_end`), and by hand (step 9.9b), rootless and root, on Ubuntu 24.04 arm64 (AppArmor's restriction on), Fedora 44 arm64 (SELinux enforcing, firewalld) and Ubuntu 24.04 x86-64 (under QEMU), each through a reboot |
| **v0.3** | 11 | Docker compatibility: the Engine API subset, the drop-in CLI, `docker compose` and Testcontainers against Shipyard; a minimal Mac path since stage 10 (internal, D21) | **reached (stage 11, 2026-10-23; not tagged: internal, D21)**: Docker's own CLI, `docker compose` and Testcontainers drive Shipyard's API, root's and rootless; by hand (11.6c) on Ubuntu 24.04 arm64 (a static musl build, the socket units, compose 8/8 as root and rootless with a recreate, Testcontainers both ways, through a reboot) and on the Mac (`docker.sock` in Shipyard's VM, compose 11/11) |
| **v0.4** | 12 | Images built with `shipyard build` (BuildKit), pushed, tagged (internal) | **reached (stage 12, 2026-10-25; not tagged: internal, D21)**: built, tagged, pushed to a registry with a password after `login`, pulled back with the same ID, saved and loaded, exported and imported; `a_built_image_is_tagged_pushed_and_runs_elsewhere` in the suite; by hand (12.5c2) on Ubuntu 24.04 arm64 (AppArmor's restriction on) and Fedora 44 (SELinux enforcing), root and rootless, through a reboot (42/42 each), and on the Mac through its VM, the Mac's credential handed to the push (13/13) |
| **v0.5** | 13 | Works on macOS, production-grade: Rosetta, the VM sized from the Mac, clock sync, file sharing (internal) | reached, not tagged (D21) |
| **v1.0** | 15 | As D30: both platforms, the developer loop through `alias docker=shipyard`, the Linux host complete, the desktop UI, green CI, conformance, signed releases. **The first public release** | |

## Decisions

- **Daemonless** (Podman-style). There's no root daemon. Each container gets a tiny
  monitor process instead (built in stage 3 for `shipyard-runtime`), and anything else
  (the API socket, the macOS VM) starts on first use and stops when idle, so an idle
  Shipyard costs nothing (D20).
- **For macOS developers and Linux hosts, UNIX-only** (D18).
  - The CLI, engine and UI are native on macOS and Linux.
  - Kernel isolation runs on Linux: natively there, and inside one Shipyard-managed VM on
    Apple Silicon Macs (D29).
- **The runtime is an OCI runtime.**
  - `shipyard-runtime` implements the OCI runtime spec (`config.json`;
    `create / start / kill / delete / state`).
  - The engine calls it like Podman calls runc. That gives us an official conformance
    suite and clean layering.
- **Syscalls via the conventional Rust route.** Use `std` where it covers the call, then
  `nix`, and raw `libc` only where neither does (stage 1, step 1).
  - `nix` over `rustix`: rustix offers no safe `fork`, `execve` or credential-changing
    API for this use, and nix covers the later stages' `clone`, `mount` and `setns`.
  - `rustix` beside it for what nix lacks (pidfds, some ioctls, `*at` calls), so those
    stop being raw `libc` (D23, from the v0.2 Rust review: `pidfd_open` was written
    out four times).
- **The container's PID 1 is Shipyard's own init** (stage 2). It re-execs the running
  binary to start clean, so every binary that runs containers calls
  `process::init_entry_point()` first in `main`. Its status pipe is never trusted beyond
  what its own exit code confirms.
- **After a raw `clone`, the child uses raw system calls** where glibc would keep
  process-wide state (the `setuid` family). They rely on the 64-bit-only build (stage 1).
- **Container mounts are recursively slave** of the host's (stage 2): nothing leaks out,
  and host unmounts reach in.
- **`serde` and `serde_json` for JSON** (stage 3): `config.json` is JSON, and `std` has
  none. The de-facto standard crates.
- **The init re-execs first** (stage 3): the cloned child re-executes the binary at once,
  while it still sees the host's files, and the clean image does all container setup
  (mounts, `pivot_root`, names) in ordinary Rust. A minimal root filesystem has no
  loader or libc for the host's binary.
- **Refuse what would confine less** (stage 3): a bundle field Shipyard can't apply yet,
  whose absence would leave the container less confined than asked, is refused, never
  silently ignored. Fields that don't confine are ignored with a warning.
- **Never reach a container's files, or its state, by path** where the path's meaning can
  change (stage 3): mount targets are resolved inside the root and mounted through
  descriptors, mount flags are only ever added (`mount_setattr`), and the runtime's state
  directory is written through a descriptor.
- **`shipyard-runtime` requires a `pid` namespace, or a cgroup** (stage 5): one of the two
  must find every process of a container.
- **Rootless is automatic** (stage 4): a non-root caller's isolated run, bundle or
  `shipyard-runtime` container gets a user namespace whose root is the caller: the full
  map through `newuidmap`/`newgidmap` and `/etc/subuid`, else the caller's own ID with a
  warning. Root keeps rootful containers, confined the same way.
- **seccomp is compiled by Shipyard itself, in Rust** (stage 4): profiles become classic
  BPF with no C library; Docker's profile is the default for `shipyard run`.
- **The init confines itself before its command goes on** (stage 4): only `CAP_KILL`,
  `no_new_privs` and the container's own seccomp filter, as Docker's init runs under it;
  every way a profile can break the init ends the container visibly.
- **Read the kernel's state; don't infer it** (stage 4): what can't be applied as asked
  (supplementary groups where `setgroups` is denied) is refused, never silently dropped.
- **Containers' cgroups are written directly** (stage 5): cgroupfs, v2 only, below
  `shipyard/` for root and inside systemd's delegated subtree for anyone else; a limit
  that can't be applied is refused, never dropped (except Docker's implicit swap
  default, skipped with a warning where swap isn't accounted).
- **A cgroup, like a PID, is pinned before it's trusted** (stage 5): a lock for a live
  run, the recorded inode for a container, pidfds for its processes.
- **Device control is Shipyard's own eBPF** (stage 5): runc's default list, compiled by
  Shipyard; real root fails closed where it has a cgroup, rootless goes without where
  the kernel refuses.
- **Netlink is Shipyard's own** (stage 7): rtnetlink, nf_tables and conntrack messages are
  built and read by its own bounded, fuzzed code; no `ip` or `nft` binary and no netlink
  crate, as with the seccomp and eBPF compilers.
- **Networking follows Docker's defaults** (stage 7): root's runs join the `bridge` network
  (NAT, no names), rootless runs get pasta (one per container; given the container's
  network namespace as a file since 8.1c, not attached by PID), and
  rootless without pasta is refused with a hint, never quietly given the host's network.
  OCI bundles keep the spec's meaning.
- **The firewall is one table of Shipyard's own** (stage 7): `inet shipyard`, with static
  rules and per-network and per-container sets and maps. Other firewalls are detected
  and warned about with the exact fix, never changed (stage 8.6: firewalld's trusted
  zone, Docker's `DOCKER-USER`; a fix applied isn't warned about again).
- **Turning a host into a router is undone for everyone else** (stage 7): when Shipyard
  turns forwarding on, it routes only containers' traffic, keeps the host learning its
  IPv6 routes (`accept_ra=2`), and never lets a container's router advertisements in.
- **One DNS server per network, rootful, not a daemon** (stage 7): started by the first
  run that needs it, outside the run's supervisor, as `nobody`; it exits when idle.
- **A container never gets the user's terminal** (stage 8, D16, D17): pipes relayed by
  Shipyard, or with `-t` a pseudo-terminal made inside the container whose master comes
  back over a socket; stdin is `/dev/null` without `-i`.
- **One monitor per container, the same binary** (stage 8, D13, D15): it holds the
  container's output, writes CRI logs rotated by size and count, and serves attach on a
  UNIX socket; no daemon.
- **What runs inside a container on its behalf is a short-lived process of Shipyard's
  own** (stage 8): `exec`'s joiner and `cp`'s helper, started from a sealed copy of the
  binary, non-dumpable before they join, holding nothing of the host's.
- **The OCI conformance suite gates the runtime** (stage 8, H5): every test passes, or
  fails for a stated reason matched in its output (`scripts/conformance/expected.txt`).
- **Images are built by BuildKit, in stage 12** (D28), not by a builder of Shipyard's own.

## Open questions

Decide these when we reach them:

- How CI runs again before v1.0: restored GitHub billing or a self-hosted runner (D31).
- Which of the Docker Engine API's endpoints and versions the subset covers: set by what
  `docker compose` and Testcontainers use (D25), listed in stage 11's plan.

Decided: the Docker-compatible API (D25, 2026-10-15); the UI framework, Tauri 2 (D14-1,
2026-10-27).

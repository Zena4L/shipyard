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
| 12 | **Build & images** | `shipyard build` driving BuildKit started on demand (D28); `push`, `tag`, `login`, image `inspect` / `history` / `save` / `load` | BuildKit, OCI image & distribution specs | |
| 13 | **macOS production** | The Mac path made production-grade: Rosetta, memory ballooning, clock sync after sleep, fast file sharing, DNS and ports, exit when idle, entitlements and notarization; Apple Silicon only (D29) | Virtualization.framework, vsock, virtiofs | |
| 14 | **Desktop UI** | The [design](design/README.md), built on the stage 11 API; the observed-traffic map first | Tauri (likely) | |
| 15 | **Hardening & v1.0** | OCI conformance as expected (it runs since stage 8), fuzzing and mutation checks on every parser and state machine, benchmarks, signed reproducible releases, packages (Homebrew, `.deb`, `.rpm`), a docs site; green CI; **the first public release** (D21, D30) | [FEATURES.md §16](FEATURES.md#16-production-quality--ongoing-from-stage-1-finalized-in-stage-15) | |

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
| F24 | low | An OCI bind source is checked at load but resolved again by path at start: swapped for a symlink in between, another host path is bound (runc shares it) | open it with `O_PATH`/`O_NOFOLLOW` at load and carry the descriptor through the init's re-exec; it needs write access above another bundle's source, so it waits | later |
| F29 | low | For a program embedding the runtime library (narrowed by 11.2c3b, which restores the subreaper setting on drop and documents the main-thread and SIGPIPE rules): while a `Supervisor` runs a command it reaps every child of the process, not only the command's tree (documented on `Supervisor`; nothing long-lived embeds it: the API runs exec as a child `shipyard exec`); a leftover still alive after `kill_leftovers`' 2 s stays its child, unreaped once the subreaper is off | reap only the command's tree (pidfds of the children it made), and what's left before the drop | 15 |
| F31 | low | Stages 7.5 and 7.6's residuals: link-local IPv6 isn't tied to its container; a store root that's a symlink is only warned about; a root filesystem's own device nodes open where no device program guards; a symlink could steer a volume onto Shipyard's tmpfs covers in `/sys`, `/dev`'s tmpfs, or securityfs, debugfs or the like below a bundle's own recursive `/sys` bind (closed in 9.8a with F36 for mounts, refused by place; a device node steered there by a symlink isn't checked yet); a container that hit its memory limit, then lost a process to the host's OOM killer, reads "out of memory"; `--blkio-weight` doesn't set BFQ's `io.bfq.weight` (runc does); `update --cpu-period` alone clears the quota (runc keeps it); a `delete` of a killed `create` can remove another `--root`'s same-ID cgroup in the moment before its init joins it (that `create` fails) | each as noted in `stages/07.5-hardening.md` | later |
| F47 | low | 9.9a clean-code review (D1, kept as is): `kill` with SIGKILL or the stop signal marks the container stopped by its user before signalling (so its `__cleanup` never restarts what its user killed); a container that ends on its own in the instant between `kill`'s status check and its signal stays down (the mark also cleared a pending restart). Marking only after the signal would reopen the worse race | known limitation: the user asked it to stop. A fix needs the monitor to deliver the signal and the mark together (control protocol) | — |
| F51 | low | 9.9b's manual milestone: a rootless container made from an SSH login, where the user has no delegated cgroup subtree, has its monitor in the session's scope: a shutdown ends it before the user unit's `system stop` (it comes back at boot, its end settled, but without its stop signal). Root's are moved out (`shipyard/monitors`, 9.9b) | documented (`systemd-run --user --scope shipyard run -d …`); a fix needs a delegated scope from systemd (D-Bus), which Shipyard doesn't use | later |
| F59 | low | The v0.2 review (network): the DNS server runs as, and leases are shared with, uid/gid 65534, which every other `nobody` process on the host has: one can hold a lease's lock shared (its sweep never reclaims it) and read containers' names and addresses | an identity of Shipyard's own (a system user, or IDs allocated as systemd's `DynamicUser` does) | 13 (D10.2-2) |
| F62 | low | The v0.2 review (security): the default network can't have `--isolate-host`, so its containers reach the host's services and the cloud's metadata (documented in `SECURITY.md`). (The default process limit: 2,048, in 11.3b1, D11.3-3.) | `--isolate-host` for `bridge` (it needs its own DNS-less exception) | 15 (moved from 11 (D11.6-1): hardening of the default network, not Docker compatibility) |
| F67 | low | Found by 10.1d's Murphy review (2026-10-16), F43's remainder: 10.1d's rule (a record is rewritten at the version it was read at unless a field set in it needs a newer one) protects fields a user asks for, not one the engine itself sets on a running container (as `stopped_by_user`, `restart_count`, `boot_id`, `next_start` were for version 3): such a field in version 4 would make every routine rewrite (`stop`, a start, the restart policy, the recovery pass) write 4, which an older binary's monitor still running refuses, and its container's end goes unrecorded | at version 4: every engine-set field in a file of its own beside the record (as `previous-start.json`, `health.json`, `network-lost`), as `needed_version`'s doc and AGENTS.md §4 say; or tolerant readers within a major version (a `min_reader` field), decided then | 15 |
| F69 | low | Left by 10.1e4a (2026-10-16, D10.1e-2): the `shipyard-runtime` binary still uses the engine for `cp` (by runtime ID: `cp_output.rs`, `cp::helper_entry_point`), so `deny.toml` keeps its exception for `shipyard-cli`'s runtime binary; attach and logs are the runtime's own since 10.1e4a, but the shared `logs_output::printed` still names the engine's `Error` to find a gone reader inside it (`shipyard logs`'s path), so a binary of its own needs that check split too. Moving `cp` would pull `shipyard-image` and `tar` (with `ureq`, `flate2`, `sha2`) into the runtime crate, and dropping the engine needs the binary in a package of its own (dependencies are per package) | `cp`'s runtime half behind a feature, or the binary its own package with only what it uses | 13 |
| F71 | low | 10.1e4a's Murphy review (2026-10-16), from before stage 10: an attach session's own resize watch (`process::ResizeWatch`, 10.2f2) is one per process, as every signal watch is (the runtime's `Signals`): a second terminal session at once (threads of an API or UI that embeds the engine) fails to start, where before 10.2f2 it overwrote the first's pipe and the first's drop left the second without resizes. The CLI never runs two | one shared watch with a count, or a resize channel per session | 15 (moved from 11 (D11.6-1): the CLI never runs two sessions; the UI (stage 14) and hardening will) |
| F89 | low | Left from F74 (10.2f3, 2026-10-17, D10.2-20: only the row's named modules were split): 25 library modules are still past ≈1,000 lines, most with their tests inside; the largest are `network/store.rs` (3,415), `container/lifecycle.rs` (2,946), `image/unpack.rs` (2,930), `oci/mod.rs` (2,457), `process/failure.rs` (2,022), `network/pasta.rs` (2,008), `isolation.rs` (1,993) | split each when it's next changed (its tests to a `tests.rs` first), checked by its test names before and after | 15 |
| F90 | low | 10.3a's Murphy review (2026-10-17): a guest whose kernel panics isn't seen by the VM's host: Virtualization.framework restarts the VM in place (observed: a panic with `panic=-1` booted four times in 25 s), so the host's watch (`State::Up`) never ends, and a command in flight ends with "the VM went before the command ended"; nor is the console log bounded while the host runs | the host probes its agent (a boot ID in the agent's answer), and treats repeated misses as a stop; the console log capped while running | 13 |
| F92 | low | 10.3a's clean-code review (2026-10-17): the guest's packages are pinned by version, their signatures checked by apk, but not by sha256, and their dependencies aren't pinned at all (`scripts/guest/alpine.lock`): two builds of one lock can differ when Alpine updates a dependency | a lock of every installed package's sha256, checked after the install (the manifest records them) | 13 |
| F93 | low | 10.3a's Murphy review (2026-10-17): `vm stop` signals the host by the PID in its lock file, read while the lock is held: a host that ends in between, and a PID reused at once, would send SIGTERM to another process | ask the host to stop over its socket | 13 |
| F94 | low | 10.3b1's Murphy review (2026-10-17): a forwarded command's signals share its connection with its input: when the command doesn't read its stdin (`yes \| shipyard run -i alpine sleep 1000`), the agent's write to it blocks its reading of frames, and a Ctrl-C or SIGTERM queued behind the input never arrives (the Mac CLI's third Ctrl-C, or 10 s after a SIGTERM, leaves anyway; the command runs on in the VM). 10.3b2's Murphy review: the same on a terminal, where the agent's write to the pseudo-terminal's master blocks once its input queue is full (a paste into a command that stopped reading), and a resize, a signal or the client's hang-up waits behind it | signals on a channel of their own: stage 11's API (`kill` beside the attach stream) | 15 (moved from 11 (D11.6-1): a second channel on the VM wire, with stage 13's macOS work or 15) |
| F96 | low | 10.3c1's plan review (2026-10-17): the Mac forwards TCP only; a UDP port a container publishes (`-p 53:53/udp`) isn't reachable from the Mac (`vm status` counts them) | a UDP relay per published port (datagrams over the agent's connection, a mapping per Mac peer) | 13 |
| F97 | low | 10.3c1's plan review (2026-10-17): a port published on the guest's IPv6 loopback (`-p [::1]:P:…`) isn't DNAT'd even in the guest (`not_to_loopback6`), so it isn't forwarded to the Mac; and two containers publishing one port on two of the guest's addresses want one Mac address (the first keeps it, the other is logged) | stage 13's DNS and ports: the agent dials the container itself rather than through the guest's DNAT | 13 |
| F98 | low | 10.3c1's Murphy review (2026-10-17; checked on this Mac): a Mac program started after a port is forwarded, binding every address with `SO_REUSEADDR` (most servers do), succeeds beside the VM's `127.0.0.1:P` listener, and `localhost:P` then reaches the container, not it (BSD gives the more specific bind the connection). The other order is refused (`bind_alone`) | bind every address with the program's own rules (as Docker Desktop does, which makes a later program fail as "in use") behind an opt-in, or `doctor` (10.3c2) warning of a wildcard listener on a forwarded port | 13 |
| F100 | low | 10.3c2's Murphy review (2026-10-18), from 10.3b: `client::connect`'s one 150 s bound covers both waiting out a stopping host (its guest's stop up to 100 s) and the next boot; past it, it sends SIGTERM to the host it last started, which may be one just booting, healthy | a bound per phase (the old host's stop, then the new boot), and no signal to a host that's booting in time | 13 |
| F103 | medium | Stage 10's whole-stage Murphy review, Mac (2026-10-18): the data disk (`data.img`, sparse, up to 64 GiB) only grows: nothing discards what the guest frees, so pulls and prunes leave it at its high-water mark on the Mac's disk; a full Mac disk makes the guest's ext4 go read-only | the data disk mounted with `discard` (if Virtualization.framework passes TRIM through), or `fstrim` at the idle stop; `doctor` says the disk's size and the Mac's free space | 13 |
| F105 | low | Stage 10's whole-stage Murphy review, Mac (2026-10-18), unconfirmed: macOS's privacy permissions (Desktop, Documents, Downloads) may be the VM host's, inherited from whatever process started it first (an ssh session, an app without them): binds under those folders would then fail in the guest until `vm stop`, while `doctor` says the home is shared | `doctor` tries the protected folders; the docs name it | 13 |
| F106 | low | Stage 10's whole-stage Murphy review, Mac (2026-10-18): after the Mac's reboot, containers with a restart policy come back only when `shipyard` is next run (no login item starts the VM); a CLI or guest of another build than the running host's is warned about only when the VM is stopped; the host keeps the first CLI's environment (`HOME`, `SHIPYARD_GUEST_DIR`) for its life; a `vm stop` during a boot can report a failure that isn't one; the port poll runs every 2 s while containers are up (a process in the guest each time); `host.log`'s rotation can hide why a start failed; a Mac has no install path but a checkout: the guest's default place isn't where `build.sh` writes (the fresh review, F112's rest) | a login item (LaunchAgent) for policies; the host records what it booted (revision, home, guest) and the CLI compares on every command; the stop's wait counting a boot; a poll backed off when nothing changed; only the host rotates its log; an install step | 13 |
| F107 | low | Stage 10's whole-stage clean-code review (2026-10-18): helpers that grew a copy per step (the versioned read: one `shipyard_sys::versioned::read` since 11.1, for the six formats alike): "128 + signal" from an `ExitStatus` in 4 places; three doctor `Level` enums (engine, `shipyard-vm`, `shipyard-runtime`'s binary); the guest's process names in two crates; `shipyard-vm`'s `pub` items with no outside user; a `Flock` held outside `shipyard_sys::lock` in five places (`image`, `network`, `events`) and raw `libc::flock` in the DNS server | one `shell_code`, one `Level` for the API (stage 11's types), the names beside each other, `pub(crate)`, the locks through `shipyard_sys::lock` | 15 (moved from 11 (D11.6-1): internal duplication, no user-facing change) |
| F108 | medium | Stage 10's fresh review, security and Mac (2026-10-18; narrowed by 10.4b: the home is mounted `nosuid,nodev`, the trust model is in `SECURITY.md`): the agent's vsock port (1024) can be taken by a container started with `seccomp=unconfined` while the agent is down (its connections then carry commands' arguments and `-e` values); a published port answers on the guest's NAT address (`192.168.64.x`, checked by hand in 10.4b) as well as on the Mac's loopback, so other VMs on the Mac's shared NAT can reach it; containers reach the Mac's own services at `192.168.64.1` | the agent's port bound by PID 1 before any container and handed over, or a nonce the host checks; the guest's firewall refusing new connections to published ports that arrive on `eth0`, with a test | 13 |
| F109 | low | Stage 10's fresh review, Mac (2026-10-18; narrowed by 10.4b: the stop's wait is bounded, the agent's restarts back off): stops at logout or shutdown are untested; the guest has 4 GiB and 4 CPUs, fixed; each connection (host and agent) is a thread with no bound and no deadline for its hello; a session ending just as the grace period does can exit 143; the relay cuts a stream still flowing 60 s after one side's half-close; `machine.rs`'s `Send`/`Sync` claims are wider than its use | a logout test; `SHIPYARD_VM_MEMORY`/`CPUS`; a bound and a deadline; `over` set before the terminal's settings are put back; the linger from the last byte; the claims narrowed | 13 |
| F111 | low | Stage 10's fresh review, network (2026-10-18): a DNS TCP reply's write isn't held to the connection's 30 s lifetime (a reader taking a byte every few seconds keeps its slot); a lease is bounded when read (64 KiB) but not when written, so enough `--network-alias`es make a lease every reader refuses (names unresolved, taken by another); F65's fallback leaves out every bridge, so a host whose uplink is a bridge (`br0`) isn't closed when the routing record can't be read; interfaces that appear after forwarding was turned on can be routed to and from | the write against the lifetime; the lease refused at its write; the uplink kept; a decision recorded (stage 7's trade-off) | 15 |
| F113 | low | 10.4b's Murphy review (2026-10-18): after a repair of the Mac VM's data disk (F104's fix), what `e2fsck` couldn't place is in `lost+found` on that disk, which nothing reaches from the Mac (a bind of `/var/lib/shipyard` is refused: outside the home); the repair is said only by the guest's `doctor` (ten boots), not by `vm status` | a way to list and copy out `lost+found` (an internal request, or `shipyard vm`'s); `vm status` saying a recent repair | 13 |
| F115 | low | 11.2a's Murphy review (2026-10-18): release builds abort on a panic (`panic = "abort"`), so one engine call that panics in the API service ends the service and every call under way in it, where a debug build answers 500 | a profile for the service that unwinds, or `catch_unwind` per call | 15 |
| F122 | low | 11.2c3b's Murphy review (2026-10-19): F88's `die` is said before its mark goes (`run/cleanup.rs`, `run/take.rs`, `run/recover.rs`), so a process killed between the two (microseconds) leaves a mark the next settling says again: two `die`s, never none. Said-then-unmarked is the choice (a lost end is worse than a repeated one) | the mark and the journal line in one step (the event's ID in the mark, the journal searched before saying) | 15 |
| F123 | low | 11.3b2's Murphy review (2026-10-20): a stop that waits long (`stop -t -1`, the API's `t=-1`, or a large `-t`) holds the container's `op.lock` for as long as its process ignores the stop signal: `rm -f` and `docker compose down` are refused Busy after their wait where Docker's `rm -f` kills it; `system stop` at a shutdown skips it (systemd's kill and the boot's pass settle it); through the API, the call keeps `system service` up after its client has gone | `rm -f` taking over a waiting stop (the monitor's `kill_at` already lets a later stop win); the API's stop cancelled with its client | 15 |
| F124 | low | 11.3c2's verify (2026-10-20): `firewall.rs`' `a_port_a_killed_run_left_on_another_network_is_reclaimed` failed once as root ("the container of shipyard N never got its network", `shipyard-test-support/src/net.rs:121`) in about 14 runs: alone 10/10, its whole file 3/3 after. Nothing 11.3c2 changed is on its path; the wait for a run's network (a bound in the helper) may be too short beside other root tests' bridges | Find what the run waited on (its lease, its veth) when it misses, then wait on that, not a bound | 15 |
| F125 | low | 11.3d1 (2026-10-20): `update --pids-limit -1` (Docker's "no limit"), and the API's `CpuQuota` of `-1`, are refused: the record keeps a process limit as a number, and none there means a start applies the default (F62), so lifting one needs a third value; device rates aren't taken by `update` either (Docker's `update` has none) | Keep "none asked" beside the record (`created.json`'s `default_pids` cleared, as F67's rule has it), then write `pids.max` `max` live | 15 |
| F126 | low | 11.3d1's Murphy review (2026-10-20): a `pause` killed during its freeze wait (a process in uninterruptible sleep holds `cgroup.events`' `frozen` at 0 up to 5 s; Ctrl-C or SIGKILL meanwhile) leaves `cgroup.freeze` at 1: the rest frozen, the state said `running` (the runtime reads `frozen`), `unpause` refusing ("not paused"), an `exec` joining it hanging, health checks failing; a stop thaws it (F46). A CLI killed after the freeze but before the event says no `pause` | The runtime's status reads `cgroup.freeze` too (freezing is paused, for `unpause`), and the pause holds its stop signals off until the freeze is settled | 15 |
| F127 | low | 11.3d2's design (D11.3d-2) and Murphy review (2026-10-20): `top` reads `/proc` itself in `ps -ef`'s columns, so `ps`'s options (`top c -o pid,comm`, the API's `ps_args` other than `-ef`) are refused and `STIME` is UTC; a PID that ends and is reused between the cgroup's list and its `/proc` read shows as one of the container's (what its user could read anyway); every row is kept in memory (a cmdline up to 64 KiB each). On the Mac, `stats` refreshes in place only when stdin is a terminal too (the guest gets a terminal for both or neither) | Run the host's `ps` with the container's PIDs (`-p`), or parse the common `-o` fields; check each PID's `/proc/PID/cgroup` after its read; give the guest a terminal for stdout alone | 15 |
| F130 | low | 11.4a1's Murphy review (2026-10-21): a network removed after a host's stop and before its boot's recovery (`shipyard-recover.service` not run, delayed, or a `network prune` in between) takes the network of the `restart: always` containers that recovery was to start: they're stopped then (D11.4a-2's rule), and their start fails. Docker restarts them before it serves anything | Count a container its boot would start (`starts_at_boot`) as up while the host's stop mark is there or the boot isn't recovered | 15 |
| F132 | medium | 11.4a2b and 11.4b1's verify (2026-10-21), measured in 11.5a (2026-10-22, `.claude/skills/shipyard-verify/scripts/guest-crash-rate.sh`): the macOS guest's kernel sometimes finds its memory corrupted a few seconds after boot (a NULL jump, an undefined instruction, kernfs's tree or a user namespace damaged, a vsock `refcount_t` underflow), then oopses and hangs. With four VMs at once on a Mac already swapping: 6 crashes in 227 boots; one VM at a time: 0 in 157 (p ≈ 0.06). Not the kernel version (6.12.114 crashed too), not the netfilter modules' loading (preloaded: still crashed). 11.4b1's "this tree's guest, not the stage-10 one" rested on 0 of 2 runs against 4 of 6, and isn't borne out. A user runs one VM; the VM tests ran four, and now run two (`vm_support::AT_ONCE`). A test's `shipyard run` still waits unbounded on a dead guest. (Once, 11.2c2a, a container's init died before its start with four VMs at once: the old F119, now said plainly as "ended before it could start", 11.5b2b.) | Name the trigger (memory pressure or concurrency: four at once with 2 GiB guests; the stage-10 guest four at once), then a debug kernel (KASAN) to catch the write; report upstream (Apple or the kernel) with consoles; bound the VM tests' commands | 13 |
| F134 | medium | 11.4b2's design review (2026-10-21, D11.4b2-5): joining another container's PID namespace (`--pid container:NAME`, compose's `pid: service:`, an OCI `pid` path) is left refused: B's setup would be visible to A (re-execed after joining A's user namespace, its memory is A's to `ptrace` with `SYS_PTRACE`), B's init isn't PID 1 (its leftovers go to A's init), a dying namespace fails the clone with `ENOMEM`, a chained join lands elsewhere | Run the clean image's set-up in the joiner's clone (no exec after a join); B's init kills and reaps its descendants before it ends, with a catchable parent-death signal; a cgroup required (its `cgroup.kill` on stop); `ENOMEM` as exiting; chained joins refused | 15 |
| F135 | low | 11.4b2a's reviews (2026-10-21): (a) a monitor's failure reaches `create` as `ContainerError::Setup(String)` (`Answer::Failed`), so the join tests match its words, not its kind (H6); (b) an enterer killed after its clone and before its word leaves the new container's child, the monitor's own, a zombie holding the clean image's descriptors until the monitor ends (invisible to the other container: its PID namespace is new); (c) the join test compares namespace links only, not a SysV segment or a listener seen across; (d) rootless, an OCI path join isn't refused up front (the kernel lets one user join its own namespaces) | (a) failure kinds on the monitor's wire; (b) reap a child the enterer left when it ended without a word; (c) tools in the test root filesystem | 15 |
| F136 | low | 11.4b2b1's Murphy review (2026-10-21): (a) a container whose network another joins, restarted by its policy (or a crash and restart), leaves the joiner on its old, empty network, shown "Up" with nothing said (Docker's too; the user's own `stop`/`restart`/`rm` say it); (b) the host's stop stops both at once, so the joiner may lose its network mid-shutdown; (c) at boot, a joiner with a restart policy whose container won't start fails once and stays down; (d) no rootless end-to-end test of joining a pasta container's network (the test container's rootless runs have no pasta) | (a) the joiner's start keeps the other's init (PID, start time) beside its record; views say "(no network)" when it's changed, and an event; (b) joiners stopped first; (c) such a joiner skipped at boot, said; (d) a pasta-capable test environment | 15 |
| F137 | low | 11.4b2b2's reviews (2026-10-21): (a) the shared `/dev/shm`'s own rules (`nosuid,nodev,noexec`, private: `rootfs.rs` `confine_shared`) and the tmpfs check have no end-to-end test: only a privileged other container (`SYS_ADMIN` and a `mount` tool) could change its own; (b) the helper's 30 s bound has no test (a lookup that never ends needs a FUSE mount in the other); (c) a container's error for its `/dev/shm` clone is `MountDev` for every cause (`setns` refused, not a tmpfs) | (a) a runtime test with a capable plan and the host's `mount` in its root; (b) a FUSE fixture; (c) a step of its own | 15 |
| F138 | low | 11.5a's Murphy review (2026-10-22): the macOS VM's network card keeps its address in its directory's `network.json`; a copy of that directory run as a second VM on the same Mac (`SHIPYARD_VM_DIR`, or a backup restored beside the live one) has the same address, and the two clash on the NAT (one lease, ARP fights, pulls failing now and then with nothing pointing at it). | Keep the directory's device and inode in `network.json` and make a new address when they differ (one lease for a copy) | 13 |
| F139 | low | 11.5b1's check with Docker's CLI (2026-10-22): `docker run --rm` whose start fails (the container never ran, then removed) prints Docker's error and also `Error waiting for container: it was removed before its end could be read`: the API's `wait?condition=removed` answers a container gone with no end seen as a failure, where moby answers a removal as the condition met. The exit code is right (125) | `condition=removed`: a container that never started and is gone answers its removal (StatusCode 0, no Error); one that ran keeps "never made up" | 15 |
| F140 | medium | 11.5b2a's verify (2026-10-22): a VM host launchd started (`shipyard vm install`) is a program of its own to macOS's privacy permissions, without the terminal's: the folders macOS guards (Desktop, Documents, Downloads, iCloud Drive) are unreadable to it until the user allows `shipyard` (a prompt; asked again for each new code identity, so for each rebuild of an ad-hoc-signed build). Until then a command run from such a folder is refused ("isn't in the guest") and a bind of one is empty. `vm install` refuses a binary or guest there; docs say so | A stable signed identity (stage 13's signing and notarization: one prompt, kept); `doctor` saying whether the host may read those folders; a clearer refusal naming the permission | 13 |
| F141 | low | 11.6a2's Murphy review (2026-10-23): `POST /auth` against a registry whose `/v2/` redirects to another host that answers 401 says `500 signing in: …`, not Docker's `401 unauthorized` (no credentials go to another host, so no sign-in is tried there); and a slow or failing registry holds a sign-in for minutes (the pull's 4 attempts, each up to its connect and response bounds) | a redirected 401 said as a refusal; `/auth` with one attempt and a shorter bound | 15 |
| F142 | low | 11.6b's API review (2026-10-23): the service's stop waits for its engine calls, but a call that never ends quickly (a big pull, `stop?t=300`) meets systemd's stop timeout (90 s by default: SIGKILL, which the stores' crash safety absorbs); a stop can drop the answer to a call that has just succeeded (its connection's task isn't drained); a request's body has no time bound (a same-user client that stalls one holds the service up); idle keep-alive connections close after 30 s (hyper's header timeout), where Docker's have none | `TimeoutStopSec` and a pull that stops at a stop; connections drained at the end; a body bound; keep-alive as Docker's | 15 |
| F143 | medium | 11.6b's Mac review (2026-10-23): on a Mac, `docker.sock` relays to the VM's API, which takes a bind source outside the home as the VM's own path (made empty there, as Docker's daemon makes a missing one): `docker run -v /tmp/proj:/w`, or a compose project under `/tmp` or `/Volumes`, silently writes into the VM instead of the Mac; the CLI refuses such paths (`forward.rs`), the API doesn't | refuse a bind source outside the shared home in the guest's API (Docker Desktop's "mounts denied"), or share more of the Mac | 13 |
| F144 | low | 11.6b's Mac review (2026-10-23): a start through launchd whose `launchctl print` fails at the kick (`runs` unknown) never sees a host that exited with an error as ended, and waits the whole 150 s instead of failing at once | read the job's runs again after the kick, or take any exit then as the start's | 13 |
| F145 | medium | 11.6b's whole-stage clean-code review (2026-10-23): what grew twice over stage 11: four `/proc/PID/stat` parsers (`docker/top.rs`, `process`, `supervisor.rs`, `pasta.rs`); five program-on-`PATH` lookups (two rules for the executable bit); the hex encoder six times; a byte-size formatter twice (`output::human_bytes`, `rows::binary_size`: pull and `stats` show sizes differently); `unix_now` open-coded; system prune's parts two shapes; the reserved network names in two lists (`RESERVED`, `BUILTINS_ROOTLESS`); test fixtures copied (`Stores` in `api.rs`, `containers.rs`, `compose.rs`; `docker_cli`/`text` in two VM tests; five copies of `eventually`); a few API unit tests that match an error's words; files past what one reader holds (`api/exec.rs`, `docker/container.rs`, `vm/client.rs`, `format/rows.rs`, `api/create.rs`) and `create_holding`/`start_holding`/`run::finish` near the line limit | one `shipyard_sys::proc::Stat`, `path::find_program`, `hex::lower`; one size formatter; a shared `Stores` in test-support; split the big modules and the create's checks into a `prepare` | 12 |
| F146 | low | 11.6b's runtime review (2026-10-23): a volume's `CreatedAt` is its directory's change time, which its first fill moves (the `filling` marker made and removed beside `_data`), as a cut-short create's `labels.json` cleanup does | keep its making's time (in `labels.json`, or a file of its own) | 12 |
| F147 | low | 11.7's measured review (2026-10-23), Mac, not reproduced: `host.rs`'s `refuse_waiting` sets launchd's listening `docker.sock` non-blocking and never back, and `serving::accept` never makes an accepted connection blocking again, so if launchd hands later hosts the same file description, their relays' `io::copy` would end at the first `EAGAIN`. Reproduced as a failed launchd start then three full transfers (20 MB out to a slow reader, 5 MB of late input): every byte came. 11.7's after-measure saw one late-input check after a failed start give no output at all (`docker run -i … wc -c` printed nothing, no error), then 10 of 10 the same way whole: unexplained, and not F147's mechanism (that would give a short count) | find what ended it (a `docker` exit status and the host's log kept for each check); instead of changing the shared listener's flags; set each accepted connection blocking | 13 |
| F148 | low | 11.7's API review (2026-10-23): an exec instance is marked started before its spawn (`exec.rs`), so a spawn that fails (`EAGAIN`, `EMFILE`) leaves it "started" with no exit code and refuses a retry (409); a finished exec shuts its client's socket for writing only, so its input thread (a copy of the client, and of the terminal's master) lives until the client closes | mark it started once spawned; shut the client both ways once the exec has ended and its output is drained | 12 |
| F149 | low | 11.7's API review (2026-10-23): the service's lifecycle at its edges: (a) self-bound (`shipyard compose`, the guest), its idle exit drops connections already queued in its backlog (one poll serves one waiting client); (b) the systemd units set no start limit, so a request that panics (release builds abort: F115) retried by a client can hit `service-start-limit-hit`, leaving the socket off until `systemctl reset-failed`; (c) the service `shipyard compose` starts logs to `/dev/null`; (d) no cap on connections, and a body that stalls after its headers holds its connection (the owner's UID alone) | (a) serve the whole backlog before the end; (b) a `StartLimit` note or setting in the units; (c) a log beside the socket; (d) a connection cap and a body timeout | 12 |
| F150 | low | 11.7's engine review (2026-10-23): (a) a create places its record, then takes its `op.lock` (`run/create.rs`): a `start` in that window runs before its volumes are filled; (b) `restart_pending` looks are zero-wait tries of `restarting`, so the recovery pass (`recover.rs`) and a plain `rm` (and `container prune`) beside another client's look skip or refuse for an instant; (c) `system prune -a` beside a `run` can remove the image between its pull and its pin; (d) `.locks/<name>` and `.fill-locks/<name>` files are never removed (one per anonymous volume ever made); (e) 11.7's Murphy review: a `nocopy` create now takes its volume's fill lock for a moment, so it waits (up to 600 s, and through the API with no client's hang-up to end it) behind another container's fill of that volume; (f) a create without an image (`--rootfs`) takes no fill lock, so one placed while another's first start finishes a fill isn't seen by that start's guard | (a) the `op.lock` made and held inside the `.new-` directory before it's placed; (b) a short bounded wait, as `__cleanup`'s; (c) a pin before the pull's end, or prune skipping an image newer than a create's bound; (d) removed with their volume, under its lock; (e) a short wait there; (f) the brief fill lock for every create's named volumes | 15 |
| F151 | low | 11.7's runtime review (2026-10-23): the API's `exec` children inherit the service's raised descriptor limit (only monitors get the caller's back, `CALLERS_DESCRIPTOR_LIMIT`), so a `docker exec` runs at soft = hard (often 524,288) while its container's processes run at 1,024 | the caller's limit put back in the exec child, as `spawn_monitor` does | 12 |
| F152 | low | 11.7's Mac review (2026-10-23): (a) a plist installed but not loaded (a manual `bootout`, a failed load at login): the CLI's host leaves `docker.sock` to launchd, which isn't running, and nothing says why `docker` can't connect; (b) without launchd, a Docker client that connects in the idle stop's window gets a 503 and doesn't retry (the CLI does); (c) the per-socket cap (1,024 each for `vm.sock`, `docker.sock`, forwarded ports) is past macOS's descriptor and thread limits at full load; (d) `shipyard system service` on a Mac says "Docker's API isn't served yet", though `docker.sock` is | (a) `doctor`/`vm status` say it; (b) a stop that leaves a waiting client to the next host; (c) one cap for all sockets; (d) point at `docker.sock` and `vm install` | 13 |
| F153 | low | 11.7's bind-source fix (D11.7-4), what it leaves: a source first made by a later start (missing at the create, made at a start) is bound by descriptor at that start only (its `created.json` is written once); and a made source's directory, the deepest one that was there, is opened as Docker resolves it, so a container that can replace that directory itself (not what's below it) still redirects it, as with Docker | the made sources a start makes in a file of its own beside the record; a pinned identity (device and inode) for the directory | 15 |
| F154 | low | 11.7's CLI/tests and security reviews (2026-10-23): (a) the dormant CI (D31) would run stage 11's Docker-driven tests against the runner's own Docker CLI 28, not the pinned 24.0.9 (`scripts/test-image/Dockerfile`); (b) on Linux, no API, compose or Testcontainers test sends traffic through a port the API published (the CLI's port tests cover the same `RunRequest`); (c) `api_request`'s fuzzing reaches create, network and exec bodies only as valid JSON, with no dictionary, and never the upgrade headers, `parse_time`, `DetachKeys::parse` or the exec start body; (d) fields the API's create doesn't know are ignored, as Docker's are: a future restrictive field would weaken a container unsaid | (a) CI runs in the test image; (b) a `TcpStream::connect` in the Testcontainers and compose tests; (c) a dictionary of Docker's field names and targets for those parsers; (d) an allow-list test of the fields read | 15 |
| F155 | low | 11.7's hang-up (2026-10-23): the Mac's host names an API connection to the guest by its vsock source port (`API_HANG_UP PORT`); a hang-up sent as the guest's relay of that connection ended and a new connection took the same port would hang the new one up (unlikely: the host sends it the moment its own relay ends, while the guest's still waits) | a token of the guest's in the welcome, from a protocol version that carries it (the other half, a relay's end removing a newer connection's entry by the same port, is closed: each kept connection has an identity of its own) | 13 |
| F95 | low | 10.3b2's Murphy review (2026-10-17), from before 10.3 (`attach` has it): with `panic = "abort"` (the release profile), a panic while the Mac's terminal is raw (a forwarded `run -it`, an `attach`) ends `shipyard` without `RawMode`'s drop: the terminal is left raw (`reset` mends it) | a panic hook that puts the saved settings back (`tcsetattr` is async-signal-safe), once for every raw mode | 15 |
| F76 | low | 10.1's whole-step clean-code review (2026-10-16): seams stage 11's API will meet. `shipyard-sys`'s locks say "deadline passed" two ways (`lock_until`'s `EWOULDBLOCK`, `try_lock_until`'s `Ok(false)`) and its errors are `nix::Result` in some modules, `io::Result` in others; "settle" means three things (`attach::client::settle`, `containers::settled`, the recovery pass's); a run's end is two types (`run::Outcome`, `run::Finished`, beside `exec::Ran` and `attach::Settled`); `report::Collect::events()` returns messages; `health::Health`'s and `events::Event`'s writer-side methods are public; `cp::copy_in`/`copy_out` by runtime ID are public beside `copy_by_name` (named unlike `to_container`, `of_container`, `in_container`); the CLI's `*_output` modules beside its command groups, with the time parsers in `logs_output` | settle each with the API's types | 15 (moved from 11 (D11.6-1): internal naming and visibility, no user-facing change) |
| F77 | low | Left by 10.2a's F53 fix (2026-10-16): the port-reservation lock is a file in the caller's runtime directory, but without one (a rootless run with no `XDG_RUNTIME_DIR`, a root whose `/run` isn't writable) it's the abstract socket name `@shipyard/port-reservations` any local user can hold, as before; and two users' Shipyards (or a v0.2.0 restart waiter beside a newer one) don't share a lock, so two asking for one port in `TIME_WAIT` at the same instant can both hold it (Linux shares `SO_REUSEADDR` across users), root's DNAT then shadowing the user's pasta port | a host-wide lock only Shipyard's users can take (a group-owned lock file), or refuse a port another user's run holds | 13 |
| F79 | low | Seen once by 10.2a's verification (2026-10-16), macOS, during `verify.sh`'s full run: `run.rs`'s `shipyard_dies_from_sigint_when_the_command_did` (its assertion at `tests/run.rs:353`) and `signals.rs`'s `signals_sent_to_shipyard_are_forwarded` (a 30 s wait in `shipyard-test-support`'s `lib.rs:691`) failed together; each passed 3/3 alone right after, and the whole macOS suite passed again (1,076 / 0). Code 10.2a didn't touch **Not reproduced in 50 runs under load (10.2e2, 2026-10-17)** | reproduce under load (both binaries looped beside the suite); then fix the signal's delivery or the tests' waits | 15 |
| F87 | low | After F63's fix (10.2e1, 2026-10-17): the root `image` tests failed once in 30 runs (10 under load, 10 alone, 10 under load: 9+10+10), where they had failed 1 in 3 to 5; the one failure's cause wasn't captured (that loop kept only pass or fail). F63's row had noted a second, unconfirmed cause in `a_missing_working_directory_is_made` **Not reproduced in 50 runs under load (10.2e2, 2026-10-17)** | loop with failures kept (as `e2e1-image.sh` does now) until it fails, then find its cause | 15 |

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
  `shipyard-runtime` still links it (D10.1e-2; F69, stage 13). F48 closed; its leftovers
  are F72–F74.
- **D24 — A minimal macOS path, used daily, right after the refactor:** not a throwaway
  spike. The Mac CLI runs the guest's `shipyard` over vsock (stdio and the terminal
  forwarded), the project directory shared, published ports on `localhost`; the stage 11
  API replaces the forwarding. *Revised 2026-10-22 (D11.5b-1):* the Mac's Docker clients
  (`docker`, compose, Testcontainers) use the API, served on a Mac socket and relayed to
  the guest (11.5b); the Mac's `shipyard` keeps its forwarding (its terminals, signals and
  Shipyard's own flags, which Docker's API lacks) until stage 13.
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
  `shipyard build`, its cache in a volume. No Dockerfile builder of Shipyard's own.
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
- **D31 — CI stays off for now** (GitHub billing). It must run again before v1.0 (D30);
  until then local verification is the check of record. Accepted trade-off: with no
  outside users either (D21), every check before v1.0 runs on the author's machines.

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
| **v0.4** | 12 | Images built with `shipyard build` (BuildKit), pushed, tagged (internal) | |
| **v0.5** | 13 | Works on macOS, production-grade: Rosetta, ballooning, clock sync, fast file sharing (internal) | |
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

- UI framework: Tauri or alternatives (stage 14).
- How CI runs again before v1.0: restored GitHub billing or a self-hosted runner (D31).
- Which of the Docker Engine API's endpoints and versions the subset covers: set by what
  `docker compose` and Testcontainers use (D25), listed in stage 11's plan.

Decided: the Docker-compatible API (D25, 2026-10-15).

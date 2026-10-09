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
| 5 | **Resource control** | cgroups v2 limits and cgroup namespaces, OOM detection, live stats, pause/resume, device access control (eBPF), OCI hooks; `shipyard`'s own `stats`/`top`/`pause`/`update` come with stage 9's named containers | TLPI 36; `cgroups(7)`; OCI runtime spec (hooks) | [done](stages/05-resources.md) |
| 6 | **Images & storage** | Registry pulls (private registries, resumable), OCI/Docker manifests, safe layer unpacking (fuzzed), content-addressed store shared by concurrent runs, overlayfs (rootless too), volumes. `tag`/`inspect`/`save`… and `--mount` come with stage 9; the `fuse-overlayfs` fallback later | OCI image & distribution specs; `overlayfs` docs; TLPI 14, 18, 55 | [done](stages/06-images.md) |
| 7 | **Networking** | Network namespaces, bridge + veth over Shipyard's own netlink, IPAM, NAT and published ports with its own nf_tables client, rootless networking with pasta, a DNS server per network, several networks per container, IPv6, reading a container's sockets. `network connect`/`disconnect` and the sockets command come with stage 9 | TLPI 56–61; `netlink(7)`, `rtnetlink(7)`, `network_namespaces(7)` | [done](stages/07-networking.md) |
| 8 | **Terminals, I/O & logs** | Per-container monitor, PTYs, attach/detach, `exec`, log files with rotation, `cp` | TLPI 44, 62–64 | |
| 9 | **Engine state** | On-disk state, locking, crash recovery, restart policies, health checks, events | TLPI 37, 55 | |
| 10 | **Compose** | `compose.yaml` subset; `up / down / ps / logs`; dependency-ordered start | Compose spec | |
| 11 | **API** | Versioned REST + event stream over a UNIX socket, socket-activated (still daemonless) | TLPI 57 | |
| 12 | **macOS VM backend** | Shipyard boots and manages its own minimal Linux VM; the Mac CLI drives the runtime inside it | Virtualization.framework, vsock, virtiofs | |
| 13 | **Desktop UI** | The [design](design/README.md), built on the stage 11 API | Tauri (likely) | |
| 14 | **Hardening & release** | OCI conformance suite passing, fuzzing, benchmarks, threat model, signed reproducible releases, packaging | [FEATURES.md §15](FEATURES.md#15-production-quality--ongoing-from-stage-1-finalized-in-stage-14) | |

Each finished stage gets **stage notes** in [`stages/`](stages/): what was built and why,
macOS and Linux differences, the failure modes and how they're handled, known limitations,
and what was carried forward to later stages.

**Ongoing from stage 1:**

- tests for every stage
- the CLI growing with each stage
- the production-quality items in FEATURES.md §15, adopted as soon as they apply
- the [hardening track](#hardening-track), each item from its home stage

Stages 2–9 are Linux-only. Until stage 12 lands, develop them on macOS inside a Lima VM,
and run their tests in a Linux container with `scripts/test-linux.sh` (see
[DEVELOPMENT.md](DEVELOPMENT.md)).

## Hardening track

What looking back at stages 0–7 says to do differently (decided 2026-10-09). Each item
has a home stage; none waits for stage 14. The first two get more expensive with every
stage, so they come first.

| # | Item | Why | Home |
|---|---|---|---|
| H1 | **An engine crate** (`shipyard-engine`): the logic now in the CLI (`run.rs`, `network.rs`, …) behind a typed API; the CLI a thin client of it, with `--json` output | stage 11's API and stage 13's UI need it as a library; the move grows with every stage | stage 9 (start in stage 8 for what it adds) |
| H2 | **One state model, versioned:** one container record and state machine, one liveness rule and one recovery path in place of each stage's own lock-and-sweep; a schema version on every on-disk format from its first write, and a test reading the previous version | today's records (run areas, leases, run directories, volume holds, `dns.lock`, cgroup records) each work alone; upgrades rest on `serde` defaults | stage 9 |
| H3 | **A threat model in every plan:** who is hostile (the container, a local user, the network, a registry, the image) and what each may never do, written as invariants with a test each; the Murphy review run against the plan too | stage 6's tar escapes and stage 7's router advertisements, open router and lockable leases were all found after building | from stage 8 |
| H4 | **Real hosts in CI:** VM jobs on the distributions users run (Ubuntu with AppArmor, Fedora with SELinux, an older LTS kernel), the probes on each | the test container is one privileged Debian on one kernel; stage 7's last failures appeared only on Ubuntu | stage 8 (one VM job), widened by stage 12 |
| H5 | **Quality tools in CI:** `cargo-mutants` on changed files, the fuzz targets on a schedule, the OCI runtime conformance suite, differential tests of Shipyard's own netlink and nf_tables against `ip` and `nft` | the method works but runs by hand; conformance was planned for stage 14 though the runtime exists since stage 3 | conformance and differential tests in stage 8; mutants and scheduled fuzzing in stage 9 |
| H6 | **Errors as a designed surface:** stable error kinds (and codes) in one module per crate; tests assert the kind, not the wording; messages stay free to improve | wording drifted across steps, and tests matching on wording broke CI twice | with H1 (stage 9), and each new error from stage 8 |
| H7 | **Performance budgets as tests:** run start, run exit, a cached pull, a network attach; a regression fails | pasta's one-second exit delay was found by accident | stage 8 |
| H8 | **Owned components decided explicitly:** for each piece Shipyard writes itself instead of using a crate or a tool (netlink, nf_tables, seccomp, eBPF, tar, DNS), the trade-off recorded here, and a differential test against a reference | each is a large, security-relevant surface Shipyard maintains alone | recorded in stage 8; tests with H5 |
| H9 | **Build hygiene:** small debug info (done: `line-tables-only`), CI caches, documented clean-up of build volumes | builds reached 30 GB on disk and 65 GB in Docker before anyone looked | done in stage 7; kept |

## Open findings

A review of the whole codebase after stage 7 (2026-10-09; five reviewers, one per area)
found these; each was then re-checked against the code by independent agents, which
corrected several (the severities and findings below are the corrected ones). Stage 7.5
(hardening) fixed the first group, and 7.6 the stage 1–7 findings the review had left (F18's pins, F19–F23, F25–F28); the rest have their stage. Each fix
comes with a test that reproduces the problem first. Remove a row when its fix lands.

| ID | Severity | Finding | Fix | Home |
|---|---|---|---|---|
| F6 | high | A joined process runs unconfined (no capability drop, no seccomp); not reachable yet (no `exec` command); `/proc/<joiner>` and `exe` patterns (CVE-2016-9962, CVE-2019-5736, CVE-2024-21626) | confine it, non-dumpable before `setns`, no host procfs descriptors | 8 (`exec`) |
| F10 | low | No per-source conntrack limit: one container can fill the host's connection table (the `ct state invalid drop` landed in 7.5) | `ct count` per source, as an option (D10) | 9 |
| F14 | medium | firewalld rejects by rule, not policy: containers have no way out there and no warning; the Docker fix leaves published ports broken | detect firewalld's table (suggest `--zone=trusted`); complete the Docker fix | 8 (with H4's VM CI) |
| F17 | low | Terminal settings aren't restored after an isolated run (a container can leave the shared terminal raw) | save and restore the termios; stage 8's pseudo-terminal replaces the path | 8 |
| F18 | low | CI: one distribution; no fuzzing or mutation in CI (actions are pinned by commit since 7.6) | H4, H5 | 8–9 |
| F24 | low | An OCI bind source is checked at load but resolved again by path at start: swapped for a symlink in between, another host path is bound (runc shares it) | open it with `O_PATH`/`O_NOFOLLOW` at load and carry the descriptor through the init's re-exec; it needs write access above another bundle's source, so it waits | later |
| F29 | low | For a program embedding the runtime library: the subreaper reaps and kills its own children and stays set; SIGPIPE can kill it if the child dies before go; the parent-death signal is tied to the thread that forked | scope reaping to the command's tree and reset the subreaper; `MSG_NOSIGNAL`/blocked SIGPIPE; document "the main thread" | 11 (the API) |
| F30 | low | DNS TCP slots still per client; no opt-in to keep containers from the host's own services and the cloud metadata address (169.254.169.254); internal networks reach host services bound to every address | per-lease TCP slots; an input chain option | 9 |
| F31 | low | Stages 7.5 and 7.6's residuals: link-local IPv6 isn't tied to its container; a store root that's a symlink is only warned about; a root filesystem's own device nodes open where no device program guards; a symlink can steer a volume onto Shipyard's tmpfs covers in `/sys`, or onto securityfs, debugfs or the like below a bundle's own recursive `/sys` bind; a container that hit its memory limit, then lost a process to the host's OOM killer, reads "out of memory"; `--blkio-weight` doesn't set BFQ's `io.bfq.weight` (runc does); `update --cpu-period` alone clears the quota (runc keeps it); a `delete` of a killed `create` can remove another `--root`'s same-ID cgroup in the moment before its init joins it (that `create` fails) | each as noted in `stages/07.5-hardening.md` | later |

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
| **v0.1** | 7 | `shipyard run --image alpine sh` rootless on Linux, on the caller's own terminal: pulled from Docker Hub, isolated, resource-limited, networked. Docker's spelling, `run -it alpine sh`, comes with a pseudo-terminal of the container's own (stage 8) and the image as the first argument (stage 9) | reached |
| **v0.2** | 9 | Containers run detached and survive the CLI exiting; logs, `exec`, restart policies | |
| **v0.3** | 11 | Compose stacks and the API | |
| **v0.4** | 12 | Works on macOS | |
| **v1.0** | 14 | Desktop UI, conformance-tested, hardened, packaged | |

## Decisions

- **Daemonless** (Podman-style). There's no long-running daemon. Each container gets a
  tiny monitor process instead (built in stage 3 for `shipyard-runtime`), so an idle
  Shipyard costs nothing.
- **macOS-first, UNIX-only.**
  - The CLI, engine and UI are native on macOS and Linux.
  - Kernel isolation runs on Linux: natively there, and inside a Shipyard-managed VM on
    macOS.
- **The runtime is an OCI runtime.**
  - `shipyard-runtime` implements the OCI runtime spec (`config.json`;
    `create / start / kill / delete / state`).
  - The engine calls it like Podman calls runc. That gives us an official conformance
    suite and clean layering.
- **Syscalls via the conventional Rust route.** Use `std` where it covers the call, then
  `nix`, and raw `libc` only where neither does (stage 1, step 1).
  - `nix` over `rustix`: rustix offers no safe `fork`, `execve` or credential-changing
    API for this use, and nix covers the later stages' `clone`, `mount` and `setns`.
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
  (NAT, no names), rootless runs get pasta (one per container, attached by PID), and
  rootless without pasta is refused with a hint, never quietly given the host's network.
  OCI bundles keep the spec's meaning.
- **The firewall is one table of Shipyard's own** (stage 7): `inet shipyard`, with static
  rules and per-network and per-container sets and maps. Other firewalls are detected
  and warned about with the exact fix, never changed.
- **Turning a host into a router is undone for everyone else** (stage 7): when Shipyard
  turns forwarding on, it routes only containers' traffic, keeps the host learning its
  IPv6 routes (`accept_ra=2`), and never lets a container's router advertisements in.
- **One DNS server per network, rootful, not a daemon** (stage 7): started by the first
  run that needs it, outside the run's supervisor, as `nobody`; it exits when idle.
- **No image building before stage 11.** It's out of scope until then; see
  [FEATURES.md](FEATURES.md#out-of-scope-on-purpose).

## Open questions

Decide these when we reach them:

- Whether to ship a Docker-compatible API subset (stage 11).
- UI framework: Tauri or alternatives (stage 13).

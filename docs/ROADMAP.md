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
| 2 | **Namespaces** | `clone3` / `unshare` into PID, mount, UTS, IPC (then net, user, cgroup, time); own session; minimal PID 1 init; `setns` to join a running container | TLPI 28; `namespaces(7)`, `pid_namespaces(7)` | [in progress](stages/02-namespaces.md) |
| 3 | **Filesystem isolation & OCI bundles** | `pivot_root`, mount propagation, `/proc` `/sys` `/dev`, masked and read-only paths; OCI `config.json` and `create / start / kill / delete / state`; user/group names | TLPI 14, 18; `mount_namespaces(7)`, `pivot_root(2)`; OCI runtime spec | |
| 4 | **Users & security** | Rootless (user namespaces, UID/GID maps), capabilities, `no_new_privs`, seccomp, device access control | TLPI 8, 9, 39; `user_namespaces(7)`, `seccomp(2)` | |
| 5 | **Resource control** | cgroups v2 limits, OOM detection, live stats, pause/resume | TLPI 36; `cgroups(7)` | |
| 6 | **Images & storage** | Registry pulls, OCI/Docker manifests, safe layer unpacking, content-addressed store, overlayfs, volumes | OCI image & distribution specs; `overlayfs` docs | |
| 7 | **Networking** | Bridge + veth over netlink, IPAM, NAT, published ports, rootless networking, embedded DNS | TLPI 56–61; `netlink(7)`, `network_namespaces(7)` | |
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

Stages 2–9 are Linux-only. Until stage 12 lands, develop them on macOS inside a Lima VM,
and run their tests in a Linux container with `scripts/test-linux.sh` (see
[DEVELOPMENT.md](DEVELOPMENT.md)).

## Milestones

| Version | After stage | What works |
|---|---|---|
| **v0.1** | 7 | `shipyard run -it alpine sh` rootless on Linux: pulled from Docker Hub, isolated, resource-limited, networked |
| **v0.2** | 9 | Containers run detached and survive the CLI exiting; logs, `exec`, restart policies |
| **v0.3** | 11 | Compose stacks and the API |
| **v0.4** | 12 | Works on macOS |
| **v1.0** | 14 | Desktop UI, conformance-tested, hardened, packaged |

## Decisions

- **Daemonless** (Podman-style). There's no long-running daemon. Each container gets a
  tiny monitor process instead (stage 8), so an idle Shipyard costs nothing.
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
- **No image building before stage 11.** It's out of scope until then; see
  [FEATURES.md](FEATURES.md#out-of-scope-on-purpose).

## Open questions

Decide these when we reach them:

- Whether to ship a Docker-compatible API subset (stage 11).
- UI framework: Tauri or alternatives (stage 13).

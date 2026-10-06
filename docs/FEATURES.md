# Features

The complete set of functionality that makes Shipyard a production-grade, open-source
container engine. Each area names the [roadmap](ROADMAP.md) stage that builds it. This file
is the reference for *what* we build; the roadmap says *in what order*.

## Architecture

Shipyard has three layers, mirroring how the established tools split up:

| Layer | Equivalent | Responsibility |
|---|---|---|
| **Runtime** (`shipyard-runtime`) | runc / crun | Turns a root filesystem plus an OCI `config.json` into an isolated process. Implements the [OCI runtime spec](https://github.com/opencontainers/runtime-spec). |
| **Engine** (daemonless) | Podman | Images, storage, networks, volumes, state, compose, API. Calls the runtime. |
| **Product** | OrbStack | CLI, macOS VM backend, desktop UI, packaging. |

## 1. Process lifecycle — stage 1 *(POSIX, runs natively on macOS)*

- **Spawning:** `fork`/`exec` with the right environment, working directory, user and groups.
- **Exit status:** reported faithfully: normal exit, killed by signal, `128 + N`.
- **Graceful stop:** `SIGTERM`, then a grace period, then `SIGKILL`.
- **Signal forwarding:** from the CLI to the command.
- **Reaping and cleanup:** every process the command started is reaped, and leftovers are
  killed when it exits (Linux: as a child subreaper). This is the stage-1 groundwork for
  stage 2's PID-1 init.
- **Resource limits:** rlimits (open files, process count, core dumps, …).
- **Terminal-aware:** with a terminal, the command shares it and job control (Ctrl-C,
  Ctrl-Z) works normally; `TERM` is passed through.

## 2. Namespaces — stage 2

- **Creating namespaces:** `clone3` (falling back to `clone`) into new PID, mount, UTS and
  IPC namespaces. Isolated by default on Linux; `--no-isolation` opts out.
- **Joining namespaces:** `setns` into a running container. This is the basis of `exec`
  and the UI's Shell.
- **Per-container identity:** the hostname (`--hostname`).
- **Own session:** each container is a new session (`setsid`), detached from the host's
  terminal.
- **Minimal PID 1 init** (tini-like): inside the container's PID namespace, reaps zombies,
  forwards signals, and exits with the main process's code. It re-execs itself clean, so
  it holds none of the engine's command line or environment.
- **Setup handshake:** a parent/child sync pipe, so the parent can finish setup before the
  child `exec`s the user's program.
- **Moved to the stages that give them meaning:**
  - network namespaces: stage 7;
  - user namespaces: stage 4;
  - cgroup namespaces: stage 5;
  - the domain name and namespaced sysctls: stage 3 (from `config.json`);
  - time namespaces: not planned yet;
  - `unshare`: dropped, since `clone3` creates them all.

  The reasons are in [the stage 2 notes](stages/02-namespaces.md#outcome).

## 3. Filesystem isolation and OCI bundles — stage 3

- **Root filesystem** (`--rootfs DIR`, or a bundle's `root.path`): `pivot_root` (never
  `chroot`) after a recursive bind, then the old root detached. Mount targets are never
  followed through symlinks out of the root, and host mounts already on them are
  detached first.
- **Mount propagation:** the container's `/` is a slave, as with runc: its mounts never
  reach the host; the host's unmounts still reach the container, so they don't keep host
  devices busy.
- **Standard mounts:**
  - `/proc`
  - `/sys`, read-only
  - `/dev` as tmpfs with only the minimal device nodes
  - `/dev/pts`, `/dev/shm`, `/dev/mqueue`
- **Masked and read-only paths:** e.g. `/proc/kcore`, `/proc/keys`; from a bundle,
  exactly the ones it lists. Read-only covers the mounts below a path too.
- **Optional read-only root filesystem** (`--read-only`, `root.readonly`).
- **OCI bundles** (`shipyard run --bundle DIR`): `config.json` plus a root filesystem.
  Bind and tmpfs mounts, rlimits, user, environment, hostname, domain name and sysctls.
  Fields that would leave the container less confined than asked are refused, all at
  once; harmless ones are ignored with a warning.
- **The OCI runtime** (`shipyard-runtime`): `create / start / state / kill / delete`,
  runc's command line, a monitor per container (no daemon). From here on, the engine
  drives the runtime the way Podman drives runc.
- **User and group names:** `--user nobody` and `--group-add wheel`, resolved from the
  container's own `/etc/passwd` and `/etc/group`, with Docker's supplementary groups and
  `HOME`.
- **Domain name and namespaced sysctls:** `--domainname`, `--sysctl`, and `config.json`'s
  `domainname` and `linux.sysctl`. Only namespaced sysctls are accepted: the IPC ones
  (`kernel.shm*`, `kernel.msg*`, `kernel.sem`, `fs.mqueue.*`) from this stage, and
  `net.*` once containers have their own network namespace (stage 7).
- **Moved to the stages that give them meaning:**
  - a terminal inside (`process.terminal`, `--console-socket`): stage 8;
  - names for commands run in an existing container (`exec`): with `exec`, stage 8;
  - hooks, `oomScoreAdj`, cgroup mounts and `linux.resources`: stage 5 (cgroups);
  - bundles without a `pid` namespace in `shipyard-runtime`, `kill STOP`, `pause` and
    `resume`: stage 5 (cgroups find and freeze every process);
  - capabilities, `noNewPrivileges`, seccomp, user namespaces: stage 4;
  - `/sys` without the host's network devices: stage 7.

  The reasons are in [the stage 3 notes](stages/03-filesystem.md).

## 4. Users and security — stage 4

- **Rootless:**
  - user namespaces (moved here from stage 2), with UID/GID maps applied via
    `newuidmap`/`newgidmap` from `/etc/subuid`
  - root inside the container maps to an unprivileged user outside
- **Capabilities:** a minimal default set, plus `--cap-add` / `--cap-drop`.
- **`no_new_privs`:** setuid binaries can't gain privileges.
- **seccomp:** a default syscall filter compiled to BPF, plus custom profiles.
- **Device access control:** an eBPF program attached to the cgroup (cgroup v2).
- *Later:* AppArmor / SELinux labels, Landlock.

## 5. Resource control — stage 5

- **cgroup v2 limits:**
  - memory: `memory.max`, `memory.high`, swap
  - CPU: `cpu.max`, `cpu.weight`, cpuset
  - processes: `pids.max`
  - disk I/O: `io.max`
- **OOM detection:** via `memory.events`, reported clearly ("killed: out of memory, exit 137").
- **Live stats:** CPU, memory, I/O and process count, for `stats`, `top` and the UI.
- **Change limits on a running container:** `update`.
- **Pause and resume:** via `cgroup.freeze`.
- **Rootless cgroups:** delegation through systemd.
- **Cgroup namespaces** (moved here from stage 2): the container sees its own cgroup as
  the root.
- **Joined processes** (`exec`) join the container's cgroup, and a cgroup kill catches
  anything that escaped the parent-death signal.

## 6. Images and storage — stage 6

**Images**

- **Formats:** the [OCI image spec](https://github.com/opencontainers/image-spec), plus
  Docker v2 manifests for compatibility.
- **Registry client** ([OCI distribution spec](https://github.com/opencontainers/distribution-spec)):
  - token authentication
  - Docker Hub, GHCR and private registries
  - credential helpers
- **Multi-arch:** pick the right platform from a manifest list (e.g. `linux/arm64`).
- **Downloads:**
  - parallel and resumable
  - sha256 verification of every blob
  - gzip and zstd decompression
- **Safe layer unpacking:**
  - deletion markers: `.wh.` whiteout files and opaque directories
  - protection against path traversal and malicious tar entries
- **Content-addressed store:** identical layers are stored once.
- **Commands:** `images`, `pull`, `tag`, `rmi`, `inspect`, `history`, `save`/`load`,
  `import`/`export`.
- **Garbage collection:** reference-counted, plus `prune`.
- *Later:* `push`, signature verification (cosign / sigstore).

**Storage**

- **overlayfs layer driver:** read-only image layers plus a per-container writable layer.
- **Rootless overlayfs:** native on kernel 5.11+, with a `fuse-overlayfs` fallback.
- **Mount types:** named volumes, bind mounts and tmpfs mounts.
- **Mount options:** read-only/read-write and propagation.
- **Commands:** `volume create / ls / inspect / rm / prune`.
- **Disk accounting:** `system df`.

## 7. Networking — stage 7

- **Network namespaces** (moved here from stage 2): one per container, except in `host`
  mode.
- **Network modes:** `none`, `host`, `bridge`.
- **Bridges and veth pairs:** created over netlink (rtnetlink), with no shelling out to `ip`.
- **IP address management:** subnet allocation and persisted IP leases.
- **Outbound traffic and ports:** NAT via nftables masquerade, plus port publishing.
- **Rootless networking:** `pasta` or `slirp4netns`.
- **Name resolution:**
  - an embedded per-network DNS, so containers resolve each other by name
  - generated `/etc/hosts` and `/etc/resolv.conf`
- **Network options:** multiple networks per container, internal (no-egress) networks, IPv6.
- **Connection observation:** reading `/proc/net/tcp` inside each container's network
  namespace. This powers the UI's "observed but undeclared" dependencies.

## 8. Terminals, I/O and logs — stage 8

- **Per-container monitor process** (like Podman's conmon), which makes "daemonless" work.
  It:
  - holds the container's stdio after the CLI exits
  - records the exit status
  - writes logs
- **Pseudoterminals** for `-it`, with resize via `SIGWINCH` and the `TIOCSWINSZ` ioctl.
- **`attach` / detach:** with detach keys.
- **`exec` sessions:** each with its own terminal.
- **File-descriptor passing:** `--preserve-fds` and systemd-style socket activation
  (`LISTEN_FDS`).
- **Logs:** log files with rotation; `logs --follow --tail --since --timestamps`.
- **`cp`:** copy files into and out of a container.

## 9. Engine state (daemonless) — stage 9

- **On-disk state:**
  - persistent data in `/var/lib/shipyard` (rootless: `~/.local/share/shipyard`)
  - runtime data under `/run`
- **Locking:** `flock`, so concurrent `shipyard` commands never corrupt state.
- **Crash recovery:** on startup, compare recorded state with what's actually running.
- **IDs:** full hex IDs, ISO 6346 display IDs, names and labels.
- **Restart policies:** `no` / `on-failure` / `always` / `unless-stopped`, enforced by the
  monitor rather than a daemon.
- **Health checks:** with health state exposed to the CLI and UI.
- **`events` stream:** start, stop, die, OOM, pull, …
- **Commands across processes:** `kill`, `wait`, `stop` and `ps` work on containers started
  by other shipyard invocations, using the on-disk state.
- *Optional:* generated systemd units so containers start at boot.

## 10. Compose and stacks — stage 10

- **A `compose.yaml` subset:** services, `depends_on` (with conditions), networks, volumes,
  ports, environment, healthcheck, restart.
- **Commands:** `compose up / down / ps / logs / restart`.
- **Start order:** derived from the dependency graph.

## 11. API — stage 11

- **A versioned REST API plus an event stream,** over a UNIX socket.
- **Still daemonless:** the API server is socket-activated on demand and exits when idle.
- *Optional:* a Docker-compatible API subset, so existing tools work against Shipyard (the
  docker CLI, IDE integrations, Testcontainers).

## 12. macOS VM backend — stage 12

- **Virtualization.framework,** driven from Rust via `objc2`.
- **Guest system:** a minimal Linux kernel and root filesystem, tuned for fast boot.
- **Guest agent:** a small agent inside the VM, spoken to over vsock.
- **File sharing:** virtiofs, so bind mounts of macOS paths just work.
- **Ports and DNS:** port forwarding from macOS into the VM, plus DNS.
- **Rosetta:** for running amd64 images on Apple Silicon.
- **VM lifecycle:**
  - start on first use, stop when idle
  - memory ballooning to return unused memory to macOS
  - clock sync after sleep

## 13. Desktop UI — stage 13

- **Built on the stage 11 API.** The spec is [`design/README.md`](design/README.md), with
  an interactive mock in [`design/shipyard-ui.html`](design/shipyard-ui.html).
- **Likely built with Tauri:** Rust core, small binaries.

## 14. CLI — grows with every stage

- **Containers:** `run, create, start, stop, restart, kill, rm, ps, logs, exec, attach,
  pause, unpause, inspect, stats, top, cp, port, wait, rename, update`.
- **Other resources:** `image …`, `volume …`, `network …`, `compose …`.
- **System:** `system info / df / prune / events`, and `doctor`.
- **Output:** `--format json|table`, quiet modes, consistent exit codes.
- **Docker-compatible flags** wherever sensible.
- **Shell completions** (bash / zsh / fish) and man pages.

## 15. Production quality — ongoing from stage 1, finalized in stage 14

**Correctness**

- Unit tests per stage, and integration tests run in a Linux VM in CI.
- Pass the OCI runtime-spec conformance suite
  ([`runtime-tools`](https://github.com/opencontainers/runtime-tools)).
- Fuzzing of every parser: manifests, tar, compose, config.

**Performance targets** (benchmarked in CI)

- Container start in under 100 ms, excluding image pull.
- Idle cost of zero processes besides the per-container monitors.
- Monitor memory under 1 MB RSS each.

**Security process**

- `SECURITY.md` and a written threat model.
- `cargo-deny` and `cargo-audit` in CI.
- A software bill of materials (SBOM), signed releases, and reproducible builds.

**Distribution**

- Static (musl) Linux binaries, a Homebrew tap, `.deb` / `.rpm` packages, and a signed and
  notarized macOS app.

**Open source**

- `CONTRIBUTING.md`, a code of conduct, issue and PR templates.
- A CHANGELOG, semantic versioning, and a docs site.

## Out of scope (on purpose)

- **Multi-host orchestration** (Swarm / Kubernetes-style scheduling). Shipyard is a
  single-host engine.
- **Windows containers.** Shipyard is UNIX-only.
- **Building images.** Deferred until after stage 11; then a Dockerfile subset, built
  in-house.
- **Checkpoint/restore (CRIU)** and a **Kubernetes CRI** implementation. Possibly much later.

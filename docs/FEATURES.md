# Features

The complete set of functionality that makes Shipyard a production-grade, open-source
container engine. Each area names the [roadmap](ROADMAP.md) stage that builds it. This file
is the reference for *what* we build; the roadmap says *in what order*.

## Architecture

Shipyard has three layers, mirroring how the established tools split up:

| Layer | Equivalent | Responsibility |
|---|---|---|
| **Runtime** (`shipyard-runtime`) | runc / crun | Turns a root filesystem plus an OCI `config.json` into an isolated process. Implements the [OCI runtime spec](https://github.com/opencontainers/runtime-spec). |
| **Engine** (daemonless; `shipyard-engine`, `shipyard-image`, `shipyard-storage`; `shipyard-network` serves the runtime too) | Podman | Images, storage, networks, volumes, state, compose, API. Calls the runtime. |
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

- **User namespaces** (moved here from stage 2): `--uidmap`/`--gidmap` for root, and
  a bundle's `user` namespace with its mappings.
- **Rootless** (`run`, bundles and `shipyard-runtime`), for anyone without root:
  - root inside the container is the caller; IDs from 1 up are the caller's
    `/etc/subuid` and `/etc/subgid` ranges, mapped via `newuidmap`/`newgidmap`;
  - without the helpers or a range, only the caller's own ID, with a warning;
  - a bundle without a `user` namespace gets one; the runtime keeps its state in
    `$XDG_RUNTIME_DIR/shipyard`;
  - user namespaces refused by the host (AppArmor, `unprivileged_userns_clone`,
    `max_user_namespaces`) are explained.
- **Capabilities:** Docker's 14 by default, plus `--cap-add` / `--cap-drop`; a bundle's
  five sets exactly.
- **`no_new_privs`:** on by default; setuid binaries and file capabilities can't gain
  privileges.
- **seccomp:** Docker's profile by default, compiled to BPF in Rust; custom profiles
  (`--security-opt seccomp=FILE`, a bundle's `linux.seccomp`) and `unconfined`.
- **The init confines itself:** only `CAP_KILL`, `no_new_privs` and the container's
  seccomp filter, before the command goes on.
- **A session keyring per container,** as runc gives: the caller's keys stay out of
  reach.
- **Moved to stage 5:** device access control, an eBPF program attached to the
  container's cgroup, which exists only from stage 5.
- *Later:* AppArmor / SELinux labels, Landlock.

## 5. Resource control — stage 5

- **cgroup v2 limits** (v2 only; managed by writing `/sys/fs/cgroup` directly, without
  systemd's D-Bus; where cgroups can't be used, a run asking for no limits still works and
  a limit asked for is refused):
  - memory: `memory.max`, `memory.low` (a reservation), swap; any other controller file
    (`memory.high`, say) through OCI's `unified`
  - CPU: `cpu.max`, `cpu.weight`, cpuset
  - processes: `pids.max`
  - disk I/O: `io.max`, `io.weight` (where the kernel has it)
- **OOM detection:** via `memory.events`, reported clearly ("the command was killed: out of
  memory", exit 137; "(out of memory)" in the runtime's exit record).
- **Live stats:** CPU, memory, I/O and process count: `shipyard-runtime events --stats`
  now; `shipyard stats`, `top` and the UI with stage 9's named containers.
- **Change limits on a running container:** `shipyard-runtime update` now; `shipyard
  update` with stage 9.
- **Pause and resume:** via `cgroup.freeze`: `shipyard-runtime pause`/`resume` now;
  `shipyard pause` with stage 9.
- **Docker's limit flags on `shipyard run`** (`--memory`, `--cpus`, `--pids-limit`, …) and
  OCI's `linux.resources`, `unified` and `cgroupsPath`.
- **The runtime's other cgroup commands:** `ps`, `kill --all`; and OCI hooks (all six
  phases), carried from stage 3.
- **Device access control** (moved here from stage 4): an eBPF program attached to the
  container's cgroup (cgroup v2), allowing only the devices it should open.
- **Rootless cgroups:** delegation through systemd.
- **`oomScoreAdj`** and `--oom-score-adj`.
- **Cgroup namespaces** (moved here from stage 2): the container sees its own cgroup as
  the root.
- **Moved to `exec`'s stage (8):** joined processes entering the container's cgroup
  (and its device rules), so a cgroup kill catches anything that escaped the
  parent-death signal. Joining has no command until `exec`.

## 6. Images and storage — stage 6

**Images**

- **Formats:** the [OCI image spec](https://github.com/opencontainers/image-spec), plus
  Docker v2 manifests for compatibility (Docker's `null` lists included).
- **Registry client** ([OCI distribution spec](https://github.com/opencontainers/distribution-spec)):
  - Bearer token and Basic authentication
  - Docker Hub, GHCR and private registries, over HTTPS with the system's trust store
    (plain HTTP only on loopback)
  - credentials read from Docker's config: `auths`, `credsStore`, `credHelpers`
    (`docker-credential-*` helpers) and identity tokens; `shipyard login` later
- **Multi-arch:** pick the right platform from a manifest list (e.g. `linux/arm64`,
  with ARM variants).
- **Downloads:**
  - parallel, and resumable (HTTP `Range`) across retries and across pulls
  - sha256 verification of every blob, a digest pin checked, every size bounded
  - gzip decompression; zstd later (a zstd layer is refused by name)
- **Safe layer unpacking:**
  - every path reached by directory descriptor, never through a symlink; hard links
    only to the layer's own files
  - deletion markers: `.wh.` whiteout files and opaque directories, made as overlayfs
    reads them
  - owners, modes, times and file capabilities as the image gives them; rootless, in a
    user namespace with the caller's subordinate IDs
  - limits against archive bombs; the layer's digest and `diff_id` both checked
  - fuzzed (cargo-fuzz targets for the parsers and the unpacker)
- **Content-addressed store:** identical layers are stored once; shared safely by
  concurrent pulls, runs and collections.
- **Commands:** `pull`, `images`, `rmi`, `image prune`, `run --image` (with `--pull`
  and `--entrypoint`). `tag`, `inspect`, `history`, `save`/`load`, `import`/`export`
  move to stage 9.
- **Garbage collection:** reference-counted, plus `prune`; never what a running
  container or a pull in progress uses.
- *Later:* `push`, signature verification (cosign / sigstore).

**Storage**

- **overlayfs layer driver:** read-only image layers plus a per-container writable
  layer, mounted inside the container's namespaces (the new mount API on Linux 6.8+, a
  `mount(2)` fallback before), with a clear message when the system refuses it.
- **Rootless overlayfs:** native on kernel 5.11+. The `fuse-overlayfs` fallback moves
  to later: it needs a helper outside the container's PID namespace to be safe.
- **`--uidmap` with images** (as root): layers extracted again for the map.
- **Mount types:** named volumes and bind mounts (`--volume`), kept from removal while
  in use. tmpfs mounts and propagation options come with `--mount` (stage 9).
- **Mount options:** read-only/read-write.
- **Commands:** `volume create / ls / inspect / rm / prune`.
- **Disk accounting:** `system df` (images, containers' writable layers, volumes).

## 7. Networking — stage 7

- **Network namespaces** (moved here from stage 2): one per container, except in `host`
  mode; joined by path and `netDevices` moved in for OCI bundles; a sysfs of the
  container's own; `net.*` sysctls, with Docker's defaults.
- **Network modes:** `none`, `host`, `bridge` and networks made with `network create`
  (root), `pasta` (rootless's default). Rootless without pasta is refused with a hint.
- **Netlink of Shipyard's own:** rtnetlink (links, addresses, routes), nf_tables and
  conntrack messages built and read by bounded, fuzzed code; no `ip` or `nft` binary.
- **Bridges and veth pairs:** each container's interface born in its namespace with its
  final name; a fixed bridge MAC; hairpin.
- **IP address management:** a subnet per network (avoiding the host's routes), leases
  held by `flock` for the run's life and swept after a killed run (with its veth,
  firewall elements and conntrack entries); `--ip`, `--mac-address`.
- **Commands:** `network create [--subnet --gateway --internal --ipv6] / ls / inspect /
  rm / prune`.
- **Outbound traffic and ports:** one nf_tables table of Shipyard's own: masquerade,
  `-p`/`-P` through DNAT maps (host ports held by bound sockets), hairpin, localhost
  clients with the 127/8 guard, isolation between networks, internal (no-egress)
  networks, routing closed to all but containers; other firewalls detected and warned
  about with the fix.
- **Rootless networking:** pasta, one per container, attached by PID: outbound traffic,
  published ports, the host's DNS through `169.254.1.1`, IPv6 where the host has it;
  never the host's loopback. (`slirp4netns` isn't supported.)
- **Name resolution:**
  - a DNS server per user-defined network: containers resolve each other by `--name`,
    `--network-alias` and run ID (A, AAAA, PTR), across every network they share;
    everything else is forwarded to the host's resolvers (internal networks refuse)
  - generated `/etc/hosts`, `/etc/hostname` and `/etc/resolv.conf` (`--add-host`,
    `--dns`, `--dns-search`, `--dns-option`); the run ID as the default hostname
- **Network options:** several networks per container (`--network` repeated, or Docker's
  long form), internal networks, IPv6 (a ULA `/64` with NAT66, `--ip6`, `-p` on IPv6).
- **Connection observation:** a library reading `/proc/<pid>/net/{tcp,tcp6,udp,udp6}`
  through a container's init (pinned by a pidfd), listening and established sockets,
  fuzzed. Its command (and the UI's "observed but undeclared" dependencies) come with
  stage 9.
- *Moved:* `network connect`/`disconnect`, the sockets command and the DNS servers' own
  cgroup and restart to stage 9; `exec` joining the network namespace to stage 8.

## 8. Terminals, I/O and logs — stage 8

- **Per-container monitor process** (like Podman's conmon), which makes "daemonless" work.
  It:
  - holds the container's stdio after the CLI exits
  - records the exit status
  - writes logs
- **Pseudoterminals** for `-it`, with resize via `SIGWINCH` and the `TIOCSWINSZ` ioctl
  (built in step 8.2: made inside the container, its master passed back with
  `SCM_RIGHTS`; the user's terminal raw and restored).
- **OCI terminals** (built in step 8.3a): `process.terminal` and `consoleSize` in
  bundles; `shipyard-runtime create --console-socket`, runc's protocol (the master
  sent with `SCM_RIGHTS` and a name, no answer awaited).
- **`attach` / detach:** with detach keys.
- **`exec` sessions:** each with its own terminal.
- **`exec` joins the container's cgroup** (moved from stage 5): its limits, device rules
  and freezing cover what it runs, and a cgroup kill catches it.
- **`exec` joins the container's network namespace** (moved from stage 7), whatever its
  network mode.
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
- **Networks of named containers** (moved from stage 7): `network connect` /
  `disconnect` on a running container; `port` and a view of its sockets (stage 7's
  reader); `--name` for everything, not only DNS; the DNS servers in a cgroup of their
  own, restarted if killed; a warning when a container's pasta dies.
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

- `SECURITY.md` (done in v0.1, with a summary of what's defended) and a written threat
  model.
- `cargo-deny` and `cargo-audit` in CI.
- A software bill of materials (SBOM), signed releases, and reproducible builds.

**Distribution**

- Static (musl) Linux binaries, a Homebrew tap, `.deb` / `.rpm` packages, and a signed and
  notarized macOS app.

**Open source**

- `CONTRIBUTING.md` (done in v0.1), a code of conduct, issue and PR templates.
- A CHANGELOG and semantic versioning (done in v0.1), and a docs site.

## Out of scope (on purpose)

- **Multi-host orchestration** (Swarm / Kubernetes-style scheduling). Shipyard is a
  single-host engine.
- **Windows containers.** Shipyard is UNIX-only.
- **Building images.** Deferred until after stage 11; then a Dockerfile subset, built
  in-house.
- **Checkpoint/restore (CRIU)** and a **Kubernetes CRI** implementation. Possibly much later.

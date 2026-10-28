# Features

The complete set of functionality that makes Shipyard a production-grade, open-source
container engine. Each area names the [roadmap](ROADMAP.md) stage that builds it. This file
is the reference for *what* we build; the roadmap says *in what order*.

## Architecture

Shipyard has three layers, mirroring how the established tools split up:

| Layer | Equivalent | Responsibility |
|---|---|---|
| **Runtime** (`shipyard-runtime`) | runc / crun | Turns a root filesystem plus an OCI `config.json` into an isolated process. Implements the [OCI runtime spec](https://github.com/opencontainers/runtime-spec). |
| **Engine** (daemonless; `shipyard-engine`, `shipyard-image`, `shipyard-storage`; `shipyard-network` serves the runtime too) | Podman | Images, storage, networks, volumes, state, and the Docker-compatible API (`docker compose` runs through it; builds through BuildKit). Calls the runtime. |
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
  - names for commands run in an existing container (`exec`): numeric in 8.4a (runc's),
    names with stage 9 (from the container's `/etc/passwd`);
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
  now; `shipyard stats`, `top` (stage 11) and the UI by name.
- **Change limits on a running container:** `shipyard-runtime update` now; `shipyard
  update` by name: `--restart` in stage 9 (9.5b), the limits in stage 11.
- **Pause and resume:** via `cgroup.freeze`: `shipyard-runtime pause`/`resume` now;
  `shipyard pause` in stage 11.
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
  move to stage 12 (build and images).
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
  in use. tmpfs mounts and propagation options come with `--mount` (stage 11).
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
  about with the fix (firewalld and Docker's completed in stage 8.6, with `doctor`'s
  `firewall` line).
- **Rootless networking:** pasta, one per container, given the container's network
  namespace as a file (not attached by PID, which SELinux refuses): outbound traffic,
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
  fuzzed. Its command came with stage 9 (`port --sockets`, 9.7c1); the UI's "observed but
  undeclared" dependencies come with the UI (stage 14).
- *Moved:* `network connect`/`disconnect`, the sockets command and the DNS servers' own
  cgroup and restart to stage 9 (built: 9.7, 9.6c2); `exec` joining the network namespace to stage 8.

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
- **`attach` / detach:** with detach keys (built in step 8.3c, by runtime ID:
  `shipyard-runtime attach`, `create --stdin [--stdin-once]`; the monitor serves
  `attach.sock`, `SO_PEERCRED`, 16 clients × 256 KiB, a versioned protocol; by name in
  stage 9, 9.4c1). The monitor's own memory stays under 1 MiB idle; up to 4 MiB more with 16
  clients behind.
- **`exec`, confined** (built in step 8.4a, by runtime ID: `shipyard-runtime exec`, root
  and rootless; a terminal for it in 8.4b; by name in stage 9, 9.4c1): it joins every
  namespace the container has (the user one first) and starts in its cgroup
  (`clone3(CLONE_INTO_CGROUP)`, moved from stage 5: its limits, device rules and freezing
  cover it, a cgroup kill catches it; its network namespace, moved from stage 7,
  whatever the network mode). Confined as the container's process from a versioned
  snapshot (`exec.json`), narrowed never widened; non-dumpable and holding no host
  descriptor until it runs, a clone of the sealed binary (F6).
- **`exec` sessions:** each with its own terminal (built in step 8.4b: `exec -t`, made
  in the container's own devpts by exec's joiner, relayed, or sent to
  `--console-socket`).
- **File-descriptor passing:** `--preserve-fds` and systemd-style socket activation
  (`LISTEN_FDS`) (built in step 8.5b: `shipyard-runtime create`/`exec` and `shipyard
  run`; `LISTEN_PID` is the process's own PID, Shipyard's init being PID 1).
- **Logs:** log files with rotation; `logs --follow --tail --since --timestamps` (built
  in step 8.3b, by runtime ID: `shipyard-runtime create --log-dir`, CRI files with a
  versioned `log.json`, the monitor's poll loop; `logs` by name in stage 9, 9.4c1).
- **`cp`:** copy files into and out of a container (built in step 8.5a, by runtime ID:
  `shipyard-runtime cp`, Docker's forms and path rules, `-` for tar streams; done inside
  the container by a helper of Shipyard's own; `cp` by name in stage 9, 9.4c1; from a stopped container, 9.4c2).

## 9. Engine state (daemonless) — stage 9

- **On-disk state** (built in steps 9.2a–9.4a: records versioned from their first write,
  stage 8's still read):
  - persistent data in `/var/lib/shipyard` (rootless: `~/.local/share/shipyard`)
  - runtime data under `/run`
- **Locking:** `flock`, so concurrent `shipyard` commands never corrupt state (built in
  9.4a: the store's lock, each container's operation lock and its `running` lock).
- **Crash recovery:** on startup, compare recorded state with what's actually running
  (built in 9.5a: every command's recovery pass; `shipyard system recover`, 9.5c).
- **IDs:** full hex IDs, ISO 6346 display IDs, names and labels (built in 9.2b and 9.4a:
  a container is found by its ID, a prefix, its name or its serial; `rename`, 9.4b2).
- **Restart policies:** `no` / `on-failure` / `always` / `unless-stopped`, enforced by the
  monitor rather than a daemon (built in 9.5b: its exit command restarts it, with backoff;
  `update --restart`).
- **Health checks:** with health state exposed to the CLI and UI (built in 9.6b: a
  companion of the monitor's runs them; the image's `HEALTHCHECK` too; `ps`, `inspect`).
- **`events` stream:** start, stop, die, OOM, pull, … (built in 9.6a: a journal the
  commands write; `--since`, `--until`, `--filter`, JSON lines).
- **Commands across processes:** `kill`, `wait`, `stop` and `ps` work on containers started
  by other shipyard invocations, using the on-disk state (built in 9.4b1; `logs`, `attach`,
  `exec` and `cp` by name in 9.4c).
- **Networks of named containers** (moved from stage 7): `network connect` /
  `disconnect` on a running container; `port` and a view of its sockets (stage 7's
  reader); `--name` for everything, not only DNS; the DNS servers in a cgroup of their
  own, restarted if killed; a warning when a container's pasta dies (built in 9.6c and
  9.7).
- *Optional:* systemd units so containers start at boot (built in 9.5c, as shipped units in
  `contrib/systemd` rather than generated ones; each container's monitor in a cgroup of
  its own since 9.9b, so a shutdown stops every container gracefully).

## 10. Foundations and the Mac path — stage 10

Decided 2026-10-15 (ROADMAP D23, D24).

- **The refactor, with no behaviour change** (checked against the v0.2 binary):
  - one `take` ("find, lock, read again") for every command that works on a container;
  - one safe-file module: atomic writes, capped no-follow reads, lock files, one `pidfd`
    helper, using `rustix` beside `nix` (closes F61's reads);
  - `doctor`'s checks into the engine (`doctor::checks`, for the UI's backend panel),
    attach's and logs' runtime halves into the runtime (`cp`'s stays: F69),
    `run/detached.rs` split by job;
  - F48's list.

  Done as step 10.1 (2026-10-16), the record's split from `Config` moved to stage 11
  (D10.1d-1).
- **Stage 9's book chapter:** skipped at the user's request (D10-close-4).
- **The F-rows homed in stage 10:** done as step 10.2 (a–f, 2026-10-16 to 10-17).
- **A minimal macOS path, used daily** (D24), done as step 10.3 (2026-10-17 to 10-18;
  [macOS](user/macos.md), [notes](stages/10-foundations.md)):
  - Virtualization.framework driven from Rust (`objc2-virtualization`), in a VM-host
    process that starts on first use and stops after 5 minutes idle (D10.3-11);
  - one shared VM: an Alpine 3.24 initramfs root with Alpine's `linux-virt` kernel and a
    data disk for the engine's state (D10.3-2), built by a script from pinned sources,
    running the static `shipyard` as its init and agent;
  - the Mac CLI forwards every command to the guest's `shipyard` over vsock, its output,
    exit code, input, signals and terminal (raw, resized) as on Linux;
  - the user's home shared at its own path (virtiofs, D10.3-3), the VM's own files in it
    hidden from containers; published TCP ports on the Mac's loopback (D10.3-10);
  - the engine's kernel needs checked in the guest by its own `doctor` (cgroups v2,
    overlayfs, nf_tables bridges), shown by a Mac's `doctor` (D10-close-1); virtiofs and
    vsock exercised by every VM test.

## 11. Docker compatibility — stage 11

Decided 2026-10-15 (ROADMAP D25–D27). **Done** 2026-10-23, steps 11.1–11.6
([notes](stages/11-docker-compat.md)): the API, the drop-in CLI, compose and
Testcontainers, and Docker's clients on the Mac; what's left moved to stages 12, 13 and
15 (the notes' "moved out" table).

- **The container record split from `Config` and from the API's types** (moved from
  stage 10, D10.1d-1), so a record version is a frozen type of its own, before any v4.
- **The Docker Engine API, as a subset,** over a UNIX socket that starts on first use and
  exits when idle (systemd socket activation on Linux; forwarded from the VM over vsock
  on macOS), plus an event stream. Shipyard's own data (serials, health history, observed
  sockets) under `/shipyard/…`. The subset is what `docker compose` and Testcontainers
  use, listed in the stage's plan.
- **Docker's clients on the Mac use this API** (step 11.5b): served on a socket in the
  VM's directory and relayed to the guest's service. The Mac's `shipyard` keeps stage
  10's forwarding (D11.5b-1): its move to the API is stage 13's.
- **The CLI, a drop-in for `docker`** on the everyday commands (`run`, `create`, `start`,
  `stop`, `rm`, `ps`, `logs`, `exec`, `inspect`, `images`, `pull`, `compose`…): Docker's
  flags and short forms (`-v`, `-h`, `--env-file`, `-l`, `logs -n`, `inspect -f`,
  `--mount`) and output formats; Shipyard-only fields only in its own formats or behind
  flags.
- **`stats`, `top`, `pause`/`unpause` and live `update`** by name.
- **Compose is Docker's `docker compose`** run against Shipyard's API; `shipyard compose`
  finds and runs it.
- **Acceptance tests:** `docker compose` and Testcontainers suites against Shipyard.

## 12. Build and images — stage 12

Decided 2026-10-15 (ROADMAP D28). **Done** 2026-10-25, steps 12.1–12.5
([notes](stages/12-build-images.md)): build, push and sign-in, archives; what's left moved
to stage 15 (the notes' "moved out" table: F156–F162).

- **`shipyard build`:** BuildKit, started on demand in a container and removed after the
  build (one per build, in a user namespace of its own: D12-2), its cache kept per build
  context (a directory of the store's, not a volume: D12.3-5); Dockerfiles in full
  (multi-stage, cache mounts, secrets) as BuildKit reads them; `-o` for files instead of an
  image. On a Mac too, through its VM.
- **Images:** `push`, `tag`, `login`/`logout` (where `docker login` keeps them; a Mac's
  credential handed to its VM's push or pull), `image inspect` / `history` / `save` /
  `load`, `import`/`export`.

## 13. macOS production — stage 13 (built)

Decided 2026-10-15 (ROADMAP D29). Apple Silicon only.

- **Rosetta:** amd64 images on Apple Silicon, when asked (`--platform linux/amd64` on
  `pull`, `run`, `create`, `build`) or when an image has no arm64 variant (pulled as amd64,
  with a warning: D13-2).
- **Memory:** the VM's size from the Mac's (a quarter of its memory, half its cores),
  `vm set` to change it. The balloon was tried and not kept: macOS took nothing back
  (D13.2-2, F168).
- **Clock sync after sleep:** the host gives the guest the Mac's time with each look at
  its ports, over vsock.
- **File sharing:** the home, `/Volumes`, `/tmp` and `/var/folders`, at their own paths, so
  bind mounts of macOS paths just work, and a bind outside them refused as Docker Desktop
  refuses it. virtiofs as it is: nothing to tune (D13.4-2; OrbStack's speed with small
  files not met, F172).
- **Ports** from macOS into the VM: TCP and UDP on the Mac's loopback, dialled at each
  container; the guest's own address answers none.
- **Lifecycle:** the VM starts on first use and stops when idle; with `vm install
  --at-login`, at login too while containers would come back.
- **The Mac's `shipyard` forwarded, its wire fixed** (D13-1: the move onto the API, D11.5b-1,
  is dropped; Docker's clients use the API, the Mac's `shipyard` runs the same `shipyard`
  in the VM): signals, resizes and hang-ups on a channel of their own (F94), an older
  guest's refusals said plainly (F167).
- **Entitlements and signing** (`com.apple.security.virtualization`): a stable signing
  identity across rebuilds (a self-signed certificate in the keychain), so macOS asks its
  privacy questions once; Developer ID signing and notarization scripted, run at release in
  stage 15 (D13-3).
- **The macOS version floor:** macOS 13 Ventura (decided in step 10.3, D10.3-4: what
  Rosetta in Virtualization.framework needs).

## 14. Desktop UI — stage 14

Built ([notes](stages/14-ui.md)): the map, the Yard, the Manifest and each container's
inspector with its actions, logs and shell; Images, Volumes and Networks; the live numbers;
on a Mac and on Linux. Each container's rx/tx rate on the map and an installer moved to
stage 15.

- **Built on the stage 11 API.** The spec is [`design/README.md`](design/README.md), with
  an interactive mock in [`design/shipyard-ui.html`](design/shipyard-ui.html).
- **The observed-traffic map first:** dependencies seen in containers' sockets, declared
  or not, the feature no other engine has.
- **Built with Tauri 2 and Svelte** (D14-1, D14-3): a Rust core that's a client of the API
  (D14-6), small binaries; macOS first, Linux by the stage's close (D14-2).
- **Declared links from compose's own label** (`com.docker.compose.depends_on`); observed
  ones from each container's socket tables, polled (D14-5); the fix for an undeclared one
  is the YAML to copy (D14-4).

## 15. CLI — grows with every stage

- **Containers:** `run, create, start, stop, restart, kill, rm, ps, logs, exec, attach,
  pause, unpause, inspect, stats, top, cp, port, wait, rename, update`.
- **Other resources:** `image …`, `volume …`, `network …`, `compose …`.
- **System:** `system info / df / prune / events`, and `doctor`.
- **Output:** `--format json|table`, quiet modes, consistent exit codes.
- **A drop-in for `docker`** on the everyday commands (stage 11, D26).
- **Shell completions** (bash / zsh / fish) and man pages.

## 16. Production quality — ongoing from stage 1, finalized in stage 15

**v1.0** (ROADMAP D30), the first public release:

- **Platforms:** Linux (x86-64 and arm64, cgroup v2, kernel ≥ 5.11) and macOS on Apple
  Silicon.
- **The developer loop:** `alias docker=shipyard` works for `build`, `run`, `ps`, `logs`,
  `exec`, `push`, `pull` and `compose`; Testcontainers and devcontainers work against the
  API; x86 images run through Rosetta.
- **The Linux host:** rootless by default, restart policies, boot units, packages.
- **The desktop UI,** with its observed-traffic map.
- **Quality:** green CI, OCI conformance as expected, fuzzing and mutation checks on
  every parser and state machine, signed reproducible releases, every open finding fixed
  or explicitly accepted.
- **Performance targets** (below) measured and reported; they don't block v1.0.

**Correctness**

- Unit tests per stage, and integration tests run in a Linux VM in CI (CI is off until
  before v1.0, ROADMAP D31: local verification is the check of record until then).
- Pass the OCI runtime-spec conformance suite
  ([`runtime-tools`](https://github.com/opencontainers/runtime-tools)): it runs since
  stage 8 (H5), gated by `scripts/conformance/expected.txt`. By v1.0, every test is
  `pass`, or a refusal by design.
- Fuzzing of every parser: manifests, tar, compose, config.

**Performance targets** (benchmarked; reported, not blocking v1.0)

- Container start in under 100 ms, excluding image pull.
- Idle cost of zero processes besides the per-container monitors.
- Monitor memory under 1 MiB each (its own memory, idle; shared libraries not counted).

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
- **A builder or a compose engine of Shipyard's own.** Images are built by BuildKit
  (stage 12) and stacks run by `docker compose` against Shipyard's API (stage 11).
- **Intel Macs.** macOS support is Apple Silicon only (Rosetta exists only there).
- **Checkpoint/restore (CRIU)** and a **Kubernetes CRI** implementation. Possibly much later.

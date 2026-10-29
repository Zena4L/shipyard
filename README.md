# Shipyard

A lightweight, daemonless container engine written in Rust: a Docker-compatible
alternative to Docker, Podman and OrbStack, for developers on macOS and for Linux hosts,
with a desktop app that maps which of your containers talk to which.

> **Status: v0.5** (0.5.0, an internal milestone; nothing is released yet). Stage 15,
> hardening and the road to **v1.0, the first public release**, is under way.
>
> **What works today:**
>
> - **On Linux:** `shipyard run alpine sh` pulls an image and runs it **rootless** by
>   default, isolated in its own namespaces, confined (capabilities, `no_new_privs`,
>   seccomp), resource-limited (cgroup v2) and **networked** (pasta without root; bridge
>   networks, container names and IPv6 as root; published ports). Containers run in the
>   background by name (`run -d`, `ps`, `logs`, `exec`, `attach`, `cp`, `stop`, `rm`…),
>   with **restart policies**, containers back after a reboot ([systemd units](contrib/systemd/README.md)),
>   **health checks**, `events`, volumes, private registries, OCI bundles and an OCI
>   runtime (`shipyard-runtime`, as runc).
> - **Docker-compatible:** Docker's Engine API (a subset), served on demand and gone when
>   idle, so **Docker's own `docker` CLI, `docker compose` and Testcontainers** work
>   against Shipyard; and `shipyard` itself takes Docker's everyday commands and flags
>   (`alias docker=shipyard`).
> - **Images:** `shipyard build` from a Dockerfile (BuildKit, in a container of its own
>   per build), `push`, `login`/`logout`, `save`/`load`, `import`/`export`, `history`.
> - **On an Apple Silicon Mac:** every command runs in a small Linux VM of Shipyard's
>   own, started on first use and stopped when idle: your home and `/Volumes` shared,
>   ports on your Mac's `localhost`, amd64 images through Rosetta, Docker's tools through
>   the VM's `docker.sock` ([macOS](docs/user/macos.md)).
> - **The desktop app**, on a Mac and on Linux: a **map** of your containers, the
>   dependencies compose declares and the connections Shipyard sees in their sockets
>   (an undeclared one flagged, with the `depends_on` lines to add), each container's
>   inspector (its namespaces and cgroup as the kernel has them, live CPU and memory,
>   actions), its logs and a shell, and your images, volumes and networks
>   ([desktop app](docs/user/desktop.md)).
>
> **Not yet:** packages and signed releases (stage 15). The Mac app isn't notarized:
> you allow it once. It isn't a hardened security boundary yet: see
> [SECURITY.md](SECURITY.md). Everything that works is in the
> [user docs](docs/user/README.md), with its [limitations](docs/user/README.md#limitations).

The project is built step by step, from first principles, with
[*The Linux Programming Interface*](https://man7.org/tlpi/) as the primary reference.
See the [roadmap](docs/ROADMAP.md) and the [changelog](CHANGELOG.md).

## Quick start

Shipyard is built from source for now ([Rust](https://rustup.rs) installs the pinned
version on the first build).

### Linux

You need Linux 5.11 or later with cgroup v2, and for rootless runs `passt` and `uidmap`
(the full list, for Debian, Ubuntu and Fedora, is in
[Requirements](docs/user/README.md#requirements)).

```sh
git clone https://github.com/Zena4L/shipyard.git && cd shipyard
cargo build --release
sudo install -m 0755 target/release/shipyard target/release/shipyard-runtime /usr/local/bin/

shipyard doctor                          # can this host run containers? what to fix
shipyard run alpine sh -c 'cat /etc/alpine-release; id; wget -qO- example.com | head -c 60'
```

On Ubuntu 23.10 and later, install Shipyard's AppArmor profile first
([why](docs/user/README.md#ubuntu-2310-and-later-apparmor)); `shipyard doctor` says
so if it's needed.

### macOS (Apple Silicon, macOS 13 or later)

The VM's Linux guest is built from the tree too (in a container: Docker or OrbStack),
and `shipyard` must be signed to start a VM:

```sh
git clone https://github.com/Zena4L/shipyard.git && cd shipyard
cargo build --release
scripts/guest/build.sh                   # the VM's guest, into target/guest
scripts/macos/sign.sh target/release/shipyard

target/release/shipyard run --rm alpine echo hello    # the first command starts the VM
```

[Shipyard on a Mac](docs/user/macos.md) covers the VM, your files, ports, Rosetta, and
`shipyard vm install` (Docker's socket through launchd, and an optional start at login).

### With Docker's own tools

```sh
alias docker=shipyard                    # Shipyard takes the everyday docker commands
shipyard compose up -d                   # Docker's own `docker compose`, against Shipyard

# Or Docker's CLI and Testcontainers against Shipyard's API:
export DOCKER_HOST=unix://$XDG_RUNTIME_DIR/shipyard/api.sock                      # Linux, rootless
export DOCKER_HOST="unix://$HOME/Library/Application Support/Shipyard/vm/docker.sock"  # a Mac
```

How the API starts and what it serves: [the Docker API](docs/user/api.md) and
[Compose](docs/user/compose.md).

### The desktop app

It needs Node.js 26 (and WebKitGTK 4.1 on Linux):

```sh
cd desktop/ui
npm ci
SHIPYARD_BIN="$PWD/../../target/release/shipyard" npm run tauri dev
```

On a Mac, `npm run tauri -- build --bundles app` makes `Shipyard.app`
(`scripts/macos/sign.sh --bundle` signs it). See [the desktop app](docs/user/desktop.md).

## Goals

- **For macOS developers and Linux hosts:** native on Linux, and on Apple Silicon Macs
  through one lightweight Linux VM that Shipyard manages.
- **Daemonless, low overhead:** nothing runs while you aren't using it, beyond one small
  monitor process per container; fast startup, small binaries.
- **Docker-compatible:** a drop-in CLI for the everyday commands, and Docker's API so
  `docker compose`, Testcontainers and IDEs work against it; images built by BuildKit.
- **Simple, and visible:** a CLI and a desktop app that do the common things well, and
  show what the kernel is doing for your containers.
- **Built from scratch where it counts:** every kernel feature (namespaces, cgroups,
  mounts, capabilities, netlink, nf_tables, …) is implemented and understood directly,
  not wrapped; what established tools already do well (image builds, compose) is used,
  not rewritten.
- **Production-grade:** strict lints, documented `unsafe`, real end-to-end tests on Linux
  (as root and rootless) and in the Mac's VM, fuzzing of every parser, mutation checks,
  and CI.

## Repository layout

```
crates/
  shipyard-cli/            # the `shipyard` and `shipyard-runtime` binaries: arguments, output
  shipyard-engine/         # what every command does, behind typed APIs
  shipyard-api/            # Docker's Engine API (a subset), served by `shipyard system service`
  shipyard-runtime/        # processes, namespaces, mounts, cgroups, seccomp, the OCI runtime
  shipyard-image/          # registries, the content-addressed image store, safe unpacking, archives
  shipyard-network/        # its own netlink (rtnetlink, nf_tables), networks and leases, pasta, DNS
  shipyard-storage/        # named volumes
  shipyard-vm/             # the macOS VM (Virtualization.framework) and its host process
  shipyard-client/         # the desktop app's logic, as a client of the API
  shipyard-sys/            # shared plumbing: atomic writes, bounded reads, locks, pidfds, fd passing
  shipyard-acceptance/     # Docker's own clients (Testcontainers) against the API (dev only)
  shipyard-test-support/   # helpers for the end-to-end tests (dev only)
desktop/                   # the desktop app: a Tauri 2 shell and its Svelte page
contrib/                   # the AppArmor profile, systemd units, macOS entitlements
scripts/                   # the Linux test container, the Mac's guest build, signing, CI helpers
fuzz/                      # cargo-fuzz targets for every parser of untrusted input
docs/
  user/                    # what users read
  design/                  # the desktop app's design and its interactive mock
  stages/                  # the engineering record of each finished stage
  ROADMAP.md               # the staged plan, decisions and open findings
  FEATURES.md              # the feature list per stage, the quality bar, what's out of scope
  DEVELOPMENT.md           # local setup, including running Linux code from a Mac
```

## How it runs on each platform

Linux containers depend on Linux kernel features (namespaces, cgroups, overlayfs) that
macOS doesn't have. Every container tool on a Mac solves this the same way, and so does
Shipyard:

| Host  | CLI, engine and desktop app | Container isolation |
|-------|-----------------------------|---------------------|
| Linux (x86-64, arm64) | native | native, on the host's kernel |
| macOS (Apple Silicon) | native | inside one lightweight Linux VM that Shipyard manages, started on first use and stopped when idle ([macOS](docs/user/macos.md)) |

## Contributing and security

Contributions are welcome: [CONTRIBUTING.md](CONTRIBUTING.md) explains how, from a first
bug report to a merged pull request, and [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) has
every command. Everyone follows the [code of conduct](CODE_OF_CONDUCT.md). Report
vulnerabilities privately, as [SECURITY.md](SECURITY.md) says.

## License

Licensed under the [Apache License, Version 2.0](LICENSE). See [NOTICE](NOTICE).

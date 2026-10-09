# Shipyard

A lightweight, daemonless container engine written in Rust: a minimal-overhead
alternative to Docker / Podman / OrbStack with a small, clean interface.

> **Status: v0.1.** On Linux, `shipyard run --image alpine sh` pulls an image from
> Docker Hub and runs it **rootless**, isolated in its own namespaces, confined
> (capabilities, `no_new_privs`, seccomp), resource-limited (cgroup v2), and
> **networked** (pasta without root, bridge networks as root, published ports,
> container names on your own networks, IPv6). Volumes, private registries, OCI
> bundles and an OCI runtime (`shipyard-runtime`, as runc) work too.
>
> **Not yet:** detached containers, `exec` and logs (v0.2); isolation on macOS
> (v0.4). It isn't a hardened security boundary yet: see [SECURITY.md](SECURITY.md). Everything that
> works is in the [user docs](docs/user/README.md), and the
> [limitations](docs/user/README.md#limitations-in-v01) are listed there.

The project is built step by step, from first principles, with
[*The Linux Programming Interface*](https://man7.org/tlpi/) as the primary reference.
See the [roadmap](docs/ROADMAP.md) and the [changelog](CHANGELOG.md).

## Quick start (Linux)

You need Linux 5.11 or later with cgroup v2, and for rootless runs `passt` and
`uidmap` (the full list, for Debian, Ubuntu and Fedora, is in
[Requirements](docs/user/README.md#requirements)).

```sh
git clone https://github.com/Zena4L/shipyard.git && cd shipyard
cargo build --release
sudo install -m 0755 target/release/shipyard target/release/shipyard-runtime /usr/local/bin/

shipyard doctor                          # can this host run containers? what to fix
shipyard run --image alpine sh -c 'cat /etc/alpine-release; id; wget -qO- example.com | head -c 60'
```

On Ubuntu 23.10 and later, install Shipyard's AppArmor profile first
([why](docs/user/README.md#ubuntu-2310-and-later-apparmor)); `shipyard doctor` says
so if it's needed.

## Goals

- **macOS-first, UNIX everywhere**: first-class on macOS, native on Linux.
- **Low overhead**: no daemon, fast startup, small binaries.
- **Simple**: a CLI (and later a UI) that does the common things well.
- **Built from scratch**: every kernel feature (namespaces, cgroups, mounts,
  capabilities, netlink, nf_tables, …) is implemented and understood directly, not
  wrapped.
- **Production-grade**: strict lints, CI, documented `unsafe`, real tests, fuzzing
  and mutation checks.

## Repository layout

```
crates/
  shipyard-cli/            # the `shipyard` and `shipyard-runtime` binaries: arguments, UX, output
  shipyard-runtime/        # processes, namespaces, mounts, cgroups, seccomp, the OCI runtime
  shipyard-image/          # registries, the content-addressed image store, safe layer unpacking
  shipyard-network/        # its own netlink (rtnetlink, nf_tables), networks and leases, pasta, DNS
  shipyard-storage/        # named volumes
  shipyard-engine/         # the engine: orchestration above the runtime, behind typed APIs
  shipyard-test-support/   # helpers for the end-to-end tests (dev only)
contrib/apparmor/          # the AppArmor profile for hosts that restrict user namespaces
docs/
  user/                    # user-facing docs, published to the website
  ROADMAP.md               # staged build plan mapped to TLPI chapters
  FEATURES.md              # full feature list per stage, quality bar, scope
  DEVELOPMENT.md           # local setup (including running Linux code from macOS)
  stages/                  # the engineering record of each finished stage
  book/                    # the learning book: TLPI, containers and Rust, in plain English
  design/                  # UI concept
fuzz/                      # cargo-fuzz targets for every parser
```

## How it runs on each platform

Linux containers depend on Linux kernel features (namespaces, cgroups, overlayfs)
that macOS doesn't have. Every container tool on a Mac solves this the same way,
and Shipyard will too:

| Host  | CLI / engine / UI | Container isolation |
|-------|-------------------|---------------------|
| Linux | native            | native, on the host kernel |
| macOS | native            | inside a lightweight Linux VM managed by Shipyard (v0.4; until then, commands run unisolated) |

## Contributing and security

See [CONTRIBUTING.md](CONTRIBUTING.md) and [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).
Report vulnerabilities privately, as [SECURITY.md](SECURITY.md) says.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

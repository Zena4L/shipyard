# Changelog

All notable changes to Shipyard. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (before 1.0, a minor version may change
behaviour; each change is listed here).

## 0.1.0 - unreleased

The first milestone: `shipyard run --image alpine sh` rootless on Linux, on your own
terminal, pulled from Docker Hub, isolated, resource-limited and networked. Built in
stages 0 to 7 of the [roadmap](docs/ROADMAP.md), then hardened (stages 7.5 and 7.6).

### Added

- **Running commands** (stage 1): `shipyard run` starts a command with a clean
  environment, a chosen user, working directory and limits; passes signals on, stops it
  gracefully (`--stop-signal`, `--stop-timeout`), cleans up everything it started, and
  reports exactly how it ended (exit codes as Docker's).
- **Isolation** (stages 2 and 3): its own processes (under a small init), mounts,
  hostname and IPC; a root filesystem of its own (`--rootfs`), with `/proc`, `/sys`,
  `/dev`, masked and read-only paths; OCI bundles (`--bundle`); an OCI runtime,
  `shipyard-runtime` (`create`, `start`, `state`, `kill`, `delete`, `pause`, `resume`,
  `update`, `ps`, `events`), as runc.
- **Users and security** (stage 4): rootless containers (user namespaces, with your
  subordinate IDs); a minimal set of capabilities, `no_new_privs`, and a seccomp filter
  (Docker's profile by default, compiled by Shipyard itself).
- **Resource control** (stage 5): cgroup v2 limits (`--memory`, `--cpus`,
  `--pids-limit`, I/O, …), rootless where a subtree is delegated; OOM reporting;
  device access control (eBPF); OCI hooks.
- **Images and storage** (stage 6): `shipyard pull`, `images`, `rmi`, `image prune`,
  `system df`; private registries (Docker's credentials and helpers); resumable
  downloads; safe, verified layer unpacking (fuzzed); a content-addressed store shared
  by concurrent runs; overlayfs, rootless too; named volumes and bind mounts.
- **Networking** (stage 7): a network of the container's own, with a way out and
  published ports (`-p`, `-P`): bridge networks as root, over Shipyard's own netlink and
  nf_tables, and pasta without root; `shipyard network` commands; names and aliases on
  your networks (a DNS server per network); several networks per container; IPv6.
- **`shipyard doctor`** checks whether the host can run containers by trying what a
  run needs (a user namespace, a mount, an overlay next to the image store, a cgroup),
  and says what to fix; exit code 1 if something fails.
- **An AppArmor profile** (`contrib/apparmor/shipyard`) for Ubuntu 23.10 and later,
  which restrict unprivileged user namespaces.

### Security

Stages 7.5 and 7.6 fixed what a review of the whole codebase found
([notes](docs/stages/07.5-hardening.md)). Among them:

- the init runs from a sealed in-memory copy of the binary (CVE-2019-5736's class);
  every entry point refuses a setuid or file-capability start;
- a container can't send as another (a bridge anti-spoofing table), nor poison a
  shared image layer for other images;
- registry credentials never follow a redirect to another host;
- a symlink in an image can't steer a mount onto a kernel interface (CVE-2019-19921's
  class).

### Known limitations

- No terminal of the container's own (`-it`), no detached containers, `exec` or logs
  (v0.2).
- macOS doesn't isolate commands yet (v0.4).
- Rootless containers can't share a network.
- Not a hardened security boundary yet: see [SECURITY.md](SECURITY.md).
- Open findings are tracked in the [roadmap](docs/ROADMAP.md#open-findings).

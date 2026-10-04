# Shipyard

A lightweight container engine written in Rust — a minimal-overhead alternative to
Docker / Podman / OrbStack with a small, clean interface.

> **Status:** early development. `shipyard run` starts and supervises a command
> (clean environment, user, limits, signals, cleanup) and, on Linux, isolates it
> in its own process IDs, mounts and hostname; there's no separate filesystem
> yet. See [Running commands](docs/user/run.md). The project is being
> built step by step, from first principles, using
> [*The Linux Programming Interface*](https://man7.org/tlpi/) as the primary
> reference. See the [roadmap](docs/ROADMAP.md).

## Goals

- **macOS-first, UNIX everywhere** — first-class on macOS, native on Linux.
- **Low overhead** — no heavyweight daemon required, fast startup, small binary.
- **Simple** — a CLI (and later a UI) that does the common things well.
- **Built from scratch** — every kernel feature (namespaces, cgroups, mounts,
  capabilities, …) is implemented and understood directly, not wrapped.
- **Production-grade** — strict lints, CI, documented `unsafe`, real tests.

## Repository layout

```
crates/
  shipyard-cli/       # `shipyard` binary: argument parsing, UX, output
  shipyard-runtime/   # low-level Linux primitives: processes, namespaces, mounts, cgroups
docs/
  user/               # user-facing docs, published to the website
  ROADMAP.md          # staged build plan mapped to TLPI chapters
  FEATURES.md         # full feature list per stage, quality bar, scope
  DEVELOPMENT.md      # local setup (including running on macOS via a Linux VM)
  design/             # UI concept: shipyard-ui.html (open in a browser) + design notes
```

New crates (image handling, networking, the engine/API layer) will be added as the
roadmap reaches them.

## Documentation

User documentation lives in [`docs/user/`](docs/user/README.md) and is published to the website.

## Quick start

```sh
cargo build
cargo run -- doctor
```

## How it runs on each platform

Linux containers depend on Linux kernel features (namespaces, cgroups, overlayfs)
that macOS doesn't have. Every container tool on a Mac solves this the same way,
and Shipyard does too:

| Host  | CLI / engine / UI | Container isolation |
|-------|-------------------|---------------------|
| Linux | native            | native, on the host kernel |
| macOS | native            | inside a lightweight Linux VM managed by Shipyard |

The Linux runtime code is identical in both cases. On macOS it simply runs inside
the VM.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

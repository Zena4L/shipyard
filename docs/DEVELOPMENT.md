# Development

## Prerequisites

- Rust — the version is pinned in [`rust-toolchain.toml`](../rust-toolchain.toml);
  `rustup` installs it automatically on first build.
- macOS or Linux. The whole workspace builds and runs on both. Kernel isolation
  code (`cfg(target_os = "linux")`) only *executes* on Linux, so on a Mac you test it
  in a Linux VM (below).

## Everyday commands

```sh
cargo build                                # debug build
cargo run -- doctor                        # run the CLI
cargo test --workspace                     # tests
cargo fmt --all                            # format
cargo clippy --workspace --all-targets     # lint
cargo deny check                           # licenses & advisories (cargo install cargo-deny)

# Regenerate docs/user/cli.md after changing any command or flag
UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference

# API docs (also checked in CI with warnings as errors)
cargo doc --workspace --no-deps --open

# On macOS: type-check the Linux-only code without leaving the Mac
cargo clippy --workspace --all-targets --target aarch64-unknown-linux-gnu
```

CI runs, on every push and PR:

- fmt;
- clippy and tests on Linux and macOS;
- the tests again as root on Linux (see below);
- clippy for `x86_64-unknown-linux-musl`;
- the docs build (warnings as errors);
- cargo-deny.

### Tests that need root

Tests that switch user or groups are marked `#[ignore]`, so a normal
`cargo test` skips them. To run them (in the Lima VM, never on your Mac), let
cargo run the test binaries through `sudo`, so the build itself isn't root:

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER="sudo -E" \
  cargo test --workspace -- --include-ignored
```

(Use `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER` on an Intel machine.)

## Running Linux-only stages on macOS: Lima VM

Eventually Shipyard manages its own VM (roadmap stage 12). Until then,
[Lima](https://lima-vm.io) gives you a lightweight Linux VM with your home directory
mounted.

```sh
brew install lima
limactl start --name=shipyard --mount-writable template://ubuntu-lts
limactl shell shipyard
```

Inside the VM:

```sh
sudo apt-get update && sudo apt-get install -y build-essential
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
cd "/Users/$USER/Desktop/shipyard"   # the Mac's path, mounted by Lima

# Keep Linux build artifacts separate from the macOS ones in ./target
export CARGO_TARGET_DIR="$HOME/.cache/shipyard-target"
cargo run -- doctor
```

Lima mounts your Mac's home directory at the same path (`/Users/<you>`), not
at the VM's own `~`. Edit on macOS, build and run in the VM. Many later stages need root (or user
namespaces), so expect to use `sudo` inside the VM — never on your Mac.

## Conventions

- **`unsafe`** — every block needs a `// SAFETY:` comment (enforced by clippy).
- **Platform gating** — two levels:
  - `#[cfg(unix)]` for POSIX code (processes, signals, pipes, sockets). Must work on
    macOS *and* Linux.
  - `#[cfg(target_os = "linux")]` for kernel isolation (namespaces, mounts, cgroups).
    Other hosts get `Error::LinuxOnly`.
  - Non-UNIX targets are rejected at compile time.
- **Errors** — libraries use `thiserror` enums; the CLI uses `anyhow`.
- **Logging** — `tracing`; control with `-D/--debug`, `--log-level LEVEL` or `RUST_LOG`
  (flags win). `-v` is reserved for volumes.
- **Commits** — [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat:`, `fix:`, `docs:`, …).

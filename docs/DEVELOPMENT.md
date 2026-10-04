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

# On macOS: type-check the Linux-only code without leaving the Mac
cargo clippy --workspace --all-targets --target aarch64-unknown-linux-gnu
```

CI runs fmt, clippy, tests (Linux + macOS), and cargo-deny on every push and PR.

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
cd ~/Desktop/shipyard        # same path as on the Mac

# Keep Linux build artifacts separate from the macOS ones in ./target
export CARGO_TARGET_DIR="$HOME/.cache/shipyard-target"
cargo run -- doctor
```

Edit on macOS, build and run in the VM. Many later stages need root (or user
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
- **Logging** — `tracing`; control with `-v`/`-vv`/`-vvv` or `RUST_LOG`.
- **Commits** — [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat:`, `fix:`, `docs:`, …).

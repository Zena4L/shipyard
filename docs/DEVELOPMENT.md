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
- the tests again as root on Linux and macOS (see below);
- clippy for `x86_64-unknown-linux-musl`;
- the docs build (warnings as errors);
- cargo-deny.

### Terminal tests

`crates/shipyard-cli/tests/terminal.rs` tests Ctrl-C, Ctrl-Z, `fg`/`bg`,
hangup and signals on a real (pseudo-)terminal. A small fake shell, forked from
the test process, runs shipyard as a job and does job control on it like bash;
the test types keys into the terminal and reads what it shows.

```sh
cargo test -p shipyard-cli --test terminal              # all of them
cargo test -p shipyard-cli --test terminal ctrl_z       # by name
```

They don't use the standard test harness (`harness = false`): the fake shell
keeps running Rust code after `fork`, which is only safe in a single-threaded
process, and the standard harness runs tests on threads. So **never start a
thread in that test binary.**

### Running the Linux tests in a container

`scripts/test-linux.sh` runs what CI runs on Linux, in a `rust:1-bookworm`
container (Docker, OrbStack or any compatible engine):

1. clippy with warnings as errors;
2. the whole test suite as a normal user;
3. the suite again with the root-only tests, run as root (as the CI `root` job
   does). Before it, `/` is made a shared mount, as on systemd
   hosts, so a mount namespace that leaks mounts would be caught.

```sh
scripts/test-linux.sh            # everything; exits non-zero on any failure
scripts/test-linux.sh --shell        # a shell in the same container, to debug
scripts/test-linux.sh --root-shell   # the same, as root, with / made shared
```

The container is **privileged**, because creating namespaces and mounting
(stage 2 on) need `CAP_SYS_ADMIN` and Docker's default seccomp profile blocks
them. The normal-user pass still has no capabilities, so it checks what an
unprivileged user sees. Use it for development only.

The repository is mounted read-only. Build output, the cargo registry and the
pinned toolchain live in Docker volumes named `shipyard-linux-*`, so later runs
are fast. It's the quick way to run Linux-only code from a Mac; the Lima VM
(below) is for interactive work and the root-heavy later stages.

### x86-64, kernel probes and mutation checks

- **x86-64 from an Apple-silicon Mac:** the same image under emulation, with its own
  volumes. OrbStack's emulation (Rosetta) lacks `clone3` and `execveat`, so this run also
  exercises both fallbacks. It's slow (the first `apt-get` alone can take many minutes),
  and Rosetta itself can deadlock when a raw `clone` meets threads being created, so the
  one test that stresses exactly that skips itself there. The kernel also refuses every
  seccomp filter under emulation, so the test support runs isolated commands there with
  `--security-opt seccomp=unconfined`, and the filter's own tests skip. Treat it as a
  check of the fallback paths, not of concurrency or seccomp; CI runs real x86-64:

  ```sh
  docker run --rm --privileged --platform linux/amd64 -v "$PWD":/src:ro \
    -v shipyard-amd64-target:/target -v shipyard-amd64-cargo:/usr/local/cargo \
    -v shipyard-amd64-rustup:/usr/local/rustup -e CARGO_TARGET_DIR=/target -w /src \
    rust:1-bookworm bash -c 'apt-get update -qq && apt-get install -y -qq procps >/dev/null \
      && mount --make-rshared / && cargo test --workspace -- --include-ignored'
  ```
- **Kernel probes:** `scripts/probes/*.c` check the kernel behaviour a stage relies on,
  before it's built on. Each file's header says how to run it, usually in the privileged
  container. The stage notes record the results.
- **Mutation checks:** to prove a test catches a bug, break a scratch copy of the repo,
  never the working tree. Build it in its **own** cargo target volume (for example
  `shipyard-mutation-target`). Cargo decides what to rebuild by timestamps, so a mutated
  build in the shared volume can be reused by the next normal run.

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

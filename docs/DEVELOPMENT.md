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

# API docs (also checked in CI with warnings as errors, private items included)
cargo doc --workspace --no-deps --open
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items \
  --target x86_64-unknown-linux-gnu        # as CI checks them

# On macOS: type-check the Linux-only code without leaving the Mac
cargo clippy --workspace --all-targets --target aarch64-unknown-linux-gnu
# Since stage 6, this needs a Linux cross-compiler for ring's C code
# (`brew install aarch64-unknown-linux-gnu` / a `musl-cross` toolchain), or
# just run the check in the container (`scripts/test-linux.sh`), which builds
# natively and needs no cross-toolchain.
```

CI runs, on every push and PR:

- fmt;
- clippy and tests on Linux and macOS. On Linux, the job first allows rootless
  containers (lifts Ubuntu's AppArmor restriction on user namespaces, installs
  `uidmap` and `passt`, gives the runner a subordinate range), and runs the tests with
  `SHIPYARD_TEST_ROOTLESS=required`, so a rootless test that can't run fails instead
  of skipping;
- the tests again as root on Linux and macOS (see below);
- clippy for `x86_64-unknown-linux-musl` (with `musl-tools`: ring compiles C with `musl-gcc`);
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

The isolated ones run as root, and as a normal user wherever rootless containers
work there (a user namespace of one's own, the single-ID map: the fake shell clears the
environment, so it finds no `newuidmap`). As a normal user elsewhere they're skipped,
unless `SHIPYARD_TEST_ROOTLESS=required` makes that a failure.

They don't use the standard test harness (`harness = false`): the fake shell
keeps running Rust code after `fork`, which is only safe in a single-threaded
process, and the standard harness runs tests on threads. So **never start a
thread in that test binary.**

### Running the Linux tests in a container

`scripts/test-linux.sh` runs what CI runs on Linux, in a container (Docker, OrbStack
or any compatible engine). The image is `rust:1-trixie` plus what the
rootless tests need (`scripts/test-image/Dockerfile`: the `uidmap` package, `passt`
for rootless networking, and a `tester` user with subordinate IDs in
`/etc/subuid`). It's Debian 13, not 12: bookworm's pasta (from 2023) can't carry
connections to a published port into a container. The rootless networking tests
(`tests/pasta.rs`) run as root and start `shipyard` as `tester`, in a network
namespace of their own with an `Upstream` world. The script builds it the first
time, which needs network, and again only when the Dockerfile changes. The steps:

1. clippy with warnings as errors;
2. the whole test suite as a normal user (`tester`): the rootless tests, the
   rootless image runs and the isolated terminal tests run here. As in CI,
   `SHIPYARD_TEST_ROOTLESS=required` makes a rootless test that can't run fail
   instead of skip;
3. the suite again with the root-only tests, run as root (as the CI `root` job
   does). Before it, `/` is made a shared mount, as on systemd
   hosts, so a mount namespace that leaks mounts would be caught.

```sh
scripts/test-linux.sh            # everything; exits non-zero on any failure
scripts/test-linux.sh --shell        # a shell in the same container, to debug
scripts/test-linux.sh --root-shell   # the same, as root, with / made shared
scripts/test-linux.sh --fuzz [SECONDS]   # each fuzz target for SECONDS (300)
```

The container's `/tmp` is a fresh tmpfs: its own root is overlayfs, which can't hold
an image store's writable layers or a normal user's whiteout devices. A rootless
store on tmpfs needs Linux 6.6 or later (user extended attributes on tmpfs).

**cgroups** (stage 5) are set up first, as on a systemd host: the container's cgroup
namespace root can't hand controllers to children while it holds processes (the
kernel's "no internal processes" rule), so the script moves its processes into a leaf
(`/sys/fs/cgroup/init`), as docker-in-docker does, and enables every controller. And
`tester` gets a delegated subtree (`/sys/fs/cgroup/user.slice/tester`, its normal-user
pass and `--shell` starting in `user.slice/tester/session`), as systemd's `Delegate=`
gives each user `user@UID.service`, with systemd's default controllers only (`cpu
memory pids`). Rootless containers' cgroups go there. Only this container's cgroups are
touched.

CI's normal-user tests run the same way, against systemd's real delegation: the job
starts the runner user's systemd manager (`loginctl enable-linger`) and runs
`cargo test` in a scope inside it (`systemd-run --user --scope`).
`SHIPYARD_TEST_DELEGATED=required` (set there and in `test-linux.sh`'s normal-user
pass) makes a test fail, rather than take its no-delegation branch, if the delegated
cgroup is missing.

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
    rust:1-trixie bash -c 'apt-get update -qq && apt-get install -y -qq procps >/dev/null \
      && cg=/sys/fs/cgroup && mkdir -p $cg/init \
      && for p in $(cat $cg/cgroup.procs); do echo $p > $cg/init/cgroup.procs 2>/dev/null || true; done \
      && for c in $(cat $cg/cgroup.controllers); do echo +$c > $cg/cgroup.subtree_control || true; done \
      && mount --make-rshared / && cargo test --workspace -- --include-ignored'
  ```

  The cgroup lines do what `scripts/test-linux.sh` does: Docker leaves the container's
  processes in its cgroup namespace's root, which then can't hand controllers down
  (cgroup v2's no-internal-processes rule), so Shipyard refuses limits there.
- **Fuzzing:** `fuzz/` holds cargo-fuzz targets for what Shipyard reads from outside:
  `manifest` (manifests, indexes, configs), `challenge` (`WWW-Authenticate` headers and
  token answers), `reference`, `tar` (the tar reader), `unpack` (a layer extracted
  beside a canary directory, which must not change), `netlink` (what the kernel
  sends over a netlink socket: messages, attributes, errors, links, addresses,
  routes, nf_tables objects and conntrack entries), `ports` (`-p` specs and an
  image's exposed ports), `dns` (DNS messages, as a network's DNS server reads
  them, and its answers, which must read back) and `proc_net` (a container's
  `/proc/<pid>/net` socket tables). It's its own workspace, built on
  nightly, so it's not in CI. `scripts/test-linux.sh --fuzz` installs nightly and
  cargo-fuzz into the toolchain volumes once (that needs network), writes starting
  corpora with `scripts/fuzz-seeds.py`, and runs each target as `tester` (an escape
  reaches only what a normal user can). Corpora and crashing inputs are kept in the
  build volume, under `/target/fuzz`; turn a crash into a unit test in the module it
  found, then fix it. Running it under `fuzz/` on a host with nightly works too:
  `cargo +nightly fuzz run manifest`.
- **Kernel probes:** `scripts/probes/*.c` check the kernel behaviour a stage relies on,
  before it's built on. Each file's header says how to run it, usually in the privileged
  container. The stage notes record the results. `scripts/probes/pasta.sh` does the same
  for pasta (rootless networking), in a throwaway Debian container it installs `passt`
  into.
- **Mutation checks:** to prove a test catches a bug, break a scratch copy of the repo,
  never the working tree. Build it in its **own** cargo target volume (for example
  `shipyard-mutation-target`). Cargo decides what to rebuild by timestamps, so a mutated
  build in the shared volume can be reused by the next normal run.

### Reclaiming Docker space

The target volumes above are cargo build output and grow without bound; the mutation and
review volumes especially, since each run is a fresh scratch copy built in its own cache.
They hold only regenerable artifacts (the repository is mounted read-only), so they are
always safe to remove. `scripts/clean-docker.sh` does it, least to most:

```sh
scripts/clean-docker.sh                # scratch only: the mutation + review volumes
scripts/clean-docker.sh --phase        # also the amd64/ci targets (keeps shipyard-linux-* + toolchains)
scripts/clean-docker.sh --images       # also the shipyard-test image and dangling images
scripts/clean-docker.sh --all          # every shipyard-* volume and image (full rebuild next run)
scripts/clean-docker.sh --dry-run      # show what a level would remove, remove nothing
```

The default keeps the everyday `shipyard-linux-*` caches and the pinned toolchains, so the
next `test-linux.sh` stays fast. Run the default after a mutation or review session;
`--phase` between stages. Only `shipyard-*` volumes and `shipyard-test:*` images are touched.

### Tests that need root

Tests that switch user or groups are marked `#[ignore]`, so a normal
`cargo test` skips them. To run them (in the Lima VM, never on your Mac), let
cargo run the test binaries through `scripts/ci-netns.sh`, so the build itself
isn't root:

```sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER="$PWD/scripts/ci-netns.sh" \
  cargo test --workspace -- --include-ignored
```

(Use `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER` on an Intel machine.)

The script runs each test binary with `sudo`, in a network namespace of its own
(`unshare --net`, `lo` up), with the networks' state in a scratch directory
(`SHIPYARD_NETWORK_DIR`). It matters: as root, `shipyard run` puts containers on
the default bridge network, so it makes the bridge `shipyard0`, adds nftables
rules (the table `inet shipyard`) and turns on IPv4 forwarding. Run through plain
`sudo -E`, the root tests would do that to the machine's own network. CI's root
job uses the same script; `scripts/test-linux.sh` runs in a throwaway container
already. The network tests make their interfaces in namespaces of their own
(`in_new_netns`), with an `Upstream` namespace standing in for the internet
(`shipyard_test_support::net`).

### What CI runs

`.github/workflows/ci.yml`, on every push and pull request:

| Job | What it covers |
|---|---|
| `fmt`, `musl`, `docs`, `deny` | formatting; clippy for musl; rustdoc; dependencies |
| `check` (Ubuntu, macOS) | clippy and every normal-user test. Rootless tests run the binaries cargo built, so the AppArmor restriction on user namespaces is turned off here |
| `root` (Ubuntu, macOS) | the root tests (`--include-ignored`), each test binary in a network namespace of its own (`scripts/ci-netns.sh`) |
| `apparmor` | **A real Ubuntu host** (H4): the restriction left on, `shipyard` and `shipyard-runtime` installed in `/usr/local/bin` with the profile (`contrib/apparmor/shipyard`), as users install them. `scripts/ci/apparmor-control.sh` first proves the profile is what lets them make user namespaces (unloaded, `shipyard doctor` fails); then `shipyard-cli`'s tests run the installed binaries (the runtime's in-process tests make user namespaces from their own test binary, which no profile covers: they run in `check`) |
| `fedora` | **A real Fedora host** (H4): `scripts/ci/fedora-vm.sh` boots Fedora Cloud (pinned by SHA-256) in QEMU/KVM, SELinux enforcing, firewalld stopped (until stage 8.6), and runs the test executables built on the runner in both passes (`scripts/ci/run-test-binaries.sh`), plus the terminal probe |

**Required, not skipped.** CI and `test-linux.sh` set `SHIPYARD_TEST_ROOTLESS`,
`SHIPYARD_TEST_DELEGATED` and `SHIPYARD_TEST_TOOLS` to `required`: a rootless test that
can't run, a missing delegated cgroup, or a missing reference tool (`ip`, `nft`, `dig`,
for the differential tests) then fails instead of skipping.

**Testing installed binaries.** With `SHIPYARD_TEST_BIN_DIR=DIR`, the end-to-end tests
run `DIR/shipyard` and `DIR/shipyard-runtime` instead of what cargo built
(`shipyard_test_support::binary`), and the rootless tests ask that `shipyard doctor`
whether user namespaces work, instead of `unshare` (which no profile covers).

**The Fedora job by hand**, on an x86-64 Linux machine with KVM (`qemu-system-x86_64`,
`qemu-img`, `cloud-localds`, `jq`, `gcc`, and `/dev/kvm` yours: the kvm group):
`scripts/ci/fedora-vm.sh` (`VM_SSH_PORT` if 2222 is taken). It builds, boots a throwaway
VM, runs both passes there, and removes the VM; the image stays cached in
`~/.cache/shipyard-vm`.

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

**Testing a real distribution** (a host the test container can't stand for: Ubuntu's
AppArmor, its cgroup layout): a VM with Ubuntu's own kernel, and a static build copied
in, saves installing Rust there. Build `shipyard` for musl in the test container:

```sh
apt-get install -y musl-tools
CC_aarch64_unknown_linux_musl=musl-gcc cargo build --release -p shipyard-cli --bins \
    --target aarch64-unknown-linux-musl
```

Then, from the Mac:

```sh
limactl start --name=shipyard-ubuntu --cpus=2 --memory=3 template:ubuntu-24.04
limactl copy target/aarch64-unknown-linux-musl/release/shipyard shipyard-ubuntu:/tmp/
limactl shell shipyard-ubuntu -- sudo install -m 0755 /tmp/shipyard /usr/local/bin/
limactl shell shipyard-ubuntu -- sudo apt-get install -y uidmap passt
limactl shell shipyard-ubuntu -- shipyard doctor
limactl delete --force shipyard-ubuntu                  # when done
```

(The test container's target directory is a Docker volume: copy the build out of it
first.) On Ubuntu 23.10 and later, rootless runs need `contrib/apparmor/shipyard`
installed (see [Requirements](user/README.md#requirements)); it attaches to
`/usr/local/bin`, hence the install there. An SSH session's cgroup isn't delegated:
run Shipyard under `systemd-run --user --scope` there for limits.

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

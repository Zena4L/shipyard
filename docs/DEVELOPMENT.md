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
scripts/test-linux.sh --conformance [NAME…]  # the OCI conformance suite (below)
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
  them, and its answers, which must read back), `proc_net` (a container's
  `/proc/<pid>/net` socket tables), `cri_record` and `logs_dir` (log records and
  directories), `attach_frames` (the attach protocol's frames and detach keys),
  `process_json` (`exec --process` documents) and `cp_unpack` (what `cp` copies into
  a container). `scripts/fuzz-targets.sh` lists them, from `fuzz/Cargo.toml`: the one
  list everything runs. It's its own workspace, built on nightly, so CI's `check`
  doesn't build it; the `Quality` workflow runs it (below). `scripts/test-linux.sh --fuzz` installs nightly and
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
  - **Each step's own runner** (the shipyard-dev skill's `mutation-runner.py`): chosen
    mutants of the step's risky lines, run where their tests run (macOS, `tester` or
    root in the container), controls first. The only check of root-only behaviour.
  - **cargo-mutants on the changes** (H5): `scripts/test-linux.sh --mutants [BASE]`
    mutates what changed against `BASE` (`main` by default; uncommitted work counts),
    or a diff file's changes (`--mutants FILE.diff`), with each crate's own tests, as
    `tester`, and prints caught, missed and timed out. `.cargo/mutants.toml` leaves out
    the tests, the test helpers, the fuzz crate and the code between `fork` and `exec`.
    It's a report, not a gate: a mutant only a root test catches shows as missed.

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

### The OCI conformance suite

`scripts/test-linux.sh --conformance` runs the OCI runtime conformance suite
([runtime-tools](https://github.com/opencontainers/runtime-tools)' `validation/`)
against `shipyard-runtime`, as root, in the same privileged container. The suite
drives the runtime through `create`, `start`, `state`, `kill` and `delete`, and runs a
static program (`runtimetest`) as each container's process, which compares what it
finds inside with the config.

- **Its image** (`scripts/conformance/Dockerfile`) adds Go and builds runtime-tools,
  pinned by commit, plus a static busybox root filesystem (runtime-tools ships one for
  amd64 and 386 only). It's built the first time, which needs network.
- **`scripts/conformance/expected.txt`** is the gate. It lists every test of the
  suite as `pass`, `fail /PATTERN/ REASON` or `skip REASON`, and the run fails on any
  difference, both ways:
  - a `fail` must fail for its reason: PATTERN (`grep -E`) must match the test's TAP
    or stderr;
  - if a fix makes a `fail` pass, the run fails until the line says `pass`;
  - a timeout is always a difference.

  A `fail` names a finding (ROADMAP's table), a refusal by design, runc's behaviour,
  or a bug in the test. A `skip` names why it isn't run (cgroup v1 only, a hang or
  race in the suite). `NAME@arm64 …` holds for one architecture only.
- **Passing, exactly:** a test exits 0 with no failing results, none skipped, and a
  plan that counts them. The hook tests and two `create`-refusal tests print no
  results; for those, any diagnostic with an `"error"` is a failure.
- **One test or a few:** `--conformance create kill`. A `skip` test runs too, when
  named, to recheck it. Each test's TAP and stderr are kept in the build volume, under
  `/target/conformance` (cleared at each run), and a difference prints its end.
- **Architectures:** `expected.txt` was recorded on arm64 (Apple silicon). The CI job
  runs amd64, which nothing has checked yet while CI is off: lines may need an
  `@amd64` twin when it runs.
- **A mutant of the runtime must build in a scratch copy:** `--conformance` builds into
  the shared target volume, and cargo goes by modification times. Afterwards, run
  `cargo clean -p shipyard-cli -p shipyard-runtime` there, or the next run tests the
  mutant.

### What CI runs

`.github/workflows/ci.yml`. **Manual only for now** (`gh workflow run CI --ref
BRANCH`): the repository's Actions billing is off, so a push would start nothing. Its
push and pull-request triggers are written, commented out. `check` and `root` run on
macOS only for now (their Ubuntu runs took too long); the Linux tests run in the
`apparmor` and `fedora` jobs, and locally in `scripts/test-linux.sh`.

| Job | What it covers |
|---|---|
| `fmt`, `musl`, `docs`, `deny` | formatting; clippy for musl; rustdoc; dependencies |
| `check` (macOS; Ubuntu off) | clippy and every normal-user test. Rootless tests run the binaries cargo built, so the AppArmor restriction on user namespaces is turned off here |
| `root` (macOS; Ubuntu off) | the root tests (`--include-ignored`), each test binary in a network namespace of its own (`scripts/ci-netns.sh`) |
| `apparmor` | **A real Ubuntu host** (H4): the restriction left on, `shipyard` and `shipyard-runtime` installed in `/usr/local/bin` with the profile (`contrib/apparmor/shipyard`), as users install them. `scripts/ci/apparmor-control.sh` first proves the profile is what lets them make user namespaces (unloaded, `shipyard doctor` fails); then `shipyard-cli`'s tests run the installed binaries (the runtime's in-process tests make user namespaces from their own test binary, which no profile covers: they run in `check`) |
| `conformance` | the OCI conformance suite, as `expected.txt` expects (amd64; recorded on arm64) |
| `fedora` | **A real Fedora host** (H4): `scripts/ci/fedora-vm.sh` boots Fedora Cloud (pinned by SHA-256) in QEMU/KVM, SELinux enforcing, firewalld running, and runs the test executables built on the runner in both passes (`scripts/ci/run-test-binaries.sh`), plus the terminal probe |

`.github/workflows/quality.yml` (`Quality`), the slow tools (H5), manual too (`gh
workflow run Quality --ref BRANCH [-f fuzz_seconds=600] [-f base=main]`), its nightly
schedule written, commented out, for when billing is back:

| Job | What it covers |
|---|---|
| `fuzz` | every target in `scripts/fuzz-targets.sh` for `fuzz_seconds` (300), from `fuzz-seeds.py`'s corpora; a crash fails it, its input uploaded |
| `mutants` (Ubuntu, macOS) | cargo-mutants on the branch's changes against `base` (`.cargo/mutants.toml`); caught, missed and timed out in the run's summary, `mutants.out` uploaded. A report, never a failure |

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

### Unattended checks while CI is off

`scripts/watch.sh` does at night what CI would (ROADMAP D31): it checks `origin/main` in a
clone of its own (`~/Library/Caches/shipyard-watch`), with its own cargo target dir and
Docker volume (`shipyard-watch-target`), so it never touches your working tree or your
build volume. Each night it verifies the whole workspace when `main` moved (`verify.sh`:
macOS, both Linux passes, rustdoc, musl), fuzzes every target for 10 minutes (the corpora
grow night after night), and runs cargo-mutants on what changed since its last sweep.

```sh
scripts/watch.sh --install    # a launchd agent: every night at 02:00 (a missed night runs on wake)
scripts/watch.sh --quick      # the plumbing only, now: the macOS checks and 20 s of fuzzing
scripts/watch.sh              # a full run, now
scripts/watch.sh --uninstall
```

The report is `~/Library/Caches/shipyard-watch/runs/latest/report.md`. It notifies you
only when something got worse (a check that passed now fails, a fuzz crash, missed
mutants) or when it couldn't run at all (Docker down, no network): silence means it ran
and nothing regressed. Docker must be running at night; the Mac is kept awake for the run.

## Shipyard's own VM on a Mac (step 10.3)

On an Apple Silicon Mac, Shipyard runs containers in one Linux VM of its own,
booted with Virtualization.framework. Two things make it work from a checkout:

**The guest**, built once (and again after a change to the engine's code: the
guest runs a static `shipyard` of this tree's):

```sh
scripts/guest/build.sh            # in the test container; writes target/guest/
```

It's Alpine's `linux-virt` kernel (unpacked: the framework boots a raw arm64
`Image`) and an initramfs holding the whole guest: Alpine's minirootfs, the
kernel's modules, `e2fsprogs` and the static `shipyard` as `/init`. What it's
built from is pinned in `scripts/guest/alpine.lock`, and every package it installs,
dependencies too, in `scripts/guest/alpine.installed` (each by its identity in apk's
database: a build that installs anything else fails, saying what; after a re-pin,
`UPDATE_GUEST_LOCK=1 scripts/guest/build.sh` writes it again); `manifest.json` beside it says
what it was made of, and the VM checks the files against it before it boots. On a
network that breaks Alpine's TLS (one met in 13.2 drops OpenSSL 3.5's post-quantum
hello), `SHIPYARD_GUEST_MIRROR=http://dl-cdn.alpinelinux.org/alpine scripts/guest/build.sh`
fetches the packages over plain HTTP: `apk` still checks each one's signature.

**The entitlement.** macOS gives a VM only to a binary signed with
`com.apple.security.virtualization` (`contrib/macos/shipyard.entitlements`). The
linker's own signature has none, so every build is signed again:

```sh
scripts/macos/sign.sh             # every shipyard binary under target/
```

`cargo run` and `cargo test` do it themselves: `.cargo/config.toml` makes
`scripts/macos/sign-and-run.sh` cargo's runner on a Mac. A binary the tests start
(`CARGO_BIN_EXE_shipyard`) isn't run by cargo: the VM tests sign it first.

**A signing identity of your own** (step 13.7). Signed ad hoc, each rebuild is a new
program to macOS's privacy permissions, so a VM launchd started (`shipyard vm install`)
is asked again for your Desktop, Documents and Downloads after every rebuild. Make one
identity, once, and every build is signed with it:

```sh
scripts/macos/make-signing-identity.sh   # "Shipyard Local Signing", in your login keychain
```

It's a self-signed certificate for code signing, its key kept in the keychain: nothing
is trusted system-wide, and what it signs is for this Mac alone. It needs OpenSSL 3
(`brew install openssl@3`: macOS's own `openssl` is LibreSSL), and asks for your
keychain's password once, there, so `codesign` may use this key from then on without a
dialog (a background build never waits on one); every other key in the keychain is left
as it is, and the script says how many it changed (one). Any program you run can then
sign with it too (SECURITY.md says what that reaches). `sign.sh` then signs with it, by its
SHA-1 (`SHIPYARD_SIGN_IDENTITY` names another identity; without one it signs ad hoc, as
before; if the keychain is locked, or the session can't reach it, it says so and signs
ad hoc, so a build never fails for it), and `shipyard doctor`'s `entitlement` line says
which. Local builds aren't signed with the hardened runtime, so a debugger attaches
(`notarize.sh` adds it for a release).
`scripts/macos/test-sign.sh` (in `verify.sh --mac`) checks both ways in a throwaway
keychain: two builds signed with an identity keep one designated requirement (what the
permissions are kept by), two ad-hoc ones don't. Builds target macOS 13
(`MACOSX_DEPLOYMENT_TARGET` in `.cargo/config.toml`).

**A release's signing** (stage 15): `scripts/macos/notarize.sh FILE…` signs with a
Developer ID (`SHIPYARD_DEVELOPER_ID`) and the hardened runtime and a secure timestamp,
and has Apple notarize the files (`SHIPYARD_NOTARY_PROFILE`, a `notarytool` profile).
It isn't run by any check here.

Then:

```sh
# (A shipyard built in this tree finds target/guest by itself, unless the VM's
# directory has a guest of its own: SHIPYARD_GUEST_DIR says it outright.)
target/debug/shipyard vm start    # boots the guest (about a second)
target/debug/shipyard vm status
target/debug/shipyard run --rm alpine echo hi   # any command: run in the VM
target/debug/shipyard vm stop     # its containers stopped, the guest powered off
```

The VM keeps its files in `~/Library/Application Support/Shipyard/vm`
(`SHIPYARD_VM_DIR`): its data disk (a sparse 64 GiB file), `console.log` (the
guest's console: why a boot failed), `host.log`, its lock and its socket.

`cargo run` and `cargo test` run commands on the Mac itself, not in the VM:
`.cargo/config.toml` sets `SHIPYARD_VM=off` for them (a value in your shell wins), so
the macOS tests keep checking the native path. Run the built binary
(`target/debug/shipyard`) to use the VM.

The VM's end-to-end tests run only where asked, with a guest built (each boots a VM;
four run at once):

```sh
SHIPYARD_TEST_VM=required cargo test -p shipyard-cli --test vm --test vm_terminal
.claude/skills/shipyard-verify/scripts/verify.sh --vm   # the guest built first, then both
```

## The desktop app (stage 14)

`desktop/` is the Tauri 2 app: `desktop/src-tauri/` its Rust shell (a Cargo workspace
of its own, so the main one never needs WebKitGTK), `desktop/ui/` its page (Svelte 5,
TypeScript, Vite). Its logic is the main workspace's `crates/shipyard-client`, which the
Linux tests reach (`the_desktop_client_reads_the_api`) and the VM tests too
(`the_desktop_client_reaches_the_vm`).

It needs Node.js 26 (`desktop/ui/.nvmrc`; `npm` refuses another, `engine-strict`, and
runs no package's install scripts, `.npmrc`).

```sh
cd desktop/ui
npm ci                                    # exact versions, no install scripts
SHIPYARD_BIN="$PWD/../../target/debug/shipyard" npm run tauri dev   # the app, its page live
npm run check                             # svelte-check, warnings fail it
npm test                                  # vitest (the store, the Manifest)
npx playwright install webkit             # once
npm run visual                            # WebKit: the page against the design's mock,
                                          # pixel for pixel (light, dark), and the built
                                          # page under the release CSP
npm run design-css                        # src/lib/design.css again from the mock
```

`npm run visual`'s pictures (the mock's, the app's, their difference) are in
`desktop/ui/test-results/`. The page is the mock's markup and its stylesheet whole
(`design.css` is extracted from `docs/design/shipyard-ui.html`, never edited); the
app's own differences are in `src/app.css`. Two things the comparison taught: the mock
file has no doctype, and quirks mode gives its tables another line height, so the test
renders it in standards mode, as its published artifact is; and no markup carries a
`style` attribute (the release CSP refuses one: a value set at run time is a Svelte
`style:` directive). `verify.sh --mac` runs all of it, and the shell's fmt, clippy,
tests and `cargo deny` (`desktop/deny.toml`), and checks that both lock files build the
client crate from the same versions (`.claude/skills/shipyard-verify/scripts/client-versions.py`:
after a `cargo update` in either, pin the other).

`shipyard` must be signed to start the VM (`scripts/macos/sign.sh target/debug/shipyard`
after each build); the app says so if it isn't.

**On Linux** (step 14.7): `scripts/desktop-linux.sh` builds `shipyard` and the app in the
desktop image (`scripts/desktop-image/Dockerfile`: Debian trixie, WebKitGTK 4.1, Xvfb,
`tauri-driver`; built on first use, tagged by its Dockerfile's hash), then opens the real
app as a rootless user with nothing serving its API, under a virtual display, and drives it
with Tauri's WebDriver (`desktop/ui/tests-linux/drive.mjs`, plain W3C WebDriver): the app
must start the service, draw the user's containers, and open every section. Its
screenshots are in `target/desktop-linux/`; `--shell` opens the image instead, and
`--build-only` leaves the two binaries in `target/desktop-linux/bin/`. Its volumes are
`shipyard-desktop-target` and `shipyard-desktop-registry` (rebuildable). `verify.sh` runs it
after the Linux passes.

To try it by hand, `scripts/macos/desktop-vm.sh up` makes a Lima VM (Debian 13) with a GNOME desktop in
a window (`scripts/macos/desktop-vm.yaml`; once, a few GB), copies the two binaries in, and
opens it; in its terminal, `shipyard-desktop`. `desktop-vm.sh install` copies a new build
in, `down` stops it. Its desktop logs Lima's user in with no password, and that user has
passwordless `sudo`: anyone at the VM's window is root in it (nothing of the Mac is
mounted there).

The Mac's `.app` (`npm run tauri -- build --bundles app`, `scripts/macos/sign.sh
--bundle`) opened from Finder or `open` gets launchd's environment: no `SHIPYARD_BIN`, a
short `PATH`, so it finds `target/debug/shipyard` only once it's chosen in Settings. Its
folder prompts (the VM's shares) are asked of "Shipyard": with the local identity, once.

## Running Linux-only stages on macOS: Lima VM

Eventually Shipyard manages its own VM (a minimal path in roadmap stage 10, production-grade in stage 13). Until then,
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

Fedora is the host with firewalld (another program's firewall, which Shipyard warns
about and never changes; see [Other firewalls](user/networking.md#other-firewalls)).
The same build, checked with firewalld running:

```sh
limactl start --name=shipyard-fedora --cpus=2 --memory=3 template:fedora
limactl copy target/aarch64-unknown-linux-musl/release/shipyard shipyard-fedora:/tmp/
limactl shell shipyard-fedora -- sudo install -m 0755 /tmp/shipyard /usr/local/bin/
limactl shell shipyard-fedora -- sudo dnf install -y firewalld passt
limactl shell shipyard-fedora -- sudo systemctl enable --now firewalld
limactl shell shipyard-fedora -- sudo shipyard doctor     # its firewall line warns
limactl shell shipyard-fedora -- sudo shipyard run alpine wget -qO- example.com
limactl delete --force shipyard-fedora                  # when done
```

Without the fix the warning names, the `wget` gets no answer. With it, the
`wget` works and the warning stops.

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

# Contributing to Shipyard

Thanks for wanting to help. Shipyard is a daemonless container engine in Rust for macOS
developers and Linux hosts: Docker-compatible (its API, its CLI, `docker compose`), with a
desktop app that maps which containers talk to which. It has two goals of equal weight:
**production-grade software**, and **learning systems programming** from first principles,
with *The Linux Programming Interface* (TLPI) as the reference. So a good change does what
it should, is tested the way the project tests, and explains the systems idea behind it.

This guide covers everything from a first bug report to a merged pull request. Everyone
taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

## Contents

1. [Ways to help](#ways-to-help)
2. [Before you start](#before-you-start)
3. [Setting up](#setting-up)
4. [Finding your way around](#finding-your-way-around)
5. [Making a change](#making-a-change)
6. [Testing](#testing)
7. [Documentation](#documentation)
8. [Commits](#commits)
9. [Pull requests](#pull-requests)
10. [Using AI tools](#using-ai-tools)
11. [Security issues](#security-issues)
12. [License](#license)
13. [Getting help](#getting-help)

## Ways to help

You don't have to write Rust to help.

- **Report a bug.** Use the [bug report form](https://github.com/Zena4L/shipyard/issues/new/choose).
  The most useful report says what you ran, exactly as you typed it, what you expected,
  what happened, and includes `shipyard version` and `shipyard doctor`'s output, your OS
  (the Linux distribution and kernel, or the macOS version), and whether you ran as
  yourself (rootless), as root, or on a Mac.
- **Try it where we haven't.** Shipyard is tested on Debian and Ubuntu, Fedora with
  SELinux, Ubuntu with AppArmor's restrictions, and Apple Silicon Macs. A run on another
  distribution, kernel or architecture, with what broke, is valuable.
- **Run your own workloads.** Point `docker` (`alias docker=shipyard`), `docker compose`
  or Testcontainers at Shipyard and report what doesn't behave as Docker does.
- **Improve the docs.** [`docs/user/`](docs/user/) is what users read. A wrong, missing
  or unclear sentence is a bug.
- **Fix an open finding.** The [roadmap's open findings](docs/ROADMAP.md#open-findings)
  list known problems with their severity, the fix, and the stage meant to fix them.
- **Request a feature,** with the [feature form](https://github.com/Zena4L/shipyard/issues/new/choose).
  Describe the task you're trying to do (a Docker command or a compose file you'd like to
  work is a great start) rather than the solution.

## Before you start

**Open or comment on an issue first** for anything bigger than a typo or an obvious
one-line fix. Say what you'd like to do and how. It saves you from building something
that can't be merged.

Some ground rules shape what can be merged:

- **The roadmap decides the order.** Work is staged in [`docs/ROADMAP.md`](docs/ROADMAP.md),
  each stage building on the last. The current stage is **15, hardening and v1.0**: fixes,
  tests, proof, packaging and docs are the most welcome changes now. Something that belongs
  to a later stage is recorded, not built early.
- **Docker-compatible, not Docker-reimplemented.** Shipyard serves a subset of Docker's
  Engine API, takes Docker's everyday commands and flags, runs `docker compose` itself
  against that API, and builds images with BuildKit. A change that departs from Docker's
  behaviour needs a reason.
- **Daemonless.** Nothing of Shipyard runs while it isn't being used: one small monitor
  process per container, and anything else (the API's service, the Mac's VM) starts on
  first use and stops when idle. No change may add a long-running daemon.
- **No new hand-written protocol or format** unless the static binary depends on it; use a
  maintained crate or tool instead.
- **Out of scope:** multi-host orchestration, Windows containers, a builder or compose
  engine of Shipyard's own, Intel Macs, checkpoint/restore and a Kubernetes CRI
  ([FEATURES.md](docs/FEATURES.md#out-of-scope-on-purpose)).

## Setting up

[`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) has every command in detail. In short:

**Everyone**

- **Rust**, the version pinned in [`rust-toolchain.toml`](rust-toolchain.toml).
  [rustup](https://rustup.rs) installs it on your first build.
- **Git**, and `cargo-deny` for the licence and advisory check (`cargo install cargo-deny`).

**On Linux** you can build and run everything natively. Kernel features need a recent
kernel (5.11 or later) with cgroup v2. Rootless runs need `uidmap` (`newuidmap`) and
`passt` (`pasta`), and subordinate IDs for your user in `/etc/subuid` and `/etc/subgid`.

**On a Mac** (Apple Silicon, macOS 13 or later), the workspace builds and its macOS tests
run natively, but the Linux-only code (namespaces, mounts, cgroups) only runs on Linux.

- **Docker or OrbStack** for the Linux test container that `scripts/test-linux.sh` uses.
- **Lima** (optional) for real distributions.
- Shipyard's own VM on the Mac needs `shipyard` signed after each build:
  `scripts/macos/sign.sh target/debug/shipyard`. `scripts/macos/make-signing-identity.sh`
  makes a stable local identity once, so macOS stops asking about each new build.

**For the desktop app** (`desktop/`): Node.js 26 (`desktop/ui/.nvmrc`), then
`cd desktop/ui && npm ci`. On Linux it also needs WebKitGTK 4.1.

A first build and test:

```sh
git clone https://github.com/Zena4L/shipyard.git && cd shipyard
cargo build
cargo test --workspace --no-fail-fast
cargo run -- doctor          # what your host supports, and how to fix what it doesn't
```

## Finding your way around

[`AGENTS.md`](AGENTS.md) is the project's operating manual. Despite its name it's written
for human and AI contributors alike: section 3 maps every crate and directory, section 4
explains the architecture (the process tree, state, the lock order), section 5 the code
rules, section 10 the lessons that already cost real time. Read the parts that touch your
change.

The crates, from the bottom up:

| Crate | What it does |
|---|---|
| `shipyard-sys` | Shared plumbing: atomic file writes, bounded reads, locks, pidfds, descriptor passing |
| `shipyard-runtime` | Processes, namespaces, mounts, cgroups, terminals; the OCI runtime |
| `shipyard-image` | Registries, the content-addressed image store, safe layer unpacking, archives |
| `shipyard-network` | Netlink, nf_tables, networks and leases, pasta, the DNS server |
| `shipyard-storage` | Named volumes |
| `shipyard-vm` | The macOS VM (Virtualization.framework) and its host process |
| `shipyard-engine` | What every command does, behind typed APIs (it never prints) |
| `shipyard-api` | Docker's Engine API, a subset, served by `shipyard system service` |
| `shipyard-cli` | The `shipyard` and `shipyard-runtime` binaries: flags in, output out |
| `shipyard-client` | The desktop app's logic, as a client of the API |

Elsewhere: `desktop/` (the Tauri app), `docs/user/` (user docs), `docs/design/` (the UI's
design and its mock), `docs/stages/` (the engineering record of each stage), `fuzz/` (fuzz
targets), `scripts/` (test, guest and release scripts), `contrib/` (systemd units, the
AppArmor profile).

## Making a change

1. **Fork the repository and branch from `main`** (`fix/what-it-fixes`,
   `feat/what-it-adds`, `docs/…`). Keep a pull request to one change: two small PRs are
   reviewed faster than one large one.
2. **Read before you write.** Read the code you'll touch, its tests, and the stage notes
   for the area (`docs/stages/`): their failure-mode register lists the bugs that area
   has already had.
3. **For a design change**, write a short design in the issue first: what changes, why,
   the alternatives, and, for anything a container, an image, a registry or another local
   user could abuse, who is hostile and what they must never be able to do.
4. **Write the tests with the code,** not after (see [Testing](#testing)).
5. **Run the checks** before you push (below).

### Code rules

The full list is in [AGENTS.md section 5](AGENTS.md#5-code-rules). The ones reviews catch
most often:

- **Clippy's `pedantic` lints are on, with `-D warnings`.** Fix a lint rather than
  allowing it; a needed `#[allow]` carries a `reason`. Functions stay under 100 lines and
  7 arguments: split them.
- **Platform gating:** `#[cfg(unix)]` for code that must work on macOS and Linux;
  `#[cfg(target_os = "linux")]` for namespaces, mounts and cgroups, returning a
  `LinuxOnly` error elsewhere.
- **Errors:** `thiserror` enums in libraries, `anyhow` in the CLI. No `unwrap()` outside
  tests. Each new failure gets an error kind, and tests match the kind, never the
  message's words.
- **System calls:** `std` first, then `nix` or `rustix`, raw `libc` only when neither has
  the call. Every `unsafe` block has a `// SAFETY:` comment that argues why it's sound.
- **Code that runs between `fork` and `exec`** (`process/child.rs`, every `pre_exec`) must
  not allocate, panic or take a lock, and calls only async-signal-safe functions.
- **New engine logic goes in a library crate behind a typed API,** never in the CLI.
- **Every on-disk format has a version from its first write,** a test that reads the
  previous version, and a refusal of a newer one.
- **Text from outside** (container names, logs, labels, a registry's messages) goes
  through `printable()` before it reaches a terminal; the CLI writes with `writeln!` to a
  locked stdout, never `println!`.
- **Dependencies** are declared once in the workspace's `Cargo.toml`, and `cargo deny
  check` must pass (licences, bans, advisories).
- **Every public item has a doc comment.** Comments are short and plain, and say why.

### The desktop app

The UI's source of truth is the design mock, [`docs/design/shipyard-ui.html`](docs/design/shipyard-ui.html)
(with its notes in [`docs/design/README.md`](docs/design/README.md)). A visual change goes
into the mock first, then into the app: `npm run design-css` extracts the mock's
stylesheet into `desktop/ui/src/lib/design.css` (never edit that file by hand), and the
app's own differences go in `src/app.css`. The app runs under a strict content security
policy: no inline `style` attributes in markup (use Svelte's `style:` directives), no
`{@html}`, and every string from a container rendered as text.

## Testing

A change isn't done until it's tested where it runs.

**Every change, on any machine:**

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
cargo deny check
```

**Linux behaviour runs on Linux.** On a Mac, or to test as both a normal user and root,
use the test container:

```sh
scripts/test-linux.sh                 # clippy, then every test as a normal user, then as root
scripts/test-linux.sh --shell         # a shell in the same container, to debug
scripts/test-linux.sh --root-shell    # the same, as root
```

**All of it in one command:** `.claude/skills/shipyard-verify/scripts/verify.sh --mac`
(minutes: the macOS side and the desktop app) or `verify.sh` alone (about an hour: both
Linux passes and, on a Mac, the VM's tests). Run the long one in the background.

**The desktop app:**

```sh
cd desktop/ui
npm run check      # svelte-check
npm test           # vitest
npm run visual     # WebKit: the app against the design's mock, pixel for pixel, light and dark
```

What makes a test count here:

- **Unit tests** for logic, **end-to-end tests** (through the real `shipyard` binary) for
  what a user sees. A change to rootless behaviour is tested rootless; one that needs root
  is marked `#[ignore = "needs root"]` and runs in the root pass.
- **A negative check needs a positive control** in the same test: "can't reach X" passes
  by accident if X is unreachable anyway.
- **No sleeps to wait for something.** Wait on the condition (`eventually(…)` in
  `shipyard-test-support`), a lock or a pidfd.
- **A fix for a race comes with a test that loses the race without the fix.**
- **A parser of untrusted input gets a fuzz target** in `fuzz/` (see the existing ones).
- **Shared helpers live in `shipyard-test-support`;** extend it instead of copying one.

For a critical line, the maintainers also run mutation checks (put the bug back, see the
test fail). You don't have to, but a test that would pass with the bug back will be
pointed out in review.

## Documentation

Docs are part of done:

- What users see (commands, flags, behaviour, messages, exit codes) is described in
  [`docs/user/`](docs/user/), only as it works in the current version.
- After changing a command or a flag, regenerate the reference:
  `UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference` (a test fails while it's
  stale; never edit `docs/user/cli.md` by hand).
- Add a line to [`CHANGELOG.md`](CHANGELOG.md) under the unreleased version: Added,
  Changed, Fixed, Security, or Known limitations.
- When a message's wording changes, search the code, the tests and the docs for the old
  wording.
- Explain the systems concept behind a change (the system call, what the kernel does,
  the TLPI chapter) in the pull request, so the next reader learns it too.

## Commits

Commits follow [Conventional Commits](https://www.conventionalcommits.org/):

```
fix(network): keep a bridge's MAC address when a port leaves

The kernel gives a bridge without a set address its lowest port's, so the
address changed when that container stopped and every neighbour's ARP entry
went stale. Set it once, at creation.
```

- The type: `feat`, `fix`, `docs`, `test`, `refactor`, `perf`, `chore`, `ci`.
- An optional scope (the crate or area), then a summary under about 72 characters, in
  the imperative.
- A body that says what and why when the summary isn't enough.
- Commits carry their human author. Leave out co-author trailers or "generated with"
  lines for AI tools (see [Using AI tools](#using-ai-tools)).

Pull requests are squash-merged, so tidy history inside a branch is welcome but not
required.

## Pull requests

- Fill in the template: what changes, why, and the checks you ran.
- **CI runs on every pull request** (fmt, clippy and tests on Linux and macOS, the root
  tests, rootless, AppArmor and Fedora jobs, the conformance suite, musl, rustdoc,
  `cargo deny`). It must be green. CI can't run Shipyard's macOS VM or the desktop app's
  pixel tests, so say in the PR if you ran `verify.sh --mac` or the VM tests yourself.
- Keep your branch up to date with `main` by rebasing.
- **Review** checks the change against [AGENTS.md](AGENTS.md)'s rules, and asks what could
  go wrong: a process killed halfway, a reboot, two commands at once, an older version's
  files, a hostile container. Expect questions; they're how the project stays solid.
- A maintainer merges once CI is green and the review is resolved, squashing the PR into
  one commit whose message follows the convention above.

## Using AI tools

AI assistants are welcome. [`AGENTS.md`](AGENTS.md) and the procedures in
[`.claude/skills/`](.claude/skills/) are written so a coding agent works by the same rules
as a person. Whatever tool you use:

- **You are the author and you are responsible** for every line: understand it, test it,
  and be ready to explain it in review.
- Don't submit code you haven't read, or tests you haven't seen fail without the fix.
- Commits carry your name, with no AI co-author trailers or "generated with" lines.

## Security issues

**Never report a vulnerability in a public issue.** Report it privately through the
repository's **Security** tab ("Report a vulnerability"), as [SECURITY.md](SECURITY.md)
describes. Say what an attacker can do, from where (a container, an image, a registry,
another local user, the network), and how to reproduce it.

## License

Shipyard is licensed under the [Apache License, Version 2.0](LICENSE). By submitting a
contribution, you agree that it's licensed under the same terms (the licence's section 5),
and you confirm you have the right to submit it. Code copied from elsewhere must be under
a licence compatible with Apache-2.0, and its origin must be said in the pull request.

## Getting help

- A question about using Shipyard, or about a change you're planning: open an issue.
- Something in this guide unclear or wrong: that's a bug too. Open an issue or a pull
  request.

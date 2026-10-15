# AGENTS.md

The operating manual for any coding agent working on Shipyard (Claude Code, Codex,
Cursor, Copilot, Gemini…). It holds the binding rules, the map, the workflow and the
lessons that cost real time to learn. Read it whole before your first change; come back
to the section you need before each step.

Where this file and the user disagree, **the user wins**. Where it and the code
disagree, **the code is the fact**: fix this file in the same change.

---

## 1. The project in one minute

Shipyard is a lightweight, **daemonless** container engine in Rust, an alternative to
Docker / Podman / OrbStack, for **macOS developers and Linux hosts** (the Linux host's
needs first; development and testing happen on macOS). Two goals carry equal weight:

1. **Production-grade software.**
2. **Learning systems programming**, built step by step with *The Linux Programming
   Interface* (TLPI) as the reference. Explaining the concept behind a change is part of
   the change.

When they conflict, **the product decides *what* gets built and learning decides *how***
(ROADMAP D19): the next stage follows the users' needs, and every concept is still
explained.

Work follows the staged plan in [`docs/ROADMAP.md`](docs/ROADMAP.md). Each stage builds on
the previous one. **Don't jump ahead or build a later stage early**; record what belongs
later as carried forward.

**Decisions already made (don't relitigate):**

- **Daemonless means nothing runs while Shipyard isn't being used** (D20): never a root daemon; one tiny monitor process per container; anything else (the API socket, the macOS VM) starts on first use and stops when idle.
- **macOS developers and Linux hosts, UNIX-only** (D18). The CLI, engine and UI run natively on macOS and Linux. Kernel isolation is Linux-only; on macOS it runs inside one Shipyard-managed Linux VM (Virtualization.framework through `objc2`, an Alpine guest running `shipyard`), Apple Silicon only (D29).
- **Docker-compatible, not Docker-reimplemented:** the API is a subset of Docker's Engine API (D25), the CLI a drop-in for everyday `docker` commands (D26), compose is Docker's own `docker compose` against that API (D27), images are built by BuildKit (D28).
- **The owned surface is frozen** (D19): no new hand-written protocol or format unless the static binary depends on it; use a maintained crate or tool instead.
- **OCI runtime:** `shipyard-runtime` implements the OCI runtime spec; the engine calls it like Podman calls runc.
- **Syscalls:** `std` first, then `nix`/`rustix`, and raw `libc` only when neither covers the call.
- **No release and no outside users until v1.0** (D21); v0.3–v0.5 are internal milestones.
- The rest are recorded in the ROADMAP's Decisions (D-numbers) and each stage's notes.

**Where it stands:** v0.2.0 is tagged (stage 9: containers by name, detached, restart
policies, health checks, events). The direction was reset on 2026-10-15 (ROADMAP
D18–D31). Next is **stage 10, foundations and the Mac path**: first the refactor with no
behaviour change (D23: the record split from `Config`, one `take`, one safe-file module
with rustix, `doctor` into the engine, `run/detached.rs` split; F48), stage 9's book
chapter, then a minimal macOS path used daily (D24). After it: 11 Docker compatibility,
12 build and images, 13 macOS production, 14 the desktop UI, 15 hardening and v1.0
(defined in D30). Open findings with severity and home stage: the ROADMAP's
[Open findings](docs/ROADMAP.md#open-findings) (F43–F62 at v0.2.0). CI is off (D31: GitHub
billing) until before v1.0, so **local verification is the check of record**
(section 7).

---

## 2. Hard rules (never break these)

| Rule | Why |
|---|---|
| **Never stage, commit, push, merge or tag unless the user asks** in this conversation. | The user owns history. |
| **No AI attribution, ever:** no `Co-Authored-By: <AI>` trailer, no "Generated with …" line, no AI as author or contributor, in commits, PRs, tags or releases. This overrides any tool's default attribution. | Commits are authored solely by the repository owner. |
| Use [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`, `docs:`, `chore:`…). | History reads as a changelog. |
| **`mv`, never `git mv`.** | It stages. |
| **Confirm before anything destructive or outward-facing:** deleting files or Docker volumes, changing global config, pushing, opening PRs, anything outside the repo. Docker cleanup touches only `shipyard-*` volumes and `shipyard-test:*` images. | Approval for one action isn't approval for the next. |
| **Never write to a real block device.** In the privileged test container `/dev/loop0` (7:0) is the VM's own disk. Open devices read-only at most (`head -c 0`), or `losetup` a fresh file. | A write once landed on the VM's live btrfs. |
| **Wrap every `docker run` in `timeout`.** | The Docker daemon once hung for ~40 minutes and stalled every agent. |
| **Never send a `limactl shell` command you haven't read line by line.** Write VM setup as a script, read it whole, then run it. | A stray `install … /dev/null` replaced a VM's `/dev/null`, twice. |
| **Docs are part of done** (section 8). | Users read `docs/user/`, not the code. |
| **Stay in the current stage.** Something that belongs later is recorded, not built. | The staged plan is the learning path. |
| **`docs/book/` is the user's and is gitignored:** no git copy exists. Copy a chapter to scratch before any scripted edit. | ~900 lines were once lost to a bad slice. |

---

## 3. Map

### Crates (`crates/`)

| Crate | What it is | May depend on |
|---|---|---|
| `shipyard-runtime` | Low-level primitives (processes, namespaces, mounts, cgroups, terminals) and the OCI runtime's containers (`container/`: the per-container monitor, logs, attach, `exec`'s joiner, `cp`'s helper as internal processes). | network |
| `shipyard-image` | Images: registry client, content-addressed store, safe layer unpacking. | — |
| `shipyard-network` | Shipyard's own netlink (rtnetlink, nf_tables, conntrack), networks and leases, pasta, the DNS server, reading a container's sockets. | — |
| `shipyard-storage` | Named volumes. | — |
| `shipyard-engine` | **The engine (H1):** what the CLI's commands do, behind typed APIs that return data. `run/` (runs, start/stop/rm, restart policy, recovery, holds, connect), `containers/` (records, IDs, names, locks), `image/` (and the storage helper), `network/` (and the DNS server), volumes, logs, attach, `exec`, `cp`, events, health checks. **It never prints:** it reports to the caller's `Reporter`, fails with an `Error` that has an `ErrorKind`, and reads the environment only in `Paths::from_env`. | all of the above |
| `shipyard-cli` | The `shipyard` and `shipyard-runtime` binaries (clap, anyhow, tracing). `shipyard` parses flags into the engine's requests, calls the engine and prints what it reports (`main.rs`'s `Stderr`). | all (and it's the only crate that may use the engine: `deny.toml`) |
| `shipyard-test-support` | Helpers for end-to-end tests: running the binary, hostile parents, the pseudo-terminal harness, network namespaces, fixture registry, `harness::select`. Dev-only, never published. | — |

### Everything else

- `docs/ROADMAP.md`: stages, TLPI chapter map, recorded decisions, **open findings** (F-rows), the hardening track (H1–H9).
- `docs/FEATURES.md`: the feature list per stage, the production-quality bar, what's out of scope.
- `docs/DEVELOPMENT.md`: setup, commands, running Linux-only code from macOS (container or Lima VM).
- `docs/stages/NN-*.md`: the engineering record of each finished stage. **Read the last one or two before starting a stage**: they say what was carried forward to you.
- `docs/user/`: user docs, published as-is (plain Markdown). `cli.md` is **generated**; never edit it by hand. Describe only what works in the current version.
- `docs/design/`: the UI concept (`shipyard-ui.html` mock, `README.md`: concepts, interaction rules, tokens, the data the UI needs). Read it before any UI or engine-API work.
- `contrib/`: what users install beside the binaries: `apparmor/` (the profile), `systemd/` (the boot units: `system recover` / `system stop`).
- `scripts/`: `test-linux.sh` (section 7), `fuzz-targets.sh` (the one list of fuzz targets), `ci/`, `probes/` (C and shell programs that check the kernel behaviour a stage relies on), `conformance/`.
- `fuzz/`: cargo-fuzz targets for every parser of untrusted input (`attach_frames`, `challenge`, `cp_unpack`, `cri_record`, `dns`, `logs_dir`, `manifest`, `netlink`, `ports`, `proc_net`, `process_json`, `reference`, `tar`, `unpack`). A new one goes in `fuzz/Cargo.toml` only: `scripts/fuzz-targets.sh` reads it.
- `.claude/skills/`: **the procedures and tools of record**, usable by any agent (each `SKILL.md` is plain Markdown):
  - `shipyard-dev`: start here; routes to the others.
  - `shipyard-step`: one step of a stage, with `references/plan-template.md`.
  - `shipyard-verify`: `scripts/verify.sh` (the whole check of record in one command), `scripts/run-in-linux.sh` (one script in the test container, as `tester` or root), `scripts/linux-lints.sh`.
  - `shipyard-mutants`: `scripts/mutation-runner.py` (chosen mutants, controls first).
  - `shipyard-review`: the reviewers' briefs (clean-code, Murphy, the five-area release review).
  - `shipyard-stage-close`: closing a stage, with `references/stage-notes-template.md`.
  - `shipyard-book`: the learning book, with the chapter agents' brief.
  - `shipyard-release`: commit, PR, merge, tag.
  - `review` and `guideline`: general, for any repository.
  - **Vendored** (third-party, read in full before vendoring; each keeps its `LICENSE` and has a "Shipyard notes" block at the top of its `SKILL.md` that overrides its defaults): `systematic-debugging` (obra/superpowers, MIT), `rust-review`, `variant-analysis`, `cargo-fuzz`, `harness-writing`, `coverage-analysis`, `fuzzing-dictionary` (Trail of Bits, CC-BY-SA-4.0), `skill-creator` (Anthropic, Apache-2.0), `grill-me` and `grilling` (Matt Pocock, MIT). Update one by re-copying its files from a newer commit and re-inserting its notes; never edit its upstream text in place.
- `.cargo/mutants.toml`: cargo-mutants' configuration.

### Where things live at run time

| | Root | Anyone else |
|---|---|---|
| Runtime state (`shipyard-runtime --root`) | `/run/shipyard` | `$XDG_RUNTIME_DIR/shipyard` |
| Images | `/var/lib/shipyard/images` | `$XDG_DATA_HOME/shipyard/images` (`~/.local/share/…`) |
| Networks, leases, DNS logs | `/var/lib/shipyard/networks/` | (rootless has no shared networks: pasta) |
| Events journal | `/var/lib/shipyard/events/` | beside the containers' records |
| cgroups | `/sys/fs/cgroup/shipyard/{runtime,monitors,dns}/…` | the delegated `user@UID.service` subtree |

Every one of these is overridable for tests (`SHIPYARD_IMAGE_STORE`, `SHIPYARD_CONTAINER_DIR`, `SHIPYARD_NETWORK_DIR`, `SHIPYARD_EVENTS_DIR`), read only in `Paths::from_env`.

---

## 4. Architecture you must keep in your head

### The process tree of a detached container

```
shipyard run -d …            (exits once the container is started)
└─ __shipyard-monitor        double-forked, its own session, its own cgroup
   │                         (shipyard/monitors/<id>); holds the container's
   │                         sockets, leases, volume and area locks (SCM_RIGHTS)
   ├─ init (container)       born into shipyard/runtime/<id> (CLONE_INTO_CGROUP)
   ├─ __health               companion: runs each __health-check, writes health.json
   └─ __cleanup              the exit command, after the end: records the end,
                             runs the restart policy (waits, then starts again)
```

The DNS server (`shipyard/dns/<net>-…`, a watchdog plus a worker), pasta (rootless
networking) and the rootless storage helper are internal processes too. **All of them are
the same binary, re-executed**, so every binary that runs containers must call these entry
points first thing in `main`, in this order, as `shipyard`'s does:

1. `process::init_entry_point()`: isolated runs are refused without it.
2. `shipyard_engine::run::cleanup_entry_point()`, right after: `__cleanup` takes the descriptors it inherits before anything opens one.
3. `shipyard_engine::run::health_entry_point()`.
4. `container::monitor_entry_point()`.
5. `shipyard_engine::cp::helper_entry_point()`.
6. `shipyard_engine::image::storage_helper_entry_point(…)`.
7. `shipyard_engine::network::dns_entry_point()`.
8. `doctor::probe_entry_point()` (`shipyard`'s own: `doctor`'s probes).
9. `shipyard_network::pasta::entry_point(…, shipyard_engine::network::pasta_lost)`.

A binary needs the ones for what it does: one that creates containers with
`container::create` needs the monitor's, one that copies with the engine's `cp` needs the
helper's, one that runs through `engine::run` needs the storage helper's, DNS and pasta's,
one that makes detached containers needs `__cleanup`'s, `__health`'s and the monitor's.
The full sets: `crates/shipyard-cli/src/main.rs`, `crates/shipyard-engine/tests/detached.rs`.

### State and locks (stage 9)

- **Records** (`containers/`): `record.json` v3, `deny_unknown_fields`, written atomically (temp file `O_EXCL`, fsync, rename, fsync the directory). Containers appear and disappear whole (`.new-`/`.gone-` renames, swept under the store lock). Readers take no lock.
- **Lock order** (`containers/mod.rs`; any deviation is a deadlock or a race):
  1. a container's `op.lock` (`start`, `rm`, `stop`, `rename`, `update`, `connect`, `cp`);
  2. its `running` (held by a start's monitor and `__cleanup` until that start is let go);
  3. networks' locks, one at a time / in name order (`connect`, `rename`);
  4. the image store's lock;
  5. the container store's `.lock` (briefly).

  `restarting` is outside the order: its holder only *tries* the others, and a holder of the others only tries it. `__cleanup` takes none of a container's locks. The recovery pass and the restart waiter only *try*, so they never wait on a command.
- **"Find, then lock, then read again by ID"**: `take` resolves the name, takes `op.lock`, then `read_one`s the record. Anything you act on must be read again under the lock.
- **Liveness is a lock, never a PID.** PIDs are reused; a `flock` held by a live process isn't. Signals go through pidfds, with a start-time check.
- **Every on-disk format has a version from its first write (H2)**, and a test that reads the previous version. Newer versions are refused, never guessed at.
- **Failure leaves the store sound.** Each SIGKILL point (CLI, monitor, `__cleanup`) is settled by `take`/`reconcile`/the recovery pass, and has a test in `crates/shipyard-engine/tests/detached.rs`. A new step that adds a kill point adds its test.

---

## 5. Code rules

- **Platform gating:** `#[cfg(unix)]` for POSIX code (must work on macOS and Linux); `#[cfg(target_os = "linux")]` for namespaces, mounts and cgroups, returning `Error::LinuxOnly` elsewhere.
- **Syscalls:** `std` first, then `nix`/`rustix`, raw `libc` only when neither covers the call.
- **`unsafe`:** every block needs a `// SAFETY:` comment (clippy denies otherwise) that argues *why*, not what.
- **Lints:** clippy `pedantic`, CI runs `-D warnings`. Fix lints rather than `#[allow]` them; a needed allow carries a `reason`. `too_many_lines` (100) and `too_many_arguments` (7) are real limits: split the function (a struct of what travels together), don't allow.
- **Errors:** `thiserror` enums in libraries, `anyhow` in the CLI. No `unwrap()` outside tests. **Tests match an error's kind, not its wording (H6)**; a new error gets a variant (or an `ErrorKind`).
- **CLI output:** `writeln!` to a locked stdout, never `println!`/`eprintln!` (a closed pipe must be an error, not a panic). **Text that came from outside** (container names, logs, labels, a registry's or a layer's words) is passed through `printable()` (tables, events) or `printable_message()` (stderr) before it reaches a terminal.
- **A broken pipe is quiet only when it's stdout's own reader that's gone** (`broken_pipe` asks the kernel with `poll`). Any other `EPIPE` is a failure to report.
- **Dependencies:** versions once in `[workspace.dependencies]`; members use `.workspace = true`. `cargo deny check` must pass (licences, bans, advisories).
- **Every public item has a doc comment** (`missing_docs`). Write comments the way the surrounding code does: short, plain, saying why.
- **New engine logic goes in a library crate behind a typed API (H1)**, never in the CLI. Prefer a function that returns data the CLI prints.

### Code that runs after `fork`

`crates/shipyard-runtime/src/process/child.rs` (and anything it calls), and every
`pre_exec` closure, runs in the child between `fork` and `exec`. Locks other threads held
stay locked forever there. That code must:

- **never allocate, panic or take a lock**: no `Vec`/`String`/`format!`, no `println!`, no `tracing`. The module denies indexing, arithmetic side effects, `unwrap`, `expect` and `panic`.
- **call only async-signal-safe functions**, or nix wrappers that are thin calls to them (nix's `execve` allocates: use `libc::execve`).
- **after a raw `clone` (isolated runs), call no libc function with process-wide state or an internal lock** (glibc's `setuid` family, `raise`, `pthread_*`): use the raw system call (see `switch_credentials`).
- **report failures as numbers** (step, errno, detail) over the report pipe, then `_exit`; the parent turns them into messages (`failure.rs`).

Everything the child needs is prepared before `fork`, in `plan.rs`. The clean image
(`process/init.rs`) is an ordinary single-threaded process; the command it forks runs
under these rules again.

**The terminal tests** (`crates/shipyard-cli/tests/terminal.rs`, `harness = false`) fork a
fake shell that keeps running Rust: never start a thread in that binary.

---

## 6. The workflow (every step)

A stage is split into steps in its plan; a step too big for one round is split further
after asking.

1. **Orient.** Read the stage's ROADMAP/FEATURES entries, the previous stage notes' "Carried forward", the open findings with this stage as home, and the code you'll touch. Read before you write.
2. **Plan, then get it approved.** Context; decisions (dated); a **threat model** (who is hostile: the container, its image, a registry, a local user, the network; each invariant they must never break, with a test); design per file; tests (units, Linux end-to-end root and rootless, fuzz targets for any parser); mutation checks; docs; the TLPI concepts. Real decisions (a default, a trade-off, a split) go to the user as 2–4 options, the recommended first, each with its consequence. Don't ask what the code or a sensible default answers.
3. **Build it with its tests.** `cargo fmt`, macOS clippy and tests as you go. **Linux-only code isn't done until it has run on Linux** (section 7).
4. **Two independent reviews**, report-only, in parallel: a *clean-code* review (this file's conventions, naming, duplication, size) and a *Murphy's-law* review (concrete failure scenarios, state → wrong outcome, with file:line: processes killed at each point, reboots, upgrades from the previous format, races between two CLIs, hostile local users, resource exhaustion, what the host itself loses).
5. **Verify every finding against the code before acting.** Reviewers are wrong sometimes (the v0.2 review claimed the bridge had no anti-spoofing; `nft/spoofing.rs` says otherwise). Fix what's real, each fix with a test; reject or carry forward the rest, saying why.
6. **Mutation checks** on the critical lines (section 9). A survivor is a missing or vacuous test.
7. **Full verification** (section 7), then **manual checks** as a user would run them, with real images.
8. **Record it:** the plan's "built and reviewed" section, the docs (section 8), the F-rows.
9. **Report** (section 11).

### Closing a stage

1. Gap and regression tests, the milestone end to end.
2. A whole-stage clean-code review and Murphy review: interactions across steps, upgrades, reboots, other programs on the host.
3. Stage notes `docs/stages/NN-name.md` in the established format: Outcome (requirement → where → tested-by; a "moved out" table), Architecture, Decisions and why, Kernel checks, macOS and Linux differences, Failure-mode register (case → handling → test), Fuzzing, Known limitations, Carried forward, Tests, Review history (one row per step).
4. ROADMAP (stage done, decisions, milestone), FEATURES, `docs/user/README.md`'s status and limitations, this file's map, the CHANGELOG.
5. Fuzz every target, run the full verification, do the manual milestone on real hosts.

### The hardening track (applies to every step)

- **H1:** engine logic in a library behind a typed API.
- **H2:** every on-disk format versioned on first write, with a previous-version test.
- **H3:** a threat model in every plan.
- **H5/H8:** anything that replaces a standard tool gets a differential test against it, and a line in the ROADMAP's H8 record saying why it's ours.
- **H6:** test the kind of an error, not its words.
- **H7:** a timing budget for anything on a run's path (an end-to-end test that fails if it gets slower than a generous bound).
- **H4:** CI's Ubuntu isn't the test container (AppArmor, systemd-resolved's `127.0.0.53`, another cgroup layout): say so when a change touches them.

---

## 7. Verify (the check of record)

**All of it, in one command:** `.claude/skills/shipyard-verify/scripts/verify.sh`
(`--mac` for minutes, no flag for everything including both Linux passes, about an
hour: run it in the background). Logs in `target/verify/`, a summary with the totals at
the end, non-zero if anything failed. The pieces it runs:

**At night,** `scripts/watch.sh` (a launchd agent once installed) checks `origin/main` the
same way, plus 10 minutes of fuzzing per target and cargo-mutants on what changed: its
report is `~/Library/Caches/shipyard-watch/runs/latest/report.md`. Read it at the start of
a session; a regression it found comes before new work.

### Every change, on macOS

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
cargo deny check
UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference   # after any flag or help change
```

### Linux (any change that touches Linux code, so: almost every change)

macOS can't cross-check Linux (`ring` needs a Linux C compiler). Use the container:

```sh
timeout 7200 scripts/test-linux.sh            # clippy, then all tests as `tester`, then as root
scripts/test-linux.sh --shell | --root-shell  # debug in the same container
scripts/test-linux.sh --fuzz [SECONDS]        # every fuzz target (do it at a stage's end)
scripts/test-linux.sh --mutants [BASE|FILE.diff]  # cargo-mutants sweep (a report)
scripts/test-linux.sh --conformance [NAME…]   # the OCI conformance suite
```

Both passes run with `--no-fail-fast`, and the root pass runs even if the first failed;
the script fails at the end if either did. Read the totals (`test result:` lines), not just
the exit code.

**Also run in the container** (`.claude/skills/shipyard-verify/scripts/linux-lints.sh`, as root through `run-in-linux.sh`):

```sh
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --document-private-items
apt-get install -y musl-tools && rustup target add aarch64-unknown-linux-musl
CC_aarch64_unknown_linux_musl=musl-gcc cargo clippy --locked --workspace --all-targets \
  --target aarch64-unknown-linux-musl -- -D warnings
```

musl catches what glibc hides (libc types differ: `statfs`'s filesystem type is `u64`;
`time_t` is deprecated there). Prefer nix's typed constants to raw libc types.

### The test container's facts

- Its root is overlayfs, and `/tmp` is a tmpfs: an image store must be on `/tmp` (`SHIPYARD_IMAGE_STORE=/tmp/…`). Unprivileged overlay whiteouts work on tmpfs but not on overlayfs: for whiteout failures, check the filesystem first (`stat -f -c %T`).
- Each run is a fresh container: images pulled in one aren't in the next.
- `tester` gets a delegated cgroup (`user.slice/tester/session`), as a real login has.
- The image is tagged by `scripts/test-image/Dockerfile`'s hash: after changing it, build the new image at once, or every runner fails to find it.
- Docker volumes: `shipyard-linux-cargo` and `shipyard-linux-rustup` (the toolchain: keep), `shipyard-linux-target` (rebuildable, grows past 50 GB: offer to delete after a stage), `shipyard-mutation-target` (per-mutant builds).

### Real distributions (Lima)

The container has no AppArmor or systemd. A milestone is checked by hand on Lima VMs
(Ubuntu 24.04 arm64 with AppArmor's restriction on, Fedora with SELinux enforcing, Ubuntu
x86-64 under QEMU), each through a reboot, with a static musl build. See
`docs/DEVELOPMENT.md`. Copy files in under distinct names (`shipyard-bin`,
`apparmor-profile`: the profile once overwrote the binary), and after setup check
`stat -c %F /dev/null` says "character special file".

---

## 8. Docs are part of done

If a change affects what users see (commands, flags, behaviour, exit codes, messages):

- update the matching page in `docs/user/` (only what works in the current version);
- regenerate `cli.md`: `UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference` (a test fails while it's stale);
- add a CHANGELOG line under the unreleased version (Added / Changed / Fixed / Security / Known limitations);
- **when a message changes, grep everything for the old wording**: `crates/*/src` (unit tests there are often Linux-only and invisible on macOS), `crates/*/tests`, `docs/user`, `docs/book`.

A finding you don't fix becomes an F-row in the ROADMAP's open findings (severity, the
finding with its source, the fix, its home stage). Remove the row when its fix lands.

The learning book (`docs/book/`, gitignored, the user's) is updated after a step or stage
lands: the agent reads the code first, describes only real behaviour, uses short plain
sentences, and verifies every "Try it" command by running it (never writing a device).
Brief book agents with `.claude/skills/shipyard-book/references/book-agent-brief.md`.

---

## 9. Testing: what makes a test count

- **A negative check needs a positive control in the same test.** "Can't reach X" passes vacuously if X is unreachable anyway (lo down, no route back, wrong port).
- **Pin messages precisely** (`contains("is internal: nothing on it can be reached")`, not `contains("internal")`), or better, the error's kind.
- **No sleeps for synchronisation.** Wait on the condition (`eventually(…)`), the lock, the pidfd. A sleep followed by a negative assertion passes on a slow host.
- **A fix against a race needs a test that loses the race without it.** Check the mutant fails *for the right reason* (read its panic line).
- **Root network tests never touch the host's networking:** `in_new_netns`, `NetworkDir`, `Upstream` (a namespace standing in for the world). A new namespace has `lo` down and inherits the host's `ip_forward`.
- **CI's root job runs each test binary in a network namespace of its own** (`scripts/ci-netns.sh`), so root tests can make bridges without touching the runner's network.
- **CI's runner isn't the test container:** Ubuntu's `/etc/resolv.conf` names systemd-resolved's `127.0.0.53` (runs with a network of their own warn about DNS; `test-linux.sh` binds a loopback `resolv.conf` to match). A test that checks stderr looks for its own line, never assumes it's the only one.
- **Rootless from root:** `TestUser` (with its own `XDG_RUNTIME_DIR`; `delegated_cgroup` for `--memory`).
- Root-only tests are `#[ignore = "needs root"]`; run them with `--include-ignored`. Tests that can't run here return early unless `SHIPYARD_TEST_ROOTLESS`/`…DELEGATED=required` (as `test-linux.sh` sets), so **a green run without those variables proves less than it looks**.
- Shared helpers live in `shipyard-test-support`; extend it instead of copying a helper.
- Test limits off record boundaries, and assert the path you meant was taken (`len < limit`).

### Mutation checks (the check of record)

Use `.claude/skills/shipyard-mutants/scripts/mutation-runner.py`: copy it to a scratch
directory, fill in `MUTANTS` (`name: (where, file, exact text, replacement, test command)`,
where `where` is `mac`, `tester` or `root`), then:

```sh
python3 mutation-runner.py --control   # each mutant's test on unchanged code: must all pass
python3 mutation-runner.py             # CAUGHT / SURVIVED / NO-RESULT per mutant
```

- **Controls first.** A test that fails anyway "catches" every mutant and proves nothing.
- **One scratch copy and one cargo target dir per mutant.** A shared target dir runs the wrong binary (two mutants with identical failure lists are suspect).
- SURVIVED needs `test result: ok`; anything else without a failure is NO-RESULT.
- Run a mutant where its test runs: a Linux-only test on macOS runs zero tests and "survives".
- `scripts/test-linux.sh --mutants` is a sweep beside it, not instead: it runs unprivileged, so mutants only root tests catch show as missed.

---

## 10. Gotchas that already cost hours

- **Stale cargo cache:** names that plainly exist are "not found". Compare source mtimes with `target/debug/deps/lib<crate>-*.rmeta`; `touch` the crate's sources (never `cp -p`/`rsync -t` older copies back in).
- **`cargo test` stops at the first failing binary:** always `--no-fail-fast`, or a count is incomplete.
- **Scripted edits:** assert every anchor occurs exactly once *before* writing anything; regex refactors catch more than meant (`at(` matched `Shipyard::at(`): grep the result.
- **A file any user can open is a file any user can `flock`:** keep liveness lock files out of other users' reach.
- **`flock` locks belong to the open file description, and nix's `Flock` unlocks on drop** (`LOCK_UN`), even if a dup is still open.
- **The run's subreaper reaps leftover children:** code waiting for its own child afterwards must treat `ECHILD` as already reaped. Start long-lived helpers before `Supervisor::install`.
- **Networking (stage 7):** set a bridge's MAC (unset, it changes when a port leaves); `accept_dad=0` on bridges and containers; set `accept_ra=2` before turning IPv6 forwarding on; writing `ip_forward=1` resets every interface's `forwarding`; forwarding must never make the host a router for anyone else.
- **`MSG_CTRUNC`:** on `EMFILE` the kernel installs some descriptors, truncates, and still reports those it installed; nix's `cmsgs()` then returns none (F54).
- **`poll` on a pipe's write end with no readers:** `POLLERR` on Linux, `POLLHUP` on macOS.
- **systemd:** `ExecStop` runs only for a unit that started; session scopes are killed at shutdown in parallel with units' `ExecStop`; a monitor in a session scope dies first (hence `shipyard/monitors/<id>`).

---

## 11. Working with the user

- **Explain the systems concept** behind each step: which syscall, what the kernel does, which TLPI chapter (e.g. `flock` §55.2, `SCM_RIGHTS` §61.13.3, `poll` §63.2). In the plan and in the report.
- **Report plainly, with numbers:** what was built; what the reviews found, fixed, rejected (and why); verification (tests passed/failed per pass, mutants caught/total with controls, fuzz time); what has no test and why; what's carried forward. If something failed or was skipped, say so. Don't call a thing done that hasn't run.
- **Keep them posted** on long work: a line when a long job starts, its result when it ends.
- A score or a verdict is backed by evidence you checked, not by a reviewer's say-so.

---

## 12. Agent superpowers (how to work at full strength)

**Parallelism.** Independent reads, searches and checks go in one batch. Long runs
(`test-linux.sh`, mutants, fuzzing) go in the background while you work on what doesn't
depend on them (docs, the next review).

**Fan-out reviews.** For a stage close or a release, run independent fresh-context
reviewers in parallel, one per area, each told to be skeptical, read-only, and to report
severity / file:line / concrete scenario / confidence plus a score:

| Reviewer | Scope |
|---|---|
| Runtime | `shipyard-runtime`: post-fork code, `unsafe`, namespaces and mounts, seccomp, cgroups, the monitor and its protocol, fd passing, PID reuse |
| Engine | `shipyard-engine`: records, lock order, crash consistency at every kill point, restart/recovery, health, events, name resolution |
| Network / image / storage | netlink parsing, firewall rules, leases, pasta, DNS (untrusted input), registry, unpacking, volumes |
| CLI / tests / release | exit codes, escapes, prompts, test quality (vacuous, flaky, root-only), CI, packaging, docs accuracy vs code |
| Security (adversarial) | a hostile container, image, registry, local user; rootless limits; terminal injection |

**Brief every sub-agent with the rules that bind it:** read-only unless told otherwise; no
builds in the shared `target/` while another build runs (give it its own
`CARGO_TARGET_DIR`); `timeout` on every `docker run`; never write a device; never
`limactl shell` unread commands; never commit. **Then verify its claims** before you
repeat them: read the code it cites.

**Self-check before "done":** fmt, clippy (macOS, glibc, musl), rustdoc as CI, macOS
tests, both Linux passes, `cargo deny`, mutants with controls, docs and CHANGELOG, F-rows
for what's left, then the report.

**Keep this file alive.** When you learn a lesson the hard way, add it here (section 10)
in the same change, with the one-line why. When the map changes, update section 3.

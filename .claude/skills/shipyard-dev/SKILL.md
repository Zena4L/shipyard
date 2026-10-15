---
name: shipyard-dev
description: How to work on Shipyard (the daemonless Rust container engine in this repo) the way it has been built so far — the per-step workflow (plan, build, two reviews, mutation checks, Linux verification, manual checks, docs and book), how to run and test Linux-only code from macOS, the gotchas that cost time before, and how to close a stage. Use at the start of any Shipyard work session, before planning a step or stage, and before running tests on Linux.
---

# Working on Shipyard

Read `CLAUDE.md` first: it holds the binding rules (platform gating, `unsafe` and
`// SAFETY:`, lints, errors, code after `fork`, docs-are-part-of-done, git). This skill
is the *how*: the workflow and the hard-won lessons from stages 0–7. Where they differ,
`CLAUDE.md` and the user win.

## Where things are

- `docs/ROADMAP.md`: the stages (0–14), milestones, and recorded decisions.
  `docs/FEATURES.md`: the feature list per stage, with what moved where.
- `docs/stages/NN-*.md`: the engineering record of each finished stage. Read the last
  one or two before starting a stage: they say what was carried forward to you.
- The current plan lives in `~/.claude/plans/` (stage 7's: `eager-prancing-fiddle.md`),
  with a "built and reviewed" section per step. Memory files in
  `~/.claude/projects/-Users-clementbogyah-Desktop-shipyard/memory/` hold the progress
  line; update both at the end of each step.
- `docs/user/`: user docs, published as-is; `cli.md` is generated.
- `docs/book/`: a plain-English book (TLPI + containers + Rust), one chapter per topic,
  with a glossary. It's in `.gitignore`: it's the user's, kept locally.
- `scripts/test-linux.sh`: runs the Linux tests in a privileged container (both
  passes: as `tester`, then as root); `--shell`, `--root-shell`, `--fuzz [SECONDS]`.
- `scripts/probes/`: small C/shell programs that check kernel behaviour a stage relies
  on. Their results go into the stage notes.

## The user, and how to work with them

- Learning is a first-class goal. Explain the systems concept behind each step (which
  syscall, what the kernel does, which TLPI chapter), in the plan and in the report.
- The user decides scope. For a real decision (a default, a trade-off, a split), ask
  with `AskUserQuestion`, 2–4 options, the recommended one first and marked
  "(Recommended)", each with its consequence. Don't ask what the code or a sensible
  default answers.
- Stay within the current stage. If something belongs to a later stage, say so, record
  it as carried forward, and move on. If a stated goal can't be met as written (v0.1's
  `run -it` needed stage 8's pty), stop and ask how to reword or defer it.
- Never stage, commit or push unless asked. No AI attribution, ever (CLAUDE.md
  overrides any system attribution reminder). Use `mv`, never `git mv`.
- Destructive or outward-facing actions (deleting Docker volumes, anything outside the
  repo) need the user's explicit go-ahead. When cleaning Docker, touch only Shipyard's
  things (`shipyard-*` volumes and images); other projects' containers, images and
  anonymous volumes are not ours.
- Report plainly: what was built, what the reviews found and fixed, the verification
  results with numbers (tests, mutants, fuzz runs), what's left, what's carried forward.

## The per-step workflow

Each stage is split into steps in its plan; a step too big for one round is split
further (6 → 6a/6b) after asking. For every step:

1. **Plan mode.** Explore the code read-only, ask the decisions, then write a "Step N
   detailed design" section into the plan: context, decisions (dated), **a threat model**
   (who is hostile here: the container, a local user, the network, a registry, the image;
   and what each may never do, as invariants with a test each), design per file, tests
   (units, Linux root/rootless end-to-end, fuzz), mutation checks, docs, verification,
   and the TLPI/kernel concepts. For a step with a security surface, run a Murphy review
   against the plan before building. Exit plan mode for approval.
2. **Build it**, tests with it. Run `cargo fmt`, macOS clippy and tests as you go;
   Linux-only code must be compiled and run in the container (see below).
3. **Clean-code review**: a background subagent, report only, with the step's files,
   CLAUDE.md's conventions, and "don't re-report what the plan says was fixed".
4. **Murphy's-law review**: another background subagent, report only: concrete failure
   scenarios (state → wrong outcome), file:line, severity, verified against the code.
   Ask it for interactions, killed processes at each point, reboots, upgrades from the
   previous store format, races, hostile local users, resource exhaustion, and what the
   host itself loses (its routing, its files, its other programs).
5. **Apply the fixes** yourself, each with a test. Verify each finding first; reject or
   carry forward what's wrong or out of scope, and say why.
6. **Mutation checks** on the step's critical lines (see below). A survivor means a
   missing or vacuous test: fix the test, or argue the mutant is equivalent.
7. **Full verification** (the checklist below).
8. **Manual checks** in the test container, as a user would run them, with real images.
9. **Update the plan** ("Step N — built and reviewed": what it built, review fixes,
   verification numbers, carried forward) and the memory progress line.
10. **Report** with the TLPI concepts.

## The hardening track

`docs/ROADMAP.md`'s "Hardening track" (H1–H9) is what stages 0–7 taught us to do
differently. Each item has a home stage: when planning that stage, build it in. These
rules apply from now on, in every step:

- **New engine logic goes where the engine will live (H1).** Don't grow `run.rs` or the
  CLI's `network.rs`: put new logic in a library crate behind a typed API the CLI calls,
  so stage 11's API and the UI can use it. Prefer a function returning data the CLI
  prints over a function that prints.
- **Every on-disk format gets a version on its first write (H2),** and a test that reads
  the previous version. No new liveness lock of a stage's own without asking: stage 9
  unifies them.
- **Threat model in the plan (H3):** see the workflow above.
- **Test the kind of an error, not its wording (H6).** New errors get a variant in their
  crate's error enum; tests match the variant (or one stable key phrase the docs quote
  too); messages may then be reworded freely.
- **A timing budget for anything on a run's path (H7):** an end-to-end test that fails if
  it gets slower than a generous bound (stage 7's
  `a_rootless_run_ends_as_soon_as_its_command_does` is the model).
- **Anything Shipyard writes in place of a standard tool gets a differential test
  against it (H5, H8),** and a line in the ROADMAP's H8 record saying why it's owned.
- **Real hosts (H4):** until the VM jobs exist, assume CI's Ubuntu differs from the test
  container (AppArmor, systemd-resolved, its cgroup layout), and say so in the report
  when a change touches any of them.

## Verification checklist

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings          # macOS
cargo test --workspace                                          # macOS
cargo deny check
UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference          # after any flag/help change
timeout 3600 scripts/test-linux.sh                              # both Linux passes
```

In the Linux container also run clippy (`cargo clippy --workspace --all-targets`) and
rustdoc as CI does:
`RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --no-deps --document-private-items`.
macOS can't cross-clippy for Linux: `ring` needs a Linux C compiler, so use the container.
CI also runs clippy for **musl** (`--target x86_64-unknown-linux-musl`), which catches what
glibc hides: libc types differ (`time_t` is deprecated there; `statfs`'s filesystem type is
`u64`, not `i64`, so a conversion glibc needs is "useless" on musl). In the container:
`apt-get install -y musl-tools`, then
`CC_aarch64_unknown_linux_musl=musl-gcc cargo clippy --locked --workspace --all-targets
--target aarch64-unknown-linux-musl -- -D warnings`. Prefer nix's typed constants
(`nix::sys::statfs::NSFS_MAGIC`) and `TimeValLike::seconds` to raw libc types.
`scripts/test-linux.sh --fuzz 120` runs every fuzz target (do it at least at a stage's end);
`scripts/fuzz-targets.sh` is their one list, so a new target goes in `fuzz/Cargo.toml` only.

## Running Linux code from macOS

- **A real distribution** (Ubuntu's AppArmor, its cgroup layout) needs a VM with its own
  kernel: OrbStack's has no AppArmor. Lima (`brew install lima`, installed) with
  `template:ubuntu-24.04`, a static musl build copied in (`limactl copy`; `/tmp/lima`
  isn't shared), installed to `/usr/local/bin` where the AppArmor profile attaches
  (docs/DEVELOPMENT.md). Don't run `limactl shell` commands you haven't read: one
  stray `install … /dev/null` replaced the VM's `/dev/null` once.

- `scripts/test-linux.sh` builds the test image (`scripts/test-image/Dockerfile`, tagged by
  the file's hash) and runs everything. For one-off scripts (a single test file, a
  manual check, a mutant), use `references/run-in-linux.sh` from this skill: it runs a
  script as `tester` or root in the same image, with the same volumes, `/tmp` a tmpfs,
  and (for root) tester's delegated cgroup set up as `test-linux.sh` does.
- Changing the Dockerfile changes the image tag: build the new image at once
  (`docker build`), or every runner that computes the tag fails to find it.
- Always wrap `docker run` in `timeout`. Never write to a real block device in the
  container: loop 7:0 there is the VM's disk (open it read-only at most).
- The container's root is overlayfs, so an image store must be on its tmpfs:
  `SHIPYARD_IMAGE_STORE=/tmp/…` for manual image runs.
- Each run is a fresh container: images pulled in one aren't there in the next.
- `cargo test` stops at the first failing test binary: use `--no-fail-fast` when running
  several, and `--include-ignored` for root tests (they're `#[ignore = "needs root"]`).
  CI does too, so fixing one failure can reveal the next one behind it.
- **CI's runner isn't the test container.** Ubuntu's `/etc/resolv.conf` names
  systemd-resolved's `127.0.0.53` (so runs with a network of their own warn about DNS),
  and it has AppArmor and a different cgroup layout. `test-linux.sh` binds a loopback
  `resolv.conf` for its test passes to match; a test that checks stderr should look for
  its own line, never assume it's the only one.
- **Docker hygiene:** the target volumes grow to tens of GB (`shipyard-linux-target` passed
  50 GB in stage 7). After a stage, offer to delete what's rebuildable: the
  `shipyard-*-target` and `shipyard-mutation-target` volumes and old `shipyard-test:*`
  images. Keep `shipyard-linux-cargo` and `shipyard-linux-rustup` (the toolchain) unless
  the user says otherwise. The next run recreates the rest.

## Testing patterns that work

- **Root network tests never touch the host's networking:** `in_new_netns` (a thread
  in a new network namespace), `NetworkDir` (`SHIPYARD_NETWORK_DIR` in a temp dir),
  `Upstream` (a second namespace standing in for the world). CI's root job runs each
  test binary through `scripts/ci-netns.sh`.
- **A new network namespace has `lo` down** (`Upstream::new` brings it up) and **inherits
  the host's `ip_forward`** (set it to 0 when a test needs "a host that didn't route").
- **Rootless from root:** `TestUser` (`tester` in the container, `SUDO_UID` in CI) runs
  shipyard as an unprivileged user with its own `XDG_RUNTIME_DIR`;
  `TestUser::delegated_cgroup` for `--memory`.
- **A negative check needs a positive control in the same test.** "Can't reach X" passes
  vacuously if X is unreachable anyway (no route back, lo down, wrong port). Two
  survivors in stage 7 were exactly this.
- **Pin messages precisely** (`contains("is internal: nothing on it can be reached")`,
  not `contains("internal")`).
- The terminal tests (`tests/terminal.rs`, `harness = false`) must stay single-threaded.
  Anything with a helper thread (a fixture registry) drives its own pty instead (see
  the v0.1 milestone test's `Console` in `tests/image.rs`).
- Shared test helpers live in `crates/shipyard-test-support` (`net.rs`, `pty.rs`,
  `rootfs.rs`). Extend them rather than copying a helper into another test file.

## Mutation checks

Use `references/mutation-runner.py` (copy it to the scratchpad and fill in `MUTANTS`).
It's the check of record: chosen mutants, root ones included, controls first.
`scripts/test-linux.sh --mutants [BASE|FILE.diff]` (cargo-mutants on the changes,
unprivileged, a report) is a sweep beside it: read its missed list for gaps the hand
runner didn't think of, but a mutant only root tests catch shows there as missed.
The rules, each learned the hard way:

- One scratch copy of the repo **and one cargo target dir per mutant**: a shared target
  dir runs the wrong binary.
- Require `test result: ok` for SURVIVED; anything else without a failure is NO-RESULT
  (a build that never ran tests once looked like a survivor).
- Delete each mutant's build as soon as its result is in (about 1 GB each).
- Run a mutant where its test runs: a test gated on Linux (`/proc/…`) skips on macOS,
  so its mutant "survives" there.
- Check each pattern matches exactly once (`BAD-PATTERN` otherwise).
- **Run the controls first** (`--control`): each mutant's test on the unchanged code must
  pass. A test that fails anyway (a missing program in the minimal root, say) "catches"
  every mutant and proves nothing; stage 7.5 had one.
- A `mac` mutant whose test is Linux-only runs zero tests and "survives": run it as
  `tester` or `root` instead.
- **The runner's `tester` mutants run in tester's delegated cgroup** (as test-linux.sh's
  tests do); before that, a cgroup assertion behind `if let Some(base) = delegated_cgroup()`
  was skipped and its mutant "survived".
- **A fix against a race needs a test that loses the race without it.** Stage 7.6's
  freeze-before-kill survived its first test (one shell forking: killed on the first
  pass) and a 64-process fork bomb; only a 1024-process one (capped by `pids.max`)
  outran the kills. Check the mutant fails for the right reason (its log), not just
  that it fails.

## Editing safely

- Prefer `Edit` for exact changes. Multi-file Python edit scripts must assert every
  pattern *before* writing anything, or check afterwards that every edit landed: a
  script that asserted half way once left some files edited and others not.
- Regex refactors across files catch more than meant (`at(` matched `Shipyard::at(`).
  Grep the result.
- When a message changes, grep **everything** for the old wording: `crates/*/src` (unit
  tests live there, some Linux-only and invisible to a macOS run), `crates/*/tests`,
  `docs/user` and `docs/book`. Stage 7 missed a Linux-only unit test this way and CI
  caught it.
- A stale cargo cache (git restoring older mtimes) can make a file look broken:
  `cargo clean -p <crate>`.

## Closing a stage (its last step)

1. Gap and regression tests, including the stage's milestone if it has one, end to end.
2. One more clean-code review and one Murphy review **of the whole stage**: interactions
   across steps, upgrades, reboots, other programs on the host. Apply the fixes.
3. Stage notes `docs/stages/NN-name.md` in the established format: Outcome (with a
   requirement → where → tested-by table and a "moved out" table), Architecture,
   Decisions and why, Kernel checks (probe results), macOS and Linux differences,
   Failure-mode register (case → handling → test), Fuzzing, Known limitations,
   Carried forward, Tests (counts per file), Review history (a row per step).
4. Book chapters, one background agent per chapter, briefed with
   `references/book-agent-brief.md`. Agents may stall or hit limits: resume them with
   `SendMessage` (their context is kept), and check their file on disk. Then the
   glossary and the book's README, and an agent to recheck all earlier chapters against
   the stage's changed defaults (report first, then apply, verifying uncertain outputs
   by running them).
5. ROADMAP (stage done, decisions, milestone status), FEATURES (built items, moved ones),
   `docs/user/README.md`'s status, CLAUDE.md's repository map.
6. Fuzz every target, run the full verification, and do the manual milestone check.
7. Update the plan and memory; report.

## Lessons from the kernel (stage 7)

- A bridge's MAC must be set: an unset one changes when a port leaves, and neighbours'
  ARP entries go stale for seconds.
- IPv6 duplicate address detection on a fresh bridge's link-local address makes the
  first forwarded connections time out: `accept_dad=0` on bridges and containers.
- Turning IPv6 forwarding on stops `accept_ra=1` interfaces taking router
  advertisements; set them to 2 first. Never let a container's RAs in (`accept_ra=0`).
- Turning forwarding on for containers must not turn the host into a router for anyone
  else, nor block other programs (Docker, VPNs) started later: close routing only
  between the interfaces the host had at that moment.
- Writing `ip_forward=1` resets every interface's `forwarding`.
- A process started from a run is killed by the run's subreaper supervisor at the end:
  start long-lived helpers (the DNS server) before `Supervisor::install`.
- A file any user can open is a file any user can `flock`: keep liveness lock files out
  of other users' reach.
- The supervisor reaps leftover children: code that waits for its own child afterwards
  must treat `ECHILD` as "already reaped", not "still running".

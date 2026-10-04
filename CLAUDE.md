# CLAUDE.md

Guidance for AI coding assistants working in this repository.

## Project

Shipyard is a lightweight, **daemonless** container engine in Rust, an alternative to
Docker / Podman / OrbStack. It has two goals that carry equal weight:

1. **Production-grade software.**
2. **Learning systems programming.** It's built step by step using *The Linux Programming
   Interface* (TLPI) as the reference.

Work follows the staged plan in [`docs/ROADMAP.md`](docs/ROADMAP.md). Each stage builds on
the previous one. Don't jump ahead or build later stages early.

## Repository map

- `crates/shipyard-cli/`: the `shipyard` binary (clap, anyhow, tracing).
- `crates/shipyard-runtime/`: low-level primitives (processes, namespaces, mounts, cgroups).
  Library errors use `thiserror`.
- `docs/ROADMAP.md`: stages, TLPI chapter mapping, and recorded decisions.
- `docs/FEATURES.md`: the full feature list per stage, the production-quality bar, and what's
  out of scope.
- `docs/DEVELOPMENT.md`: setup, commands, and running Linux-only code from macOS (a
  container via `scripts/test-linux.sh`, or the Lima VM).
- `docs/stages/`: stage notes, the engineering record of each finished stage (decisions,
  failure modes, platform differences, items carried forward).
- `scripts/`: developer scripts (`test-linux.sh`).
- `docs/user/`: **user-facing docs**, published to the website as-is (plain Markdown).
  - `cli.md` is generated from the clap definitions; never edit it by hand.
  - Describe only what works in the current version.
- `docs/design/`: UI concept.
  - `shipyard-ui.html`: interactive mock.
  - `README.md`: concepts, interaction rules, tokens, and the data the UI needs. Read this
    before any UI or engine-API work.

## Decisions already made (don't relitigate)

- **Daemonless** (Podman-style): no long-running root daemon; one tiny monitor process per
  container.
- **macOS-first, UNIX-only.**
  - The CLI, engine and UI run natively on macOS and Linux.
  - Kernel isolation is Linux-only. On macOS it runs inside a lightweight Linux VM managed
    by Shipyard (stage 12).
- **OCI runtime:** `shipyard-runtime` implements the OCI runtime spec; the engine calls it
  like Podman calls runc.
- **Syscalls:** use `std` first, then `nix`/`rustix`, and raw `libc` only when neither
  covers the call.

## Conventions

- **Platform gating:**
  - `#[cfg(unix)]` for POSIX code, which must work on both macOS and Linux.
  - `#[cfg(target_os = "linux")]` for namespaces, mounts and cgroups; return
    `Error::LinuxOnly` elsewhere.
- **`unsafe`:** every block needs a `// SAFETY:` comment (clippy denies otherwise).
- **Lints:** clippy `pedantic` is on and CI runs with `-D warnings`. Fix lints rather than
  `#[allow]`-ing them; if an allow is genuinely needed, give it a `reason`.
- **Errors:** `thiserror` enums in libraries, `anyhow` in the CLI. Don't `unwrap()` in
  non-test code.
- **CLI output:** use `writeln!` to a locked stdout, not `println!`, so broken pipes become
  errors instead of panics.
- **Dependencies:** declare versions once in `[workspace.dependencies]`; members use
  `.workspace = true`.

## Code that runs after `fork`

`crates/shipyard-runtime/src/process/child.rs` (and anything it calls) runs in the
child between `fork` and `exec`. If the parent had other threads, any lock they held
(the allocator's, stdio's) stays locked forever in the child. So that code must:

- **never allocate, panic or take a lock**: no `Vec`/`String`/`format!`, no
  `println!`, no `tracing`. The module denies indexing, arithmetic side effects,
  `unwrap`, `expect` and `panic`.
- **call only async-signal-safe functions**, or nix wrappers that are thin calls to
  them. nix's `execve` allocates, so `libc::execve` is used directly.
- **report failures as numbers** (step, errno, detail) over the report pipe, then
  `_exit`. The parent turns them into messages (`failure.rs`).

Everything the child needs is prepared before `fork`, in `plan.rs`.

The terminal tests (`crates/shipyard-cli/tests/terminal.rs`, `harness = false`)
fork a fake shell that keeps running Rust code, so that test binary must stay
single-threaded: never start a thread in it.

## Before calling work done

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets
cargo clippy --workspace --all-targets --target aarch64-unknown-linux-gnu   # Linux-only code, from macOS
cargo test --workspace
```

**Docs are part of done.** If a change affects what users see (commands, flags,
behavior, exit codes, messages):

- update the matching page in `docs/user/`;
- regenerate the command reference with
  `UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference`
  (a test fails while it is stale).

Every public Rust item needs a doc comment; the `missing_docs` lint enforces it.

Linux-only behavior must also be *run* on Linux, not just compiled:
`scripts/test-linux.sh` (a container) or the Lima VM (see `docs/DEVELOPMENT.md`). Test as you build: every stage ships with tests.

## Working with the author

- Explain the systems concept behind each step (which syscall, what the kernel does, which
  TLPI chapter) before or alongside the code. Learning is a first-class goal.
- Keep changes scoped to the current stage.

## Git

- Use [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`,
  `docs:`, `chore:`, …).
- **Never add AI attribution to commits or pull requests:**
  - no `Co-Authored-By: Claude …` trailer
  - no "Generated with Claude Code" line
  - no AI tool listed as an author or contributor

  Commits are authored solely by the repository owner.
- Only commit or push when asked.

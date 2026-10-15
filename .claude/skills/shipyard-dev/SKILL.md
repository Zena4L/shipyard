---
name: shipyard-dev
description: Start here for any work on Shipyard (the daemonless Rust container engine in this repo). Orients a session (where the project stands, what's open, what to read) and routes to the focused skill for the task — a step (shipyard-step), verification (shipyard-verify), mutation checks (shipyard-mutants), reviews (shipyard-review), closing a stage (shipyard-stage-close), the learning book (shipyard-book), a release (shipyard-release). Use at the start of every Shipyard session and whenever unsure which procedure applies.
---

# Shipyard: start here

`AGENTS.md` is the always-on manual (rules, map, architecture, code rules, gotchas). The
`shipyard-*` skills are the procedures, one per kind of task. Where they differ,
`AGENTS.md` and the user win.

## Orient (every session, before any change)

1. `git status` and `git branch --show-current`: what's in flight, on which branch.
   Nothing gets staged or committed unless the user asks.
2. The memory progress line and the current plan in `~/.claude/plans/`: which stage and
   step, what's built, what's next.
3. **The night watch's report** (`~/Library/Caches/shipyard-watch/runs/latest/report.md`,
   from `scripts/watch.sh`): a regression it found comes before new work.
4. The current stage in `docs/ROADMAP.md` and `docs/FEATURES.md`; the previous stage
   notes' **Carried forward**; the open findings (F-rows) whose home is this stage.
5. The code you're about to touch, read before written.

Then tell the user in two or three lines where things stand and what you'll do.

## Which skill for what

| The task | Skill |
|---|---|
| Plan or build a step of a stage (the normal unit of work) | `shipyard-step` |
| Check a change: fmt, clippy (3 targets), rustdoc, deny, macOS + both Linux passes; run one script in the Linux container; a real distribution in a Lima VM | `shipyard-verify` |
| Prove the tests catch what they claim (chosen mutants, controls first) | `shipyard-mutants` |
| A clean-code or Murphy review of a step, a whole-stage review, a release review with five reviewers | `shipyard-review` |
| The last step of a stage: gap tests, whole-stage reviews, stage notes, docs, fuzzing, the milestone | `shipyard-stage-close` |
| Write or update chapters of the learning book (`docs/book/`) | `shipyard-book` |
| Date the changelog, open the PR, merge, tag a version | `shipyard-release` |
| Review any branch on two axes (standards, spec) | `review` |
| Bias toward caution: think first, simplest change, surgical edits, verifiable goals | `guideline` |

**Vendored specialists** (third-party, audited; each keeps its `LICENSE` and has a
"Shipyard notes" block that overrides its defaults):

| The task | Skill |
|---|---|
| Any bug, failing test or surprise: root cause before any fix | `systematic-debugging` (obra/superpowers) |
| A deep Rust security audit of a crate: `unsafe` boundaries, memory safety, concurrency, panics as DoS, FFI (many parallel workers, per crate) | `rust-review` (Trail of Bits) |
| A bug was found: where else does the same root cause occur? | `variant-analysis` (Trail of Bits) |
| Fuzzing: a target (`cargo-fuzz`), a better harness (`harness-writing`), what it reaches (`coverage-analysis`), tokens for a parser (`fuzzing-dictionary`) — always in the Linux container | Trail of Bits' testing handbook |
| Create, test or tune a skill (its description, evals) | `skill-creator` (Anthropic) |
| Stress-test a plan, a decision or the project's direction: a relentless interview, round by round, with a recommended answer to each question | `/grill-me` → `grilling` (Matt Pocock) |

Where a reviewer (`shipyard-review`) finds a bug, follow with `variant-analysis`; where
the runtime crate changes, `rust-review` scoped to it is the deep pass beside the
five-area review.

## The non-negotiables, in one breath

No commits, pushes, merges or tags unless asked; no AI attribution anywhere; `mv` not
`git mv`; confirm destructive or outward actions; never write a real block device;
`timeout` on every `docker run`; never an unread `limactl shell` command; docs are part
of done; stay in the stage; explain the TLPI concept behind the work. (`AGENTS.md`
section 2 has the why of each.)

## End of every piece of work

Update the plan's "built and reviewed" section and the memory progress line; report
plainly with numbers (tests, mutants, fuzz time), what has no test and why, and what's
carried forward. A lesson every agent needs goes into `AGENTS.md` section 10.

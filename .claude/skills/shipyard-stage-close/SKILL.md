---
name: shipyard-stage-close
description: Closing a Shipyard stage (its last step) — gap and milestone tests, whole-stage clean-code and Murphy reviews, the stage notes in the established format (template included), ROADMAP/FEATURES/user docs/CHANGELOG/AGENTS.md map updates, fuzzing every target, the full verification, the manual milestone on real hosts, the book, and the report. Use when the last step of a stage begins, and to check a stage is really done before saying so.
---

# Closing a stage

A stage is done when everything below is true, each with evidence. Plan it as the stage's
last step (it's usually split: a = tests and reviews, b = notes and the manual milestone,
c = fresh-eyes reviewers), and go through `shipyard-step` for each part.

## 1. Tests that prove the stage
- **Gap tests:** every requirement in FEATURES §N has a test (the Outcome table in step 3
  will show any gap).
- **The milestone end to end**, if the stage reaches one, as a user would run it, in one
  test through the CLI.
- **Regression tests** for what earlier stages promised and this stage touched.
- **Timing budgets** for anything new on a run's path (H7).

## 2. Whole-stage reviews (`shipyard-review`)
A clean-code and a Murphy review of the **whole stage's diff**: interactions across
steps, upgrades from the previous stage's formats, reboots, other programs on the host.
Then, ideally, fresh-eyes reviewers who saw none of it. Verify, fix with tests, record
the rest as F-rows.

## 3. The stage notes
`docs/stages/NN-name.md` from [references/stage-notes-template.md](references/stage-notes-template.md):
Outcome (requirement → where → tested-by; moved out), Architecture, Decisions and why,
Kernel checks, macOS and Linux differences, Failure-mode register (case → handling →
test), Fuzzing, Known limitations, Carried forward, Tests (counts per file), Review
history (a row per step). Every claim checked against the code; every test name real
(grep it).

## 4. Everything else that says what's true
- `docs/ROADMAP.md`: the stage done, its decisions, the milestone's status, F-rows opened
  and closed.
- `docs/FEATURES.md`: §N marked built; what moved where.
- `docs/user/`: every page the stage touched; `README.md`'s status and limitations;
  `cli.md` regenerated.
- `CHANGELOG.md`: the version's Added / Changed / Fixed / Security / Known limitations.
- `AGENTS.md`: the map (section 3), the architecture (section 4), new gotchas (section 10).
- The book (`shipyard-book`): the stage's chapters, the glossary, an earlier-chapters
  recheck against changed defaults.

## 5. Proof
1. `scripts/test-linux.sh --fuzz 300` (every target; longer for new parsers).
2. `verify.sh` (all) — numbers per pass.
3. Mutants on the stage's critical lines, controls first (`shipyard-mutants`).
4. **The manual milestone** on real hosts (`shipyard-verify`'s Lima section): rootless and
   root, through a reboot, on each distribution the milestone names.

## 6. Close out
- The plan's last "built and reviewed" section, and the memory progress line ("stage N
  DONE <date>").
- Offer Docker hygiene (the target volumes) — ask first.
- The report: what the stage delivered (user-visible), the numbers, the reviews' worst
  findings and their fixes, what's carried forward, what the next stage starts with.
- Committing, the PR and the tag are `shipyard-release`'s, when the user asks.

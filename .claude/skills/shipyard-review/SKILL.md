---
name: shipyard-review
description: Independent reviews of Shipyard work by fresh-context sub-agents — a clean-code and a Murphy's-law review of a step (or of a plan before building), a whole-stage review, and a five-reviewer production-readiness review (runtime, engine, network/image/storage, CLI/tests/release, adversarial security) with a score. Includes the briefs to give each reviewer and the rule to verify every finding before acting. Use after building a step, before building a security-sensitive plan, when closing a stage, and before a release or when asked how ready the project is.
---

# Reviews

Reviewers start fresh (no context of yours), read-only, in parallel, in the background.
Their reports are claims, not facts: **you verify every finding against the code before
you act on it or repeat it.** (The v0.2 review said the bridge had no anti-spoofing; it
has, in `nft/spoofing.rs`. Another said a leak was fixed; the code showed it wasn't.)

## Which review

| When | Reviewers | Brief |
|---|---|---|
| A step is built | clean-code + Murphy, in parallel | [clean-code](references/clean-code.md), [murphy](references/murphy.md) |
| A plan with a security surface, before building | Murphy, on the plan | [murphy](references/murphy.md) (give it the plan's path) |
| A stage closes | clean-code + Murphy of **the whole stage**: interactions across steps, upgrades, reboots | both, scoped to the stage's diff |
| A release, or "how ready is this?" | five, one per area, each with a score | [release](references/release.md) |

## Running them

1. **Scope precisely:** the files or the diff (`git diff <base>...HEAD -- <paths>`, plus
   untracked files), the plan's path, and what the plan already fixed ("don't re-report").
2. **Paste the binding rules into every brief** (they read nothing you don't point at):
   read-only; no edits, stages or commits; no cargo builds in the shared `target/` while
   another build runs; `timeout` on any `docker run`; never write a device; no `limactl`.
3. Launch them **in one message** so they run concurrently; keep working meanwhile
   (baseline checks: `verify.sh --mac`, `cargo deny`).
4. **Verify** each finding you'll act on or report: read the cited code, its callers and
   callees. Mark each: confirmed, partly (say what part), wrong (say why).
5. Fix what's real, each with a test that fails without it (`shipyard-step` step 5);
   record what you won't fix as F-rows with severity, source ("the v0.2 review, engine")
   and home stage.

## Reporting

Per reviewer: its score and its worst confirmed finding. Then the confirmed findings,
most severe first, with file:line and the concrete scenario; what was wrong in the
reviews; and your own verdict, backed by what you checked, separating **code quality**
from **release readiness** (CI, artifacts, docs' honesty) — they score differently.

---
name: shipyard-step
description: The per-step workflow for building a piece of a Shipyard stage — plan mode with a threat model and decisions asked, build with tests, two parallel reviews, fixes verified, mutation checks, full verification, manual checks, record and report. Use before planning or starting any step of a stage (or a hardening pass), and to check nothing was skipped before calling a step done.
---

# One step of a stage

A stage is split into steps in its plan (`~/.claude/plans/`). A step too big for one
round is split further (6 → 6a/6b) after asking. Every step goes through all ten
stages below; the report says which were skipped and why.

## 1. Orient
Read the stage's ROADMAP/FEATURES entries, the previous stage notes' **Carried forward**,
the F-rows homed here, and every file you'll touch. Read the last stage notes' failure-mode
register for the area: the bugs found there are the ones to expect.

## 2. Plan (plan mode), then approval
Write a "Step N detailed design" section into the plan from
[references/plan-template.md](references/plan-template.md): context, dated decisions,
**threat model**, design per file, tests, mutants, docs, verification, TLPI concepts.
- **Ask the real decisions** (a default, a trade-off, a split): 2–4 options, the
  recommended first and marked "(Recommended)", each with its consequence. Don't ask what
  the code or a sensible default answers.
- **A step with a security surface** (parsing untrusted input, privileges, mounts,
  networking, anything another user or a container controls) gets a Murphy review of the
  *plan* before building (`shipyard-review`).
- If a stated goal can't be met as written, stop and ask how to reword or defer it.
- Exit plan mode for approval.

## 3. Build, with the tests
Tests are written with the code, not after. Follow `AGENTS.md` sections 4–5 (lock order,
versioned formats, post-fork rules, typed errors, escaping). Run `cargo fmt`, macOS
clippy and tests as you go; **Linux-only code is compiled and run in the container**
before you call it built (`shipyard-verify`).

## 4. Two reviews, in parallel, report only
A clean-code review and a Murphy's-law review, as background agents briefed from
`shipyard-review`'s references, given the step's files and "don't re-report what the
plan says was fixed".

## 5. Apply the fixes
**Verify each finding against the code first**: reviewers are sometimes wrong. Fix what's
real, each fix with a test that fails without it. Reject or carry forward the rest, and
say why in the plan.

## 6. Mutation checks
On the step's critical lines, controls first (`shipyard-mutants`). A survivor is a
missing or vacuous test: fix the test, or argue the mutant is equivalent.

## 7. Full verification
`verify.sh` (`shipyard-verify`): macOS, both Linux passes, rustdoc, musl. Numbers, not
"it passed".

## 8. Manual checks
In the test container (`--shell` / `--root-shell`), as a user would run them, with real
images; on a Lima VM when the step touches AppArmor, systemd, SELinux or a real cgroup
layout.

## 9. Record
- The plan: "Step N — built and reviewed": what it built, the review fixes, the
  verification numbers, what's carried forward.
- Docs (`AGENTS.md` section 8): `docs/user/`, `cli.md` regenerated, CHANGELOG, F-rows for
  what's left.
- The memory progress line.

## 10. Report
What was built and the systems concept behind it (the syscall, what the kernel does, the
TLPI chapter); what the reviews found, fixed and rejected; verification with numbers;
what has no test and why; what's carried forward; what's next.

## The hardening track (every step)
H1 engine logic in a library behind a typed API · H2 every on-disk format versioned on
its first write, with a previous-version test · H3 a threat model in the plan · H5/H8 a
differential test for anything that replaces a standard tool · H6 test an error's kind,
not its words · H7 a timing budget for anything on a run's path · H4 CI's Ubuntu isn't the
test container: say so when a change touches AppArmor, systemd-resolved or cgroups.

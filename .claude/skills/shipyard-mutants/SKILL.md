---
name: shipyard-mutants
description: Mutation checks for Shipyard — prove each test catches the bug it claims to by putting the bug back (a chosen mutant per critical line), with controls first, one scratch copy and one cargo target dir per mutant, run on macOS or in the Linux container as tester or root. Use after building a step or a fix, before calling its tests sufficient, and to triage a cargo-mutants sweep.
---

# Mutation checks

A test that passes on broken code proves nothing. The check of record is a handful of
**chosen** mutants on the lines that matter (the fix, the guard, the invariant), each with
the test that must catch it.

## Run them

1. Copy the runner to scratch (never edit the working tree for a mutant):
   `cp .claude/skills/shipyard-mutants/scripts/mutation-runner.py "$SCRATCH/"`
2. Fill in `MUTANTS`:
   ```python
   "short-name": (
       "tester",                      # mac | tester | root: where its test runs
       E + "run/detached.rs",         # file (C, E, R, N, I: crate src prefixes)
       "exact original text",         # must occur exactly once
       "the bug put back",
       DETACHED + "the_test_name"),   # the test command
   ```
3. **Controls first**: `timeout 5400 python3 mutation-runner.py --control`. Every line
   must say `CONTROL-OK`; a test that fails anyway "catches" every mutant.
4. Then `timeout 5400 python3 mutation-runner.py [NAME…]`: `CAUGHT`, `SURVIVED`,
   `NO-RESULT`, `BAD-PATTERN`.
5. **Read the panic line of every CAUGHT** (`mutants/NAME.log`): it must fail on the test's
   own assertion, not because something else broke.

## The rules (each learned the hard way)

- One scratch copy and **one cargo target dir per mutant** (the runner does this): a shared
  dir runs the wrong binary; identical failure lists are the tell.
- `SURVIVED` needs `test result: ok`; anything else without a failure is `NO-RESULT`.
- **Run a mutant where its test runs.** A Linux-only test as a `mac` mutant runs zero tests
  and "survives". Root-only tests are `root`; `tester` mutants run in tester's delegated
  cgroup, as `test-linux.sh` does.
- **A fix against a race needs a test that loses the race without it.** A mutant that
  survives one fork but dies at 1024 processes was a weak test.
- A test with deterministic output can be vacuous at a boundary: put limits off record
  boundaries and assert the path was taken.
- Each mutant's copy and build are deleted once its result is in (about 1 GB each);
  the Linux ones build in the `shipyard-mutation-target` volume.
- Some fixes can't be mutated deterministically (a failure that needs `/proc` unreadable;
  a race window of microseconds): say so in the report, and name the regression tests
  that cover the code around them.

## The sweep beside it

`scripts/test-linux.sh --mutants [BASE|FILE.diff]` runs cargo-mutants on the changed
lines (`.cargo/mutants.toml`), unprivileged, as a report. Read its "missed" list for gaps
you didn't think of; a mutant only root tests catch shows there as missed, so check those
by hand with the runner.

## Report

`N/N caught (controls N/N)`, which test caught each, any survivor with its verdict
(a fixed test, or why it's equivalent), and what couldn't be mutated.

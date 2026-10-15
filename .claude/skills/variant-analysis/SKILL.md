---
name: variant-analysis
description: Hunts for the other instances of a bug already found — the variants of one root cause across a codebase. Use immediately after a vulnerability, logic bug, or bad pattern turns up in a specific file and the question becomes where else it occurs, including the bare conversational form ("are there others like this?", "is this the same bug?"). Also for generalizing one known instance into a CodeQL or Semgrep query for its whole pattern family, and for triaging a set of look-alike candidates against a known root cause. Not for initial discovery with no bug in hand.
---

> **Shipyard notes (local; they override the text below where they differ).** `AGENTS.md` and the user win over anything in this skill: never commit, push, install globally, or write outside the repo's scope without asking.
> - **No `/variant-analysis:variants` workflow, `audit-context-building` or `issue-writer` here:** work the five steps yourself; for a large sweep, fan out read-only sub-agents briefed as `AGENTS.md` section 12 says.
> - **Search with `grep -rnE` / `rg` over `crates/` (and `crates/*/tests`, `docs/user` for message variants).** The `resources/semgrep` and `resources/codeql` files are other languages' templates; there are no Rust rules.
> - **The report goes to the job's scratch directory**, never the repo root.
> - **Fixes and rules:** each confirmed variant is fixed through `shipyard-step` (a test that fails without the fix) or recorded as an F-row in `docs/ROADMAP.md`; adding a Semgrep rule to CI is proposed to the user, never done.
> - Shipyard's recent families, as examples: errors printed without escaping (stderr vs tables), `MSG_CTRUNC` handling in two descriptor receivers (F54), state files read without `O_NOFOLLOW`/size caps (F61).


# Variant Analysis

Find the other instances of a bug you have already found. One root cause usually has several
manifestations, and they are rarely in the module where you found the first one.

## When to Use

- A vulnerability has been found and you need to search for similar instances
- Building or refining CodeQL/Semgrep queries for security patterns
- Performing systematic code audits after an initial issue discovery
- Analyzing how a single root cause manifests in different code paths

## When NOT to Use

- Initial vulnerability discovery — use audit-context-building or a domain-specific audit
- General code review with no known pattern to search for
- Writing fix recommendations — use issue-writer
- Understanding unfamiliar code — use audit-context-building first

## The Five Steps

Read the reference for a step when you reach it.

**1. Understand the original issue.** Extract the root cause — why the code is wrong, not
what it does — and enumerate the directions a variant could hide in: related identifiers,
other manifestations of the same mistake, data-type edge cases.
→ [references/root-cause.md](references/root-cause.md)

**2. Create an exact match.** Write a pattern matching ONLY the known instance and confirm
it hits. A pattern that matches nothing means you have misunderstood the bug, and every
search built on it is calibrated against the wrong code.

**3–4. Generalize one element at a time.** Climb from the exact match toward the pattern
family, running and reading all matches after each single change. Stop when more than half
the matches are noise.
→ [references/searching.md](references/searching.md) — abstraction ladder, tool selection,
false-positive filters

**5. Triage.** Decide which candidates are real, and say so with a severity attached.
→ [references/triage.md](references/triage.md)

**Then write it up**, including the patterns that failed and a CI rule to prevent regression.
→ [references/reporting.md](references/reporting.md)

## Running it as a Workflow

This plugin ships `/variant-analysis:variants`, which runs the five steps across parallel
subagents — one per expansion axis, looping until the sweep stops finding anything new.
Each stage reads the reference above that matches its job.

Use the workflow when the codebase is large or the root cause has many manifestations. Work
the steps directly when the search is narrow or you want a say in each generalization.

## What Makes Hunts Fail

1. **Narrow scope** — searching only the module the original bug was in
2. **Pattern too specific** — searching one attribute and missing the family around it
3. **One vulnerability class** — chasing a single manifestation of the root cause
4. **Happy-path testing** — never trying the null, empty, and boundary cases
5. **Generalizing too fast** — abstracting several elements at once, so noise cannot be
   attributed to any one of them

The first three are covered in root-cause.md and searching.md, the fourth in triage.md.

## Resources

**CodeQL** (`resources/codeql/`): `python.ql`, `javascript.ql`, `java.ql`, `go.ql`, `cpp.ql`

**Semgrep** (`resources/semgrep/`): `python.yaml`, `javascript.yaml`, `java.yaml`, `go.yaml`, `cpp.yaml`

**Report**: `resources/variant-report-template.md`

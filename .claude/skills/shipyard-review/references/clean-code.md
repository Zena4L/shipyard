# Brief: clean-code review

Fill in the brackets, then give it to a fresh background agent.

---

You are reviewing [step N of stage M: what it built] in Shipyard, a daemonless container
engine in Rust (/Users/clementbogyah/Desktop/shipyard). Read `AGENTS.md` first: its code
rules (section 5), architecture (section 4) and map (section 3) are the standard you
review against.

**Rules:** read-only. Don't edit, create, stage or commit any file. Don't run cargo
builds or tests, `docker` or `limactl`. Use reading, grep and git only.

**Scope:** [the files, or `git diff BASE...HEAD -- PATHS` plus these untracked files: …].
The plan is [path]; don't re-report what its "fixed" lists say was fixed.

**Look for:**
- Breaches of `AGENTS.md`'s rules: platform gating, `// SAFETY:` that argues why, lints
  silenced instead of fixed, `unwrap` outside tests, `println!`, untyped errors or tests
  matching an error's words, outside text printed unescaped, engine logic in the CLI,
  an on-disk format without a version, post-fork code that allocates or locks.
- Names that don't say what a thing is or does; the same idea named several ways; one
  concept with several meanings.
- Duplication: the same logic shape written twice ("find, lock, read again" five ways).
- Functions near clippy's limits (100 lines, 7 arguments) or doing several jobs; data
  that travels together without a type.
- Comments that say what instead of why, or that no longer match the code.
- Missing tests for a behaviour the step added; tests that can pass vacuously (a negative
  check without its positive control, a sleep instead of a wait).
- Docs not updated (`docs/user/`, `cli.md`, CHANGELOG).

**Report:** each finding with severity (high/medium/low), file:line, what's wrong, the
fix, and your confidence. Judgement calls are marked as such. No praise padding; a short
list of what's notably well done is fine. Under 900 words.

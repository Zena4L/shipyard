# Working with AI coding agents: a step-by-step playbook

A reusable workflow for getting reliable, high-quality work out of AI coding agents
(Claude Code, Codex, Cursor and the like) on any project. It's distilled from practice:
what held up under real reviews, and what failed and why.

The core idea: **the agent is fast and tireless but can be confidently wrong; you are the
owner of decisions and understanding.** So every step below does one of two things. It
either gives the agent more *context and limits* up front, or it puts an *objective gate*
between the agent's work and "done".

---

## 0. Principles (read once, apply always)

1. **You own decisions; the agent owns facts.** It should look things up (code, docs,
   the web) instead of asking you, and ask you only for decisions, each with a
   recommendation.
2. **Gates beat opinions.** A green test suite, a caught mutant and a reproducible
   benchmark count. "Looks good to me" from an agent doesn't.
3. **Context lives in files, not in chat.** Rules, plans, decisions and findings go in
   the repository, where every session and every agent can read them. Chat is temporary.
4. **Verify every claim before acting on it, including reviewers' claims.** In one
   five-reviewer review, a confident finding ("no anti-spoofing on the bridge") was
   simply false. Another ("this leak was fixed") was wrong the other way.
5. **Small steps, each finished.** A step is done when it's built, tested, reviewed,
   verified, documented and reported, not when the code compiles.
6. **Never let output outrun understanding.** If the agent produces faster than you can
   follow, you're accumulating a codebase nobody understands. Slow down, or make the
   reports teach you.
7. **Separate code quality from release readiness from problem fit.** They're scored
   differently, and a strong score in one hides a weak one in another.

---

## 1. Set up a project once (about an hour)

### 1.1 An `AGENTS.md` at the root: the operating manual

`AGENTS.md` is read by most agents (Codex, Cursor, Copilot, Gemini…). For Claude Code,
make `CLAUDE.md` import it (`@AGENTS.md`) and add only Claude-specific notes, so you keep
one source of truth with no drift. Sections:

| § | Section | What goes in it |
|---|---|---|
| 1 | The project in one minute | what it is, for whom, the goals, the decisions not to relitigate, **where it stands now** |
| 2 | Hard rules | a table: *rule → why*. Commit/push policy, attribution policy, destructive actions, anything that once caused damage |
| 3 | Map | crates/packages and what each may depend on, directories, where state lives at run time |
| 4 | Architecture to keep in your head | process model, data formats and versions, lock order, invariants |
| 5 | Code rules | language conventions, error handling, lint posture, unsafe/security rules |
| 6 | Workflow | the per-step procedure (§4 of this playbook) |
| 7 | Verify | the exact commands, plus the one command that runs them all |
| 8 | Docs are part of done | which docs change when behaviour changes |
| 9 | What makes a test count | controls, no sleeps, mutation checks |
| 10 | Gotchas that already cost hours | one line each, with the *why* |
| 11 | Working with the user | how to explain, how to report, how to ask |
| 12 | Agent superpowers | parallelism, delegation, briefs, self-check |

**Rules that earn their place:**
- "Where this file and the user disagree, the user wins; where it and the code disagree,
  the code is the fact: fix this file in the same change."
- **Check every claim in it against the code.** A first draft had three factual errors
  (dependency directions, a missing entry point, an incomplete list).
- **Keep it alive:** each hard-won lesson goes into §10 in the same change.

### 1.2 Skills: procedures, one per task

A skill is a directory with a `SKILL.md` (frontmatter `name` and `description`, then the
procedure) plus its own `scripts/` and `references/`. The **description** decides when
it loads, so write it as a trigger: "Use when…, before…, after…".

Organise them as a lifecycle with a router:

```
.claude/skills/
├── <project>-dev/         START HERE: orient the session, route to the right skill
├── <project>-step/        one unit of work: plan → build → review → verify → record
│   └── references/plan-template.md
├── <project>-verify/      the gate: scripts/verify.sh (everything in one command)
├── <project>-mutants/     prove the tests catch what they claim
├── <project>-review/      fresh-reviewer briefs (clean-code, failure-mode, release)
├── <project>-close/       closing a milestone: notes template, docs, proof
├── <project>-release/     commit → PR → merge → tag
└── vendored/third-party   specialists (see 1.3)
```

**Two mistakes to avoid:**
- one giant skill that loads for everything;
- skills that duplicate the always-on rules. Rules go in `AGENTS.md`; skills hold
  procedures.

### 1.3 Third-party skills: vendor them, don't trust them

Before adding anyone else's skill:
1. **Download it to scratch, pinned to a commit.** Read every file it runs, scripts
   included. Skills are instructions an agent follows, so treat them like code you
   execute. Watch for:
   - network calls;
   - writes outside the project;
   - deletions;
   - servers or port kills (one popular skill SIGTERMs whatever is on its port);
   - "you MUST use skills for everything" wording;
   - lines that route around a guardrail.
2. **Take only what runs.** Leave out the authors' tests, eval fixtures, logos and other
   tools' metadata.
3. **Keep the upstream text byte-identical**, and add a short "Project notes" block after
   its frontmatter that overrides only what conflicts with your rules: install
   locations, output directories, dangling references to skills you don't have, plugin
   paths.
4. Add a `VENDORED.md`: source, commit, licence (CC-BY-SA requires attribution and marked
   changes), files taken, files left out, what changed, and how to update.
5. **Wire it into the router**, saying when to chain it ("after a reviewer finds a bug →
   variant analysis").

**Specialists worth having** for most code projects:
- **systematic debugging:** root cause before any fix;
- **variant analysis:** "where else does this bug occur?";
- **a language-specific security review;**
- **fuzzing guidance:** if you parse untrusted input;
- **a skill creator with evals:** to tune your own skills;
- **a "grill me" interviewer:** to stress-test plans.

### 1.4 One command that is "the gate"

Write `verify.sh` that runs *everything* CI would, logs each check, prints test totals,
and exits non-zero on any failure. Agents and you call the same command, so "done" means
the same thing for both.
- Include the lints for every target platform. Different targets catch different bugs:
  musl flagged what glibc hid.
- Include docs builds the way CI runs them (private items, warnings as errors).
- Make the slow half runnable in the background (`--quick` / `--full`).

### 1.5 Memory and plans

- **Plans:** one per milestone, with a "built and reviewed" section added per step
  (what, review fixes, verification numbers, carried forward).
- **Memory:** short facts and lessons, each with *why* and *how to apply*, plus an index.
  A lesson every agent needs graduates into `AGENTS.md`.
- **A findings ledger** (e.g. `F1, F2…` in a roadmap): each unfixed finding with
  severity, source, fix and home milestone. Remove the row when the fix lands.
- **A decisions log** (`D1, D2…`), dated: the choice, the reason, the alternatives
  rejected.

---

## 2. Start every session the same way (5 minutes)

1. `git status`, the branch: what's in flight.
2. The plan and memory: which milestone, which step, what's next.
3. **The night watch's report** (§9): a regression comes before new work.
4. The milestone's open findings and carried-forward items.
5. Read the code you'll touch before writing any.
6. The agent states in two or three lines where things stand and what it will do.

Automate it: put this list in the router skill, so it runs without being asked.

---

## 3. Decide before you build: grill the direction

Before a milestone, or whenever the "why" is fuzzy, run a structured interview:

1. **Get the facts first, with sub-agents in parallel:** the landscape, the code's state,
   prior reviews. Facts are the agent's job.
2. **Map the decisions as a tree.** Each round asks only the *frontier*: decisions whose
   prerequisites are settled. Number the questions; give each a recommended answer
   worded so "yes" accepts it.
3. **You answer; the tree reshapes; repeat** until nothing is left assumed. Name the
   trade-offs you accepted (e.g. "no CI and no outside users means every check runs on
   my machine").
4. **Confirm the shared understanding, then write it down:** dated decisions, the new
   milestone order, the definition of done for the release.

Ask three separate confidence questions, with numbers:
- **Is the problem worth solving, and well defined?**
- **Is the code good?**
- **Can it reach the goal as defined?**

A project can score 8 on code and 4 on problem fit. That's the most important thing to
know before writing more code.

---

## 4. One step of work (the core loop)

```
orient → plan → (approve) → build+tests → 2 parallel reviews → verify findings → fix
      → mutants → gate → manual check → record → report
```

### 4.1 Plan (plan mode, read-only)
Use a template so nothing is skipped:
- context;
- dated decisions;
- **threat model:** who is hostile and what they may never do, each as an invariant with
  a test;
- design per file;
- tests: units, end-to-end, fuzz for parsers;
- mutants to run;
- docs to change;
- verification;
- the concepts involved.

Ask the user only the real decisions: 2–4 options, the recommended one first, each
with its consequence. For anything security-sensitive, run a failure-mode review **on
the plan** before building.

### 4.2 Build
- Tests are written with the code.
- Run fast checks as you go; run platform-specific code on that platform, not just
  compile it.
- **Scripted edits:** assert every anchor occurs exactly once *before* writing anything,
  then grep the result. A sloppy slice once deleted 900 lines of an untracked file. A
  broken pattern once replaced a whole block with "X", caught only because the result
  was checked.
- When a message changes, grep *everywhere* for the old wording: tests, docs,
  platform-gated code.

### 4.3 Review: fresh agents, in parallel, read-only
Two reviewers per step, briefed from templates:
- **Clean code:** the project's rules, naming, duplication, size, docs.
- **Failure modes ("Murphy"):** killed at each point, reboot, upgrade from the old format,
  two clients racing, hostile users, resource exhaustion, errors swallowed.

Every brief includes the rules the reviewer must keep (read-only, no builds in the
shared target dir, timeouts, never commit), the exact scope (the diff and untracked
files), and "don't re-report what the plan already fixed".

### 4.4 Verify findings, then fix
Read the cited code for every finding. Mark each one confirmed, partly or wrong. Fix the
real ones, **each with a test that fails without the fix**. Record the rest in the
findings ledger.

### 4.5 Mutation checks: do the tests catch the bug?
For each fix and each critical line, put the bug back and see a test fail.
- **Controls first:** the test must pass on unchanged code, or it "catches" everything.
- One scratch copy and **one build directory per mutant**. A shared one runs the wrong
  binary.
- Read *why* each mutant was caught: the right assertion, not an unrelated crash.
- Say plainly what couldn't be mutated (a race window, an unreachable error) and which
  tests cover around it.

### 4.6 Gate, manual check, record, report
- Run `verify.sh` and **report numbers** (passed/failed/ignored per pass, mutants caught
  of total).
- Do a manual check as a user would.
- Update the docs, changelog, plan, memory and the findings ledger.
- **The report:**
  - what was built and the concept behind it;
  - what the reviews found, fixed and rejected;
  - the numbers;
  - what has no test and why;
  - what's next.

---

## 5. Delegating to sub-agents well

- **Parallelise what's independent:** reviews, research, audits. Launch them in one
  message; keep working on something else while they run.
- **Brief like a colleague who knows nothing:** the goal, the scope, the files to read
  first, the rules, the output format and length, "verify before reporting".
- **Give them separate places to work:** build directories, Docker volumes, scratch
  folders. Two builds in one target dir fight.
- **They report claims; you verify.** Check the two or three findings that would change
  a decision yourself, before you repeat them.
- **Long jobs go in the background** (test suites, mutants, fuzzing), and you're told
  when they finish. Never "sleep and check".

---

## 6. Testing that proves something

- **A negative check needs a positive control** in the same test ("can't reach X" passes
  vacuously if X is unreachable anyway).
- **No sleeps for synchronisation.** Wait for the event that proves the window passed.
  A sleep followed by "nothing happened" passes on a slow machine without proving
  anything.
- **Test an error's kind, not its wording.**
- **Property tests and model tests for state machines,** not only hand-picked scenarios.
  Interleavings live there.
- **Fuzz every parser of untrusted input,** then check what the fuzzer actually reaches
  (coverage).
- **Make vacuous passes impossible:** tests that skip when the environment can't run
  them should *fail* in the gate's environment (`…=required`).
- `cargo test`-style runners stop at the first failing binary: use `--no-fail-fast`, or
  your counts are incomplete.

---

## 7. Shipping

- Commit only when asked; one Conventional Commit with a body that says what and why.
- **Attribution policy in `AGENTS.md`**, so no tool adds its own trailer unasked. Set
  merge commit messages yourself.
- Scan staged changes for secrets and large files.
- **PR body:** summary, review notes, **testing with numbers**, and an honest "not yet"
  (e.g. "CI didn't run: billing").
- Release: date the changelog on the branch, squash-merge, annotated tag on the merge
  commit, check that the tag reached the remote.
- Run your dependency advisory check (`cargo deny`, `npm audit`, …) before tagging. A
  release review found four TLS advisories that a version bump fixed.

---

## 8. Reviews at milestones: the readiness review

At a milestone, run **five fresh reviewers at once**, one per area, each with a score
out of 10:
1. the low-level/runtime layer;
2. the core logic (concurrency, crash consistency);
3. I/O and parsers (network, files, external formats);
4. CLI, tests and release engineering;
5. adversarial security.

Then verify the key claims yourself. Report per area. Separate **code quality**
(e.g. 8.5) from **release readiness** (e.g. 4: never released, CI never ran) from
**problem fit**. Follow with a short hardening pass: fix the confirmed issues and
record the rest.

---

## 9. Automation: what to automate, and what not to

**Automate checking, not building.**

| Automate | Don't automate (yet) |
|---|---|
| A **nightly watch** on `main`: the full gate, long fuzzing, a mutation sweep of what changed; a report every night, **a notification only on regression or when it couldn't run** | an agent that builds features unattended |
| Long jobs in the background while you work | merging or releasing |
| Prepared questions: research done, decisions waiting for you | decisions |
| Bulk *mechanical* edits with an objective gate, in a session you can watch | refactors that reshape code you'll maintain |

**Why not a building loop:** the bottleneck is rarely how fast code gets written. It's
decisions, your understanding, and verification. A loop makes all three worse, and its
errors compound when nobody reviews between steps.

**If you do build one later:**
- a queue of pre-approved steps in a file;
- a narrow authority contract (its own branches only, stop on any decision);
- a scripted gate it can't argue with;
- fresh-context reviewers;
- a cap on unreviewed steps in a row;
- notifications when blocked.

**Night-watch design:**
- its own clone of `origin/main`, its own build dirs and volumes, so it never touches
  your working tree;
- a lock;
- keep the machine awake for the run;
- a missed night runs on wake;
- **silence must mean "it ran and nothing regressed"**, so also alert when it couldn't
  run.

---

## 10. Anti-patterns (each one cost real time)

| Anti-pattern | Instead |
|---|---|
| Trusting a reviewer's finding because it sounds right | Read the cited code; mark it confirmed, partly or wrong |
| Counting tests from a run that stopped at the first failure | `--no-fail-fast`; read the totals, not the exit code |
| Two builds sharing one target directory | One build directory per job, or per mutant |
| A test that sleeps, then asserts nothing happened | Wait on the event that proves the window passed |
| A slice edit whose anchor occurs twice | Assert uniqueness first; back up untracked files |
| A fix "verified" by the build alone | A test that fails without the fix, then a mutant to prove it |
| Installing skills or tools without reading them | Read in full, pin, vendor, override with notes |
| A single giant instructions file that loads for everything | Always-on rules in `AGENTS.md`; procedures in skills |
| Shell surprises (`zsh` doesn't split `$var`; `?` in URLs is a glob) | Quote URLs; use arrays or `bash -c` for word lists; check that an empty result is real |
| Running a scheduled check against code that isn't there yet | Check what the job will see (`origin/main`), not your working tree |
| An agent writing faster than you understand | Reports that teach the concept; smaller steps |
| Only measuring code quality | Also score release readiness and problem fit |

---

## 11. Templates

**Decision question (grill round):**
```
❓ Q3 - <title>: <context, options, each with its consequence>
➡️ <recommended answer, worded so "yes" accepts it>
```

**Reviewer brief (skeleton):**
```
You are a fresh, skeptical <area> reviewer of <project> at <path>. Read <AGENTS.md> first.
Rules: read-only; no edits, commits, or builds in the shared target dir; timeouts on long commands.
Scope: <diff / files / plan>. Don't re-report: <already fixed>.
Look for: <the area's checklist>.
Verify each issue by reading the code paths before reporting.
Report: severity, file:line, concrete scenario, fix, confidence. Score /10 with why. Under N words.
```

**Step report:**
```
Built: <what> — the concept: <syscall / algorithm / protocol, with a reference>
Reviews: <found> → fixed <n>, rejected <n> (why), recorded <F-numbers>
Verification: <platform A> passed P / failed F; <platform B> …; mutants caught X/Y (controls Y/Y)
No test: <what, and why>
Next: <step>
```

**Vendored skill notes block:**
```
> **Project notes (local; they override the text below where they differ).** <the project's rules win>.
> - <install/run only in X; never globally>
> - <outputs to scratch, never the repo>
> - <where it names skill A, use our B>
> - <ask before servers, network, or paid runs>
```

---

## 12. The minimum setup for a new project

1. `AGENTS.md` (§1.1) and `CLAUDE.md` with `@AGENTS.md`.
2. `verify.sh`: the gate, in one command.
3. Three skills: a router, a step procedure with a plan template, review briefs.
4. A findings ledger and a decisions log in the roadmap.
5. A night watch once there's a `main` worth guarding.
6. Before the first big milestone: a grilling session on the problem, and a readiness
   review with five reviewers.

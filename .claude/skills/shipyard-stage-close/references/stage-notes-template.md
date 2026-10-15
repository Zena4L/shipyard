# Stage N notes: <name>

The engineering record of stage N: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [<user page>](../user/<page>.md), ….

## Outcome

What stage N asked for ([ROADMAP](../ROADMAP.md), [FEATURES §N](../FEATURES.md#…)), which
hardening-track items it took on (H…), which F-rows it closed. A user can now:

- <a concrete thing, as a command they'd type>;
- …

| Requirement | Where | Tested by |
|---|---|---|
| <from FEATURES §N> | `crates/…/file.rs` (`function`) | `test_name` |

**Moved out:**

| Item | Now in | Why |
|---|---|---|

## Architecture

The pieces and how they fit: processes (a tree), files on disk (with their versions),
locks (in their order), protocols (with their versions). Diagrams in plain text.

## Decisions and why

One subsection or bullet per decision (D-numbers from the plan, dated): the choice, the
alternatives, why this one, and its cost.

## Kernel checks

What the stage relies on the kernel to do, checked by a probe (`scripts/probes/…`) on the
test container's kernel and on real hosts: the check, the result, the kernel version.

## macOS and Linux differences

What differs (syscalls, `poll` flags, filesystems, what's refused where) and how the
code handles it.

## Failure-mode register

| Case | Handling | Test |
|---|---|---|
| <a process killed at a point; a race; a reboot; an older format; a hostile input> | <what the code does> | `test_name` |

## Fuzzing

Targets added or extended, the corpus seeds, the time each ran, what they found.

## Known limitations (by design, for stage N)

What doesn't work and why it's acceptable now, each with where it would be fixed.

## Carried forward

To later stages, each with its home: F-rows opened, items deferred, decisions to revisit.

## Tests

| File | Tests | Stage N's |
|---|---|---|

Totals: macOS passed; Linux both passes passed / failed / ignored; mutants caught / total
(controls); fuzz time.

## Review history

| Step | What | A finding of its own reviews |
|---|---|---|
| N.1 | <what it built> | <the finding its reviews caught that tests hadn't> |

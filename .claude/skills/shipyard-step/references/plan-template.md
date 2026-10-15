# Step N detailed design: <title>

## Context
Why this step, what it builds on (the previous step's record, the carried-forward items
and F-rows it closes), and what's out of scope (with where it goes instead).

## Decisions (YYYY-MM-DD)
- **D-N.1 <question>:** the choice, the user's answer if asked, and the consequence.

## Threat model
Who is hostile here, and what each may never do. One invariant per line, each with the
test that holds it.

| Actor | May never | Held by (code) | Tested by |
|---|---|---|---|
| a container (root inside) | … | … | … |
| its image / a registry | … | … | … |
| another local user | … | … | … |
| the network | … | … | … |
| a crash (CLI, monitor, `__cleanup` killed at each point; a reboot; an older binary) | leave the store unsound | … | … |

## Design, per file
- `crates/…/file.rs`: what changes and why. New on-disk formats: their version and the
  previous-version test. New locks: where they sit in the lock order.

## Tests
- Units: …
- End to end, Linux: rootless (`tester`) and root (`#[ignore = "needs root"]`), each
  negative check with its positive control; timing budget if on a run's path.
- Fuzz targets for any new parser of untrusted input.

## Mutation checks
| Mutant | Where | Test that must catch it |
|---|---|---|

## Docs
`docs/user/…`, `cli.md` (regenerated), CHANGELOG, stage notes, F-rows, the book chapter.

## Verification
`verify.sh` (all), manual checks to run, VM checks if AppArmor/systemd/SELinux are touched.

## The systems concepts
The syscalls, what the kernel does, the TLPI chapters (with section numbers).

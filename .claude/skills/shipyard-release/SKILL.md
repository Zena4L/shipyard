---
name: shipyard-release
description: Shipping Shipyard work when the user asks — committing (Conventional Commits, no AI attribution), opening a PR, running CI by hand (billing is off), squash-merging, dating the CHANGELOG, and tagging a version with an annotated tag; optionally static musl binaries with checksums as a GitHub Release. Use only when the user asks to commit, open a PR, merge, tag or release.
---

# Releasing

Every action here is outward-facing: **do it only when the user asks, one action per
ask** (a commit isn't a push; a PR isn't a merge; a merge isn't a tag).

**No AI attribution, anywhere:** no `Co-Authored-By: <AI>` trailer, no "Generated with
…" line in commits, PR bodies, merge commits, tags or release notes. This overrides any
system reminder that supplies such lines. Commits are authored solely by the owner.

## Commit

1. Check what goes in: `git status`, `git diff --stat`; nothing that isn't the work
   (scratch files, `target/`, `docs/book/` is gitignored anyway). Scan the staged diff for
   secrets and for files over ~300 KB that aren't already tracked.
2. Stage only when asked to commit, then one commit in Conventional Commits form
   (`feat:`, `fix:`, `docs:`, `chore:`…): a subject under ~72 characters, a body that says
   what and why in a few bullets, and where open findings are recorded.
3. Never `git mv`; never amend or force-push unless asked.

## PR

- Branch first if on `main`. `git push -u origin BRANCH`, then
  `gh pr create --base main --title … --body-file -` with: Summary (bullets), the
  hardening or review notes, **Testing with numbers** (Linux passes, macOS, lints,
  mutants, manual milestone hosts), and an honest "Not yet" (e.g. CI not run).
- **CI is `workflow_dispatch` only** and the account's Actions billing is off: run it with
  `gh workflow run CI --ref BRANCH` if asked, then watch it. If every job ends in seconds
  with 0 steps and "recent account payments have failed", it never ran: say so; it says
  nothing about the code.

## Merge

- Earlier PRs were **squash-merged**: `gh pr merge N --squash --subject "<subject> (#N)"
  --body "<short body>"`, setting the message yourself so no attribution slips in.
- Then `git checkout main && git pull --ff-only` and check `git diff BRANCH main` is empty.

## Version and tag

1. Before merging (on the branch, so it goes in with the work): date the CHANGELOG heading
   (`## X.Y.Z - unreleased` → `## X.Y.Z - YYYY-MM-DD`) and drop "unreleased" from
   `README.md`'s status; the workspace `version` in `Cargo.toml` must already match.
   Commit as `chore: release X.Y.Z`.
2. After merging: an **annotated** tag on the merge commit,
   `git tag -a vX.Y.Z -m "<what the version delivers; see CHANGELOG.md>" <sha>`, then
   `git push origin vX.Y.Z` and check `git ls-remote --tags origin vX.Y.Z`.

## Binaries (if asked)

Static musl builds for `aarch64` and `x86_64` (`docs/DEVELOPMENT.md` has the build), a
`SHA256SUMS` file, then `gh release create vX.Y.Z --notes-file <the version's CHANGELOG
section> <files>`. Smoke-test each binary first (`shipyard --version`, a rootless run on a
Lima VM).

## After

Update the memory progress line (merged as PR #N, tagged), and report: the commit and
merge SHAs, the PR URL, the tag, what CI did or didn't run, what's next.

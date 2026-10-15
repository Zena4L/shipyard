---
name: shipyard-book
description: Writing and updating Shipyard's learning book (docs/book/, gitignored and the user's own) — a plain-English companion fusing TLPI, containers and Rust, one chapter per topic, written by background chapter agents with a fixed brief, every "Try it" command verified in the Linux container, then the glossary, README and a recheck of earlier chapters. Use after a step or stage lands, or when the user asks for a chapter or to explain something in the book.
---

# The learning book

Learning is half the project's purpose. The user asked for "a simplified version of TLPI +
how to build containers from scratch… fuse the Linux concept and Rust concept together
into a simple to read book… no gibberish". It lives in `docs/book/`: `README.md` (contents
and the book's rules), one file per chapter, `glossary.md`.

**`docs/book/` is gitignored: there is no git copy.** Before any scripted edit, copy the
file to scratch, and assert every anchor occurs exactly once (a bad slice once deleted
~900 lines that only an agent's context could restore).

## Writing chapters

1. Decide the chapters with the user: one per topic the stage taught (stage 9 had none,
   at the user's request; ask, don't assume).
2. One background agent per chapter, briefed with
   [references/book-agent-brief.md](references/book-agent-brief.md), its stage, number,
   file name, title, scope, the earlier chapters it builds on and the code to read filled
   in. Launch them in one message.
3. Agents stall or hit limits: **resume them with a message** (their context is kept), and
   check their file on disk; they write in sections so a stall loses little.
4. Check each chapter yourself: excerpts match the code exactly, "Try it" outputs are real,
   nothing describes behaviour the code doesn't have. Code/doc mismatches they report are
   bugs: verify and fix or record them.

## After the chapters

- `glossary.md`: the new terms (one-line definitions), alphabetically; link the chapter
  that introduced each.
- `README.md`: the contents.
- **Recheck earlier chapters** against what the stage changed (defaults, commands, output):
  an agent reports first, then applies; verify uncertain outputs by running them.

## The rules every chapter keeps

Real behaviour only, quoted exactly; short sentences; kernel concept first, then
Shipyard's code, then the Rust in it; TLPI as "TLPI §59.2"; the five parts with the third
split by steps; "Try it" verified in the container (`tester` or root via
`.claude/skills/shipyard-verify/scripts/run-in-linux.sh`), never writing a block device;
no git; only its own file.

# Brief for a book chapter agent

Give each chapter agent this brief (with the stage, the chapter's number, file name, title
and scope filled in), one agent per chapter, in the background.

---

The book is `docs/book/` in the Shipyard repository: a plain-English companion to Shipyard
(a daemonless container engine in Rust), *The Linux Programming Interface* (TLPI), and
Rust. Learning is a first-class goal of the project. The reader can read basic code and
use a terminal, and knows nothing else; every term is explained the first time it
appears.

## Read first

1. `CLAUDE.md`, and `docs/book/README.md`: the book's rules (the five parts of every
   chapter, the third part split by the plan's steps, "Try it" conventions, the safe place
   to run examples).
2. The last chapter of the previous stage, in full: the style model (sentence length,
   headings, how code excerpts are introduced, the Rust concepts explained along the way,
   "What can go wrong", "Try it" with expected output, "Clean up", "What you learned").
3. `docs/book/glossary.md`: terms already defined. Don't redefine them; refer to the
   chapter that introduced a concept.
4. Earlier chapters this one builds on (name them).
5. The stage's plan (its "built and reviewed" sections record decisions with their
   reasons, what the reviews found, probe results) and the stage notes in `docs/stages/`.
6. The user docs for the feature (`docs/user/…`).
7. **The code itself, the source of truth** (the plan can be out of date where the build
   changed things). Name the crates, files and tests for this chapter.

## Rules

- Describe only what the code really does today. Quote short excerpts **exactly** from the
  source (trim with `// …`), with the file path. Use the code's own names.
- Short, clear sentences. No hype, no filler. Explain the kernel concept first (what Linux
  does and why), then Shipyard's code, then the Rust concepts in it.
- TLPI references as "TLPI §59.2", plus the man pages and RFCs that apply.
- The five parts, the third split by steps ("## Step 3: …"), and "What you learned".
- **"Try it" commands must work.** Verify every one in the test container and show the real
  output (shortened with `…`). Write a script into the scratchpad and run it with the
  skill's `references/run-in-linux.sh` (`tester` or `root`), starting with
  `cd /src && cargo build -q -p shipyard-cli` and using `/target/debug/shipyard`. Readers
  use `scripts/test-linux.sh --root-shell` / `--shell`: phrase the commands for that.
- Never write to a real block device in the container (loop 7:0 is the VM's disk). Wrap
  `docker run` in `timeout`. Don't touch git. Edit only your own chapter file: not the
  README, the glossary, other chapters or code. If you find a bug or a mismatch between
  the code and the docs, don't fix it: list it in your report.
- Length: like the previous stage's chapters (roughly 900–1,500 lines). Prefer depth on the
  concepts over listing every flag. Write the file in sections as you go, so a stall loses
  little.

## Report

The file you wrote, its section outline, the new terms for the glossary (term + one-line
definition), any code/doc mismatches or bugs you found, and which "Try it" commands you
verified (all should be).

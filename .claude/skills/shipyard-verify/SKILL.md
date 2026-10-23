---
name: shipyard-verify
description: Shipyard's check of record and how to run Linux-only code from macOS — one command for fmt, clippy (macOS, glibc, musl), rustdoc as CI runs it, cargo deny, the macOS suite and both Linux test passes (tester and root); one-off scripts as tester or root in the test container; real distributions in Lima VMs. Use before calling any change done, whenever Linux code changed, to run a single Linux test or manual check, and for milestone checks on Ubuntu/Fedora.
---

# Verify

GitHub Actions is manual-only (billing off): **this is the check of record**. Report
numbers (passed / failed / ignored per pass), never just "green".

## One command

```sh
S=.claude/skills/shipyard-verify/scripts
$S/verify.sh --mac      # minutes: fmt, clippy, macOS suite, cargo deny
$S/verify.sh            # ~1 hour: the above, then both Linux passes, rustdoc, musl (and the VM's part on a Mac)
$S/verify.sh --linux    # the Linux half only
$S/verify.sh --vm       # the macOS VM's: the guest built from this tree, the VM tests
                        # (no flag runs it too, on a Mac)
```

Logs land in `target/verify/` (one per check); the summary prints the test totals. Run
the full one in the background and keep working. After a flag or help-text change, also
`UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference`.

What it runs, if you need a piece alone:
- `scripts/test-linux.sh`: clippy, then every test as `tester` (rootless, with a delegated
  cgroup), then as root including `#[ignore]`d root tests; both passes `--no-fail-fast`.
  Also `--shell`, `--root-shell`, `--fuzz [SECONDS]`, `--mutants [BASE|FILE.diff]`,
  `--conformance [NAME…]`.
- [scripts/linux-lints.sh](scripts/linux-lints.sh) (as root in the container): rustdoc with
  `-D warnings --document-private-items`, clippy glibc, clippy musl.
- [scripts/guest-crash-rate.sh](scripts/guest-crash-rate.sh) `N P [GUEST_DIR] [OUT_DIR]`
  (macOS, a diagnosis tool, F132): boots N guests P at a time with the VM tests' kind of
  work and prints the rate of crashes and hangs with its 95% bounds. Settings to compare
  through the environment: `SHIPYARD_VM_CPUS`, `SHIPYARD_VM_MEMORY_MIB` (read only for
  diagnosis, said in the VM's `host.log`). Run it alone, the lid open (it `caffeinate`s
  against idle sleep), each run under two hours.

## Against the previous release (a change that must change nothing)

```sh
$S/differential.sh mac tester root   # v0.2.0's binaries against this tree's
```

[scripts/differential.sh](scripts/differential.sh) builds the tag in a worktree of its
own, runs [the corpus](scripts/differential-corpus.sh) (≈170 invocations: refusals,
`--help`, the whole lifecycle by name, networks, volumes, images, the runtime binary)
with both, and diffs stdout, stderr, exit codes and the files left behind, with IDs,
times and ports normalised. "identical (N cases)" or the diff. 10.1's oracle; keep the
corpus growing with what a step changes.

## One script in the Linux container

```sh
timeout 3000 .claude/skills/shipyard-verify/scripts/run-in-linux.sh tester|root SCRIPT
```

[scripts/run-in-linux.sh](scripts/run-in-linux.sh) runs `SCRIPT` the way `test-linux.sh`
runs the tests: the repo read-only at `/src`, `SCRIPT`'s directory at `/scratch`, `/target`
the shared target volume, `/tmp` a tmpfs, and for root, `tester`'s delegated cgroup set up.
Use it for a single test file, a manual check, a probe. Put scratch scripts in the job's
scratch directory, not in the repo.

## The container's facts (each one cost time)

- Its root is overlayfs: an image store must be on `/tmp` (`SHIPYARD_IMAGE_STORE=/tmp/…`).
  Overlay whiteouts work unprivileged on tmpfs, not on overlayfs: check `stat -f -c %T`.
- Every run is a fresh container: pulled images aren't kept.
- The image is tagged by `scripts/test-image/Dockerfile`'s hash: after editing it, build it
  (`docker build --tag shipyard-test:<hash12> - < scripts/test-image/Dockerfile`).
- `/dev/loop0` is the VM's disk: **open devices read-only at most**.
- **Every `docker run` under `timeout`**; the daemon has hung for 40 minutes before.
- Two builds in one target dir at once fight: a second job gets its own
  `CARGO_TARGET_DIR` (or `TARGET_VOLUME=` for `run-in-linux.sh`).
- A green run proves less without `SHIPYARD_TEST_ROOTLESS=required` /
  `SHIPYARD_TEST_DELEGATED=required` (tests that can't run return early): `test-linux.sh`
  sets them.
- **Docker hygiene after a stage** (ask first): `shipyard-linux-target` (rebuildable, grows
  past 50 GB), `shipyard-mutation-target`, old `shipyard-test:*` images. Keep
  `shipyard-linux-cargo` and `shipyard-linux-rustup` (the toolchain). Never touch another
  project's containers, images or volumes.

## A real distribution (Lima)

For AppArmor, systemd units, SELinux, real cgroup layouts and reboots — the milestone
checks (Ubuntu 24.04 arm64 with AppArmor's restriction, Fedora with SELinux enforcing,
Ubuntu x86-64 under QEMU). The procedure is in `docs/DEVELOPMENT.md`. The rules:

1. A static musl build (`--target aarch64-unknown-linux-musl`, release), copied in with
   `limactl copy` under **distinct names** (`shipyard-bin`, `apparmor-profile`: the
   profile once overwrote the binary).
2. **Setup as a script file, read whole, then run**:
   `limactl shell VM -- bash /tmp/sy/setup.sh`. Never send a line you haven't read: a stray
   `install … /dev/null` broke a VM twice.
3. After setup: `stat -c %F /dev/null` must say "character special file".
4. Install to `/usr/local/bin` (where the AppArmor profile attaches).
5. Check rootless and root, then reboot and check again.

# Stage 4 notes: users and security

> **In progress.** This file grows with each step and becomes the full stage notes (same
> format as [stage 3](03-filesystem.md)) when the stage is done.

## Decisions made before starting

- **Device access control moves to stage 5.** It's an eBPF program attached to the
  container's cgroup, and cgroups arrive in stage 5. Stage 4 keeps stage 3's minimal
  `/dev`.
- **Rootless ID mapping: `/etc/subuid` ranges, with a single-ID fallback** (Podman's
  model):
  - With `newuidmap`/`newgidmap` and an entry in `/etc/subuid` and `/etc/subgid`, map
    65,536 IDs: root inside is the caller, and users 1–65535 inside are the caller's
    subordinate range.
  - Without them, map only root inside to the caller, and say why. A `--user` other
    than 0 then fails clearly.
- **seccomp: our own BPF, in Rust.** Profiles (Docker's default, OCI's `linux.seccomp`,
  custom JSON) are compiled to classic BPF by Shipyard's own code: names become numbers
  for the build's architecture, an architecture check comes first, and argument
  comparisons are supported. No C library.
- **Rootless is automatic:** a non-root caller of an isolated `shipyard run` gets a user
  namespace, as with Podman. Root keeps rootful containers, now confined.
  `--no-isolation` still opts out. `shipyard-runtime` gets a per-user state root
  (`$XDG_RUNTIME_DIR/shipyard`).

## Steps

1. **Kernel checks and groundwork:** a probe, the capability names, the syscall table.
2. **Capabilities and `no_new_privs`,** for rootful containers.
3. **seccomp:** the BPF compiler, Docker's default profile, custom profiles.
4. **User namespaces,** the mechanism.
5. **Rootless `shipyard run`.**
6. **Rootless `shipyard-runtime` and bundles.**
7. **Terminals, CI, docs, and the full review.**
8. **Stage notes and book.**

## Kernel checks

Checked before building on them, with
[`scripts/probes/security.c`](../../scripts/probes/security.c): Linux 7.0, OrbStack,
arm64, in a privileged `rust:1-bookworm` container, as root; the user-namespace checks
drop to `nobody` (65534) themselves. Then under x86-64 emulation. Each check runs in a
forked child, and the probe removes its scratch files.

**Capabilities** (TLPI ch. 39, `capabilities(7)`):

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| C1 | `PR_CAPBSET_DROP` without `CAP_SETPCAP` in the effective set | EPERM; with it, ok | Drop the bounding set while still holding `CAP_SETPCAP`: before the credential switch |
| C2 | root → uid 65534 with `setresuid`, no `PR_SET_KEEPCAPS` | permitted and effective both emptied | A non-root command keeps nothing unless asked |
| C3 | the same with `PR_SET_KEEPCAPS` | permitted kept, effective emptied | Keep-caps, switch, then `capset` the effective set back |
| C4 | uid 65534 keeps `CAP_NET_BIND_SERVICE`, raises it ambient, then `execve` | after exec: effective and ambient both `0x400` | Ambient is how a non-root command gets a capability across `exec` |
| C5 | `CAP_SYS_ADMIN` dropped from the bounding set, then a setuid-root program | euid 0, but `CAP_SYS_ADMIN` not effective (`…dfffff`) | The bounding set limits even setuid-root programs |

**`no_new_privs`:**

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| N1 | uid 65534 runs a setuid-root program, without and with `no_new_privs` | without: euid 0, every capability; with: euid 65534, none | `no_new_privs` disarms setuid programs inside |
| N2 | set, then `fork` and `execve` | still set in the child and after exec | Set once, before the command's exec |
| N3 | unsetting it | EINVAL; still set | It's one-way |

**seccomp** (`seccomp(2)`):

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| S1 | uid 65534 installs a filter without `no_new_privs` | EACCES; with it, ok | Install after `no_new_privs` (or with `CAP_SYS_ADMIN`) |
| S2 | the filter's view of `seccomp_data.arch` | `0x…00b7`: `AUDIT_ARCH_AARCH64` | The architecture check is real: filters are per architecture |
| S3 | the syscall number the filter sees for `getppid` | 173, this build's `__NR_getppid` | Names map to the build's own numbers |
| S4 | after `execve` | the filter still applies (`uname` refused with the filter's errno) | Installed as the command's last step, it binds the command |
| S5 | **under x86-64 emulation (Rosetta)** | **every `seccomp(SET_MODE_FILTER)` is EINVAL, even as root** | No seccomp filter can be installed there: step 3 must decide what an emulated run does without one |

**User namespaces** (`user_namespaces(7)`):

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| U0 | the host's limits; `newuidmap`, `newgidmap` | no `unprivileged_userns_clone` or AppArmor restriction here; `max_user_namespaces` 48184; **the helpers are missing** from `rust:1-bookworm` | The single-ID fallback is testable locally; the `/etc/subuid` path needs the helpers (CI's Ubuntu, or the Lima VM) |
| U1 | uid 65534: `unshare(CLONE_NEWUSER)` | ok | Rootless needs no privilege here |
| U2 | writing its own one-ID `uid_map` | **EACCES while the process is non-dumpable** (a credential change made it so: its `/proc/self` belongs to root); ok once dumpable; a second write EPERM | Nothing before the maps are written may make the process non-dumpable |
| U3 | `gid_map` before and after writing `deny` to `setgroups` | EPERM, then ok | An unprivileged single-ID map must deny `setgroups` first |
| U4 | inside, as the namespace's root | uid 0, every capability (within the namespace) | Stage 2–3's setup runs as before, with these differences: |
| U5 | `proc`, with a PID namespace it owns | ok | `/proc` as today |
| U6 | `sysfs` without its own network namespace | EPERM; a recursive bind of the host's `/sys`, ok | `/sys` becomes a read-only bind of the host's until stage 7 |
| U7 | `mknod` of `/dev/null` | EPERM; binding the host's `/dev/null`, ok and writable | `/dev`'s nodes become binds of the host's |
| U8 | `devpts` (newinstance), `mqueue` (its own IPC namespace), `sethostname`, an IPC sysctl | all ok | As today |
| U9 | a bind of a host mount, then `mount_setattr` adding read-only; remounting the host's `/proc` without its flags | adding: ok; taking a host mount's flags away: EPERM (the kernel locks them) | Restrictions only ever add, so stage 3's `add_restrictions` fits; never try to clear a host mount's flag |
| U10 | `pivot_root` into a tmpfs | ok | The root switch works rootless |

**Root without `CAP_SYS_PTRACE`:**

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| P1 | root opens `/proc/1/fd/0` of a non-dumpable PID 1, with every capability, then without `CAP_SYS_PTRACE` | ok, then EACCES | Once capabilities drop (step 2), the init's non-dumpable flag protects its pipes |

**Under x86-64 emulation** every capability, `no_new_privs` and user-namespace result is
the same, except two (the full outputs diffed):

- seccomp: every filter refused (S5);
- `mount_setattr` is ENOSYS (U9), as stage 3 found for rootful containers, so a
  rootless container there takes stage 3's remount fallback, inside a user namespace
  whose host mounts are locked.

## Step 1: kernel checks and groundwork

- **The probe** above. Two findings are decisions for later steps:
  - **no seccomp under x86-64 emulation** (S5): step 3 decides whether an emulated
    run refuses to start without a filter, or runs unfiltered only when asked;
  - **no `newuidmap` in the test image** (U0): steps 4–5 decide how the `/etc/subuid`
    path is tested.
- **Capabilities** (`security::Capability`, `security::CapabilitySet`):
  - the kernel's 41 names, in kernel order, accepted as `CAP_NET_ADMIN`, `NET_ADMIN`
    or `net_admin`;
  - sets as the kernel's bit masks;
  - Docker's default 14 (`CapabilitySet::DOCKER_DEFAULT`).

  They're cross-platform: names parse on macOS too, where nothing applies them.
- **The syscall table** (`security::syscall_number`, Linux): generated by
  `scripts/gen-syscalls.py`, 328 calls on aarch64 and 381 on x86-64, from two sources:
  - the libc crate's numbers, glibc's and musl's together (where both name a call,
    the numbers must agree);
  - the calls since Linux 5.1 that libc doesn't have yet, at the kernel's unified
    numbers (424 and up: the same on every architecture), from the kernel's
    `scripts/syscall.tbl`, checked against Linux 6.12's headers up to `mseal` (462).
  - A unit test checks the numbers against `libc::SYS_*` wherever libc has the name.
- **A deviation:** the plan put a `SeccompProfile` type in this step. It comes in step 3
  instead, designed together with its compiler.
- **Rules for the steps ahead, from this step's findings:**
  - step 2: drop the bounding set up to the kernel's `/proc/sys/kernel/cap_last_cap`
    (40 here), read at run time, not up to Shipyard's table: a newer kernel's unknown
    capabilities must go too; a name past an older kernel's last capability is refused
    clearly;
  - step 3: a profile can only allow a call the table names; the table must keep up
    with the kernel (regenerate);
  - step 4: nothing may make a process non-dumpable before its maps are written (U2);
    restrictions only add (U9).
- **Tests:**
  - the capability names: every spelling round-trips, the numbers against
    `linux/capability.h`, and Docker's 14;
  - the syscall table on aarch64 and on x86-64 (emulated): the calls containers use
    are there, names and numbers are unique, the numbers agree with libc's, the calls
    libc lacks are there, and the legacy calls (`open`, `stat`, `fork`, `arch_prctl`)
    are x86-64 only.

### Review fixes (step 1)

- **The probe's seccomp filter** lost two instructions that did nothing (`TAX` then
  `TXA`, copying A to X and back), and each instruction is now numbered with where its
  jumps lead: classic BPF jump offsets count the instructions skipped, which nobody
  should have to work out by hand.
- **`show()` returns nothing:** no caller used its status.
- **The probe's output is in order:** the second `uid_map` write (U2) came after the
  `gid_map` checks (U3).
- **One `COUNT` drives the capability table:** `NAMES` had its length written out, and
  `ALL`, `from_number` and `iter` each re-derived it (with a conversion that could never
  fail).

### Worst-case review (step 1)

Checked on Linux. The fixes:

- **The probe left a setuid-root copy of itself in `/tmp`, and anyone could use it.**
  - Its only check was `geteuid() != 0`, which a setuid program passes. So `nobody`,
    running the copy without `show`, got the probe's root-only checks run as root.
    Shown: `C1 … C2 …` printed for `nobody`.
  - An interrupted run left the copy in a fixed, predictable directory. Even a copy
    that does nothing is attack surface: glibc's own setuid handling has had holes
    (CVE-2023-4911).
  - Now:
    - run setuid, the copy only reports;
    - the scratch directory is a `mkdtemp` name, with a tmpfs over it inside a mount
      namespace of the probe's own, so the copy can't outlive the probe: killed with
      SIGKILL at its C5 line, it left only an empty `0700` directory;
    - `nobody` can run the copy only through the `0711` directory, which it can't list.
- **The syscall table missed real calls:**
  - taking only names both glibc and musl define dropped `getrlimit`, `setrlimit`,
    `renameat`, `sync_file_range` and `io_pgetevents` on aarch64;
  - the libc crate has no call since about Linux 6.5: `fchmodat2`, `cachestat`, the
    `futex_*` family, `statmount`, `listmount`, `mseal` and more.

  A filter can only allow what it can name, so programs using them (glibc's `fchmodat`
  uses `fchmodat2`) would have been refused. Now a union of both libcs, plus the kernel's
  unified numbers (above), tested against libc's constants.
- **The notes said every emulated result matched but seccomp:** `mount_setattr` is
  ENOSYS there too (diffing the full outputs, not a sample, showed it).
- One run read the probe half-written: OrbStack's file sharing caught the file mid-edit.
  It compiled to "expected ')' at end of input"; re-running read the whole file.

**Checked, and fine:**

- This kernel's last capability is 40, the table's last.
- The probe's results are the same after the hardening (50 lines, natively).


## Step 2: capabilities and `no_new_privs`

**Decided with the user:**

- `no_new_privs` is **on by default** (`--security-opt no-new-privileges=false` turns
  it off). Docker's default is off.
- **A non-root user's `--cap-add` capabilities are raised ambient** (Podman's rule).
- **A bundle without `process.capabilities` gets none,** with a warning (runc leaves
  root's full set).
- **Only isolated runs are confined;** with `--no-isolation`, the flags are errors.

**The model** (`security/confinement.rs`):

- `CapabilitySets`: bounding, effective, permitted, inheritable, ambient.
- `Capabilities::Exact(sets)` (a bundle's) or `Default { kept, added }` (the CLI's:
  Docker's 14 after `--cap-drop`, and `--cap-add`). `resolve(uid)` gives the sets once
  the user is known, in the clean image after names resolve:
  - root gets bounding, effective and permitted `kept ∪ added`, nothing inheritable
    (Docker, since CVE-2022-24769);
  - a non-root user gets bounding `kept ∪ added`, and effective, permitted,
    inheritable and ambient `added`.
- `Security { capabilities, no_new_privileges }` in `Isolation.security`, carried in
  the setup memfd. `Placement::New` boxes its `Isolation`, which grew past clippy's
  enum size limit.

**Applying it** (`child.rs`, after-fork rules: everything precomputed in
`Confinement`, raw `prctl` and `capset` system calls). The order is step 1's probe's:

1. `PR_SET_KEEPCAPS` for a non-root user (C3);
2. drop from the bounding set every capability up to the kernel's `cap_last_cap`
   (read in the clean image) that isn't kept, so a newer kernel's unknown capabilities
   go too; this needs `CAP_SETPCAP`, so it's before the switch (C1);
3. the credential switch;
4. `capset` (version 3, two 32-bit halves);
5. raise ambient (C4);
6. `no_new_privs`.

A capability the kernel lacks (past its `cap_last_cap`) is refused with its name. A
kernel with more than 64 capabilities is refused (the sets are 64-bit). New
`ChildStep`s 38–45 (`KeepCaps`, `BoundingDrop`, `Capset`, `AmbientRaise`,
`NoNewPrivs`, `CapLastCap`, `CapabilityUnknown`, `TooManyCapabilities`) name the
capability involved.

**The init** keeps only `CAP_KILL` once its command is forked. It needs that to pass
signals to a command running as another user: Linux lets a process signal another
only if their user IDs match, or it has `CAP_KILL` (TLPI §20.5). It also sets
`no_new_privs`. An init that can't confine itself kills the command, says so on the
container's stderr, and exits 125: it must not stay with every capability. Without
`CAP_SYS_PTRACE`, its non-dumpable flag now keeps root inside out (probe P1).

**The kernel's rule for root, found by the tests.** On `execve`, a process with UID 0
gets its whole bounding set back as permitted and effective: the program's file
capabilities count as all (`capabilities(7)`), unless `no_new_privs` forbids gaining
anything. So a bundle asking root for a smaller effective set than its bounding set
gets it only with `noNewPrivileges`. Docker never shows this, since root's sets are
all the same there; runc behaves as Shipyard does, the rule being the kernel's. It's
tested both ways, and documented in `bundles.md`.

**Front ends:**

- **CLI:**
  - `--cap-add`/`--cap-drop` (repeatable, `ALL`, a name in both is an error);
  - `--security-opt no-new-privileges[=|:]true|false` (other options are refused
    until step 3);
  - all conflict with `--no-isolation` and `--bundle`, and are errors on macOS;
  - the `--no-isolation` hint needs the standard security too.
- **OCI:** `process.capabilities` (exact; an unknown name is an error; absent means
  none, with a warning) and `process.noNewPrivileges` (absent means false) are applied,
  and no longer refused. The test fixture `Bundle` writes what `runc spec` writes
  (`CAP_AUDIT_WRITE`, `CAP_KILL`, `CAP_NET_BIND_SERVICE`, `noNewPrivileges`).

**Joined commands** (`setns`, internal for now) keep the joiner's capabilities:
carried to `exec` (stage 8).

**Tests:**

- **Unit:**
  - `resolve` for root, non-root and exact sets;
  - the bounding drop for kernels whose last capability is 37, 40 and 45, and one
    past 63;
  - a capability the kernel lacks;
  - the `capset` words (with a capability above 31);
  - the init's confinement;
  - the CLI's rule and its errors;
  - OCI exact sets, an unknown name, absent sets with the warning, and
    `noNewPrivileges`.
- **`crates/shipyard-cli/tests/security.rs`,** reading `/proc/self/status` from the
  command: 9 root tests and 1 as any user:
  - Docker's 14 and `NoNewPrivs: 1` by default;
  - `--cap-drop ALL`, `--cap-add SYS_ADMIN --cap-drop CHOWN`;
  - a non-root user with and without `--cap-add` (ambient);
  - a setuid-root `id` run by user 1000: 1000 by default, 0 without
    `no_new_privs`;
  - the init's `CAP_KILL` only, and root inside unable to follow `/proc/1/fd/0`
    without `SYS_PTRACE`;
  - the init forwarding TERM to a user-1000 command;
  - a bundle's exact sets both ways (the root rule above), and absent sets.
- **`crates/shipyard-cli/tests/runtime.rs`:** a `shipyard-runtime` container gets its
  bundle's sets and `noNewPrivileges` too, through the monitor and the start gate.
- **Existing tests, updated:**
  - the tests that look into the init (`/proc/1/fd`, `environ`, `cwd`) now ask for
    `CAP_SYS_PTRACE` (`inspecting_the_init`), and mounting inside needs
    `--cap-add SYS_ADMIN`;
  - **the status-pipe forgery tests could pass without forging anything** once
    `/proc/1/fd` was out of reach: the shell's glob found nothing to write to, and
    their only check was the exit code. Their pipe loop now prints
    `reached-the-pipe`, and each test requires it.
- **Mutation-checked** (scratch copies, each caught): no bounding drop; no
  `no_new_privs`; no ambient raise; the init keeping every capability; the init
  dropping `CAP_KILL` (the user-1000 command no longer gets TERM); root given a
  non-root user's sets; the forgery tests without `SYS_PTRACE` (their new
  `reached-the-pipe` check fails). The `/proc/1/fd` refusal doesn't depend on the init
  dropping its own capabilities: it's the caller's missing `CAP_SYS_PTRACE` and the
  init's non-dumpable flag.
- **Runs:** `scripts/test-linux.sh` under `/bin/bash`; under x86-64 emulation, security
  10, isolation 32, bundle 19, runtime 24 (capabilities behave the same there, as
  step 1 found).

### Review fixes (step 2)

- **One `prctl` wrapper** in `child.rs`: the same raw call, with its own `unsafe` block
  and SAFETY comment, had been written out four times. The `capset` header's magic
  number is a named constant (`LINUX_CAPABILITY_VERSION_3`).
- **No boolean arguments in a row:** `Confinement::from_sets` took
  `(sets, keep_caps, no_new_privileges, last_cap)`, so the init's call read
  `from_sets(sets, false, true, 40)`. It builds the sets only now, and each caller
  names the two fields.
- **One step, one meaning:** `CapLastCap` stood both for "couldn't read
  `cap_last_cap`" and, with ERANGE, for "more than 64 capabilities".
  `TooManyCapabilities` (45) is the second.
- `PR_CAP_AMBIENT_RAISE` is converted exactly (`unsigned_abs`), not with a fallback
  that would have passed 0.


### Worst-case review (step 2)

Checked on Linux with bundles made to break. The fixes:

- **A bundle that masked `/proc/sys` couldn't run:** the clean image read
  `/proc/sys/kernel/cap_last_cap` after the masks ("reading the kernel's last
  capability…: No such file or directory"). It's read first now, before any mount,
  while `/proc` is the host's. Tested (`a_bundle_may_mask_proc_sys`).
- **Capability sets the kernel can't apply failed only at start, with a bare
  EPERM:** an ambient capability not also permitted and inheritable, an effective one
  not permitted, an inheritable one outside the bounding set (all three reproduced).
  `CapabilitySets::check` applies `capset(2)`'s rules when the bundle is read, and
  names the rule and the capability (exit 125). Tested.
- **`--cap-add ALL` would have failed on kernels before 5.9:** `ALL` is Shipyard's 41
  capabilities, and an older kernel lacks the newest three. With the CLI's rule, `ALL`
  now means all the kernel has; a capability named explicitly that it lacks is still
  refused. Unit-tested with a kernel whose last capability is 37.
- **`no_new_privs` disarms file capabilities too,** not only setuid: Debian's `ping`
  (`cap_net_raw+ep`) fails for a non-root user by default. The user's decision; now
  documented with the way out (`--cap-add NET_RAW`, kept as ambient), and tested
  with a copy of `cat` given `cap_dac_override+ep` (the `security.capability`
  attribute written directly, `vfs_cap_data` revision 2): denied by default, read
  without `no_new_privs`, and read by plain `cat` with `--cap-add DAC_OVERRIDE`.

- **Under x86-64 emulation, a masked `/proc/sys` stops Rosetta itself,** which reads
  `/proc/sys/vm/mmap_min_addr` to start each x86 program in the container (exit 133,
  its own assertion). The emulator's limit, not Shipyard's: the test skips there. The
  Rosetta check, now in three tests, moved to the test-support crate
  (`under_rosetta`).

**Checked, and fine:**

- Ctrl-C and other forwarded signals reach a non-root command: the init's `CAP_KILL`.
- The keep-caps flag is cleared by `execve` itself.
- A capability past Shipyard's table on a newer kernel is dropped from the bounding
  set, so `ALL` never includes it.
- The command and the init confine themselves independently: the init drops its own
  power after the fork, and the command can't reach it.

## Step 3: seccomp

**Decided with the user:**

- **Under x86-64 emulation, refuse with a hint.** The kernel refuses every filter
  there (probe S5): the run fails (exit 125), names emulation as the cause, and points
  to `--security-opt seccomp=unconfined`. Shipyard never drops the filter on its own.
- **A call name another architecture has is skipped; one no architecture has is an
  error.** One profile serves both architectures (as with Docker), and a typo is
  caught.
- **Calls Docker's profile doesn't name get EPERM, as it says.** No ENOSYS stub for
  newer calls (runc has one); the profile's own `clone3` → ENOSYS rule stays.
- **Foreign architectures are killed:** 32-bit programs (and x32 on x86-64) don't run
  under a filter; `seccomp=unconfined` runs them.

**Decided while building it:**

- **The CLI applies Docker's default; a bundle without `linux.seccomp` is unfiltered**
  (the OCI spec, runc).
- **Of the rules that match a call, the most restrictive wins** (kill, trap, errno,
  log, allow: the kernel's own precedence between filters), whatever order they're
  written in. libseccomp's semantics for overlapping rules are harder to state; Docker's
  profile has no overlaps, so it behaves the same either way.
- **The init isn't filtered.** It's Shipyard's own code, already down to `CAP_KILL`,
  non-dumpable and under `no_new_privs`; a user's profile could break its supervision
  (`wait`, `kill`, the status pipe) without making anything safer.
- **A `--security-opt` profile file is strict; a bundle's follows the spec.** Unknown
  fields in a file are errors (a field Shipyard doesn't know might be a restriction).
  In `linux.seccomp` they're ignored, as the OCI spec requires of unknown fields, with
  a warning naming each one (`Profile::from_oci`).
- **`syscall(-1)` on x86-64 isn't treated as x32:** a tracer sets the number to -1 to
  skip a call, and libseccomp lets it reach the default action too.
- **Joined processes** (`setns`, internal) stay unfiltered until `exec` (stage 8).

**The syscall tables** (`scripts/gen-syscalls.py`):

- Both architectures are generated for every build (`AARCH64`, `X86_64`), so the
  compiler and its tests run on macOS too; `Arch::number(name)` replaces step 1's
  `syscall_number`.
- A names-only list of every call of any Linux architecture the libc crate knows,
  plus 32-bit Arm's and RISC-V's private calls (`set_tls`, `riscv_flush_icache`),
  answers "is this a real name?" (561 names).
- **Checked entry by entry against Linux 7.3's own tables:** aarch64 matched; x86-64
  lacked five calls libc doesn't have (`uretprobe` 335, `uprobe` 336, and three
  long-removed ones), now added as `EXTRA`. Docker's profile names `uretprobe`.

**The profile** (`security/seccomp/profile.rs`): Docker's JSON format, which is OCI's
`linux.seccomp` plus `includes`/`excludes` (capabilities, architectures, kernel
version), `archMap`, `comment` and the older `name`. `null` lists are read as empty
(Go writes them so: Docker's own profile has `"subArchitectures": null`).
`validate()` checks names, actions (`TRACE` and `NOTIFY` refused: no tracer, no
listener), argument indexes (0–5), errnos (0–4095), flags (`LOG`, `SPEC_ALLOW`;
`TSYNC` is a no-op, since the command is single-threaded), capabilities and
`minKernel`. Errors name the rule (`syscalls[3]: …`).

**Docker's profile is vendored** (`docker-default.json`, from `moby/profiles` v0.2.3
as Docker 29.8 vendors it; Apache-2.0) and parsed at run time.

**The compiler** (`security/seccomp/compile.rs`, platform-neutral, pure):

1. **Resolve** Docker's conditions as Docker does: `includes` needs every capability
   in the container's bounding set, the architecture among its arches (`arm64`,
   `amd64`) and the kernel at least `minKernel`; any one `excludes` drops the rule.
2. **The architecture first** (`seccomp_data.arch` must be native, else kill the
   process), and on x86-64 the x32 bit (except -1).
3. **A binary search over the call numbers** (`jge`, then up to four `jeq`): about a
   dozen comparisons per call, where a linear chain would be up to 300.
4. **Each call's rules**, most restrictive first. A call whose first rule has no
   argument conditions jumps straight to a shared `ret`.
5. **64-bit argument comparisons on a 32-bit machine:** the high half first, then the
   low half (`EQ`, `NE`, `LT`, `LE`, `GT`, `GE`, `MASKED_EQ`).

**The assembler** (`security/seccomp/bpf.rs`) writes with labels and resolves them
from the end: jumps only go forward, so when a jump is placed, everything it can reach
is already placed. A conditional jump further than 255 instructions (its 8-bit
offsets) goes through a `ja` (32-bit offset). Above `BPF_MAXINSNS` (4096) the profile
is refused. Docker's profile compiles to 544 instructions on aarch64 (268 calls) and
714 on x86-64 (310), 683 and 799 with every capability.

**Applying it** (`child.rs`, after-fork rules: a raw `seccomp(SECCOMP_SET_MODE_FILTER)`
on a program compiled in the clean image, with the kernel version from `uname`):

- **with `no_new_privs`:** last, just before the `exec` loop, so only `exec` (and the
  failure report's `write`) runs under it;
- **without:** first, before the bounding drop, while root still has `CAP_SYS_ADMIN`
  (S1), as runc does. The profile must then allow the setup's own calls (`prctl`,
  `capset`, `setresuid`, `setgroups`); Docker's does.
- New `ChildStep`s 46 `Seccomp` (EINVAL explained: emulation, or a kernel before 4.14
  refusing `LOG`/`KILL_PROCESS`) and 47 `SeccompCompile` (too long, with the length).

**Front ends:**

- **CLI:** `--security-opt seccomp=FILE|unconfined` (the last one wins, as with
  Docker); the file is read and validated on the host, errors naming it; conflicts
  with `--no-isolation` and `--bundle`.
- **OCI:** `linux.seccomp` is parsed (`from_oci`) and validated, errors prefixed
  `linux.seccomp:`, and no longer refused.

**Tests:**

- **Unit (on macOS too):**
  - a classic-BPF interpreter runs the compiled programs on synthetic `seccomp_data`;
  - **a property test** (2000 cases): for random profiles (every operator and action,
    conditions, overlapping rules), architectures, foreign architectures, x32 and -1,
    the interpreter's verdict equals a reference evaluator's, which applies the
    profile's meaning directly;
  - Docker's profile on both architectures: what programs need is allowed, the
    kernel's own state is EPERM, `clone` with `CLONE_NEWUSER` is EPERM without
    `SYS_ADMIN` and allowed with it, `clone3` is ENOSYS, `personality(0xffffffff)` is
    allowed but not with a different high half, foreign architectures and x32 are
    killed, and the size;
  - the assembler: offsets, far targets either way, every gap from 250 to 261, too long;
  - validation errors, null lists, `from_oci`'s dropped fields, and that its field
    lists are exactly the types' fields;
  - the CLI's options and file errors; OCI present, absent, unknown fields, errors.
- **Root (`security.rs`, 17 tests now; `runtime.rs`):** the default filter
  (`Seccomp: 2`, `unshare -U` refused, allowed with `SYS_ADMIN`); `unconfined`; a
  profile of one's own (an errno, and a kill: exit 159, SIGSYS); without
  `no_new_privs`, as root and as user 1000 (the early install); a bundle's filter
  (`chdir` refused) and none without one; the runtime's container; under emulation,
  the refusal and its hint.
- **Under emulation** the test support adds `--security-opt seccomp=unconfined` to
  isolated runs (`for_this_kernel`, right after `run`, after global options like `-D`),
  and the filter's own tests skip.
- **Mutation-checked** (scratch copies, each caught): no architecture check; x32
  allowed; the arguments' high half ignored; no precedence; `includes.caps` ignored
  (by the condition tests: the property test shares `applies` with the compiler, so it
  can't see this one); the assembler's trampoline off by one; no install; always
  installed last (fails every run without `no_new_privs`).
- **A trap in mutation testing, found here:** two mutants built into one target
  directory share the uplifted `target/debug/shipyard` that `CARGO_BIN_EXE_shipyard`
  points at, so the second mutant's tests ran the first one's binary (identical
  failures gave it away). Each mutant gets its own target directory now.
- **Runs:** `scripts/test-linux.sh` under `/bin/bash` (real filters, aarch64); the
  whole workspace under x86-64 emulation (security 17, isolation 32, bundle 19,
  runtime 26, terminal 31, containers 17), where the first run showed two gaps in
  the test support, both fixed: terminal tests start shipyard as `-D run …` (the
  injection now goes after `run`, wherever it is), and the runtime's own
  `containers.rs` builds `Isolation::standard()` through the library (it now leaves
  the filter out under Rosetta).

### Review fixes (step 3)

- **The filter's placement was a rule written twice** in `child.rs` (an inline
  `no_new_privs` test before the switch, its opposite before `exec`).
  `Confinement::filter_before_setup` and `filter_before_exec` now state it once, next
  to the data, and a unit test checks that a filter goes on exactly once.
- **`Confinement::new` compiled the filter inline**, in a four-way match that wrapped
  each error by hand. `SeccompFilter::compile` does it, and `?` converts the error
  (`From<SeccompError>`).
- **`compile` did four things in one function.** Its parts are named now:
  `check_architecture`, `search`, `check_rules`, and `Returns` (the shared verdicts,
  which a closure capturing a map used to collect).
- **`applies` built `Vec<bool>`s only to ask `contains`.** Docker's two rules are
  now two functions that say them: `any_holds` (an `excludes`) and `all_hold` (an
  `includes`).
- **Smaller ones:**
  - `SeccompError::UnsupportedAction` has named fields, not two anonymous strings;
  - `profile.rs` groups its constants, and its helpers come after the types;
  - the assembler's instruction builder is a `const fn`, not a closure made on every
    loop turn;
  - `Seccomp::profile` borrows a profile instead of cloning it, and the OCI reader
    takes `linux.seccomp` instead of cloning it;
  - the CLI's `SecurityOpt` variants are named alike (`SeccompUnconfined`,
    `SeccompProfile`);
  - the test helpers sit together at the top.
- **The failure messages:** `describe` sat at clippy's 100-line limit, and
  `describe_seccomp` chose between two steps inside itself. Like `describe_container`,
  `describe_confinement` now has the capability and seccomp steps (38–47), each its
  own arm; a unit test checks the messages.

### Worst-case review (step 3)

Checked on Linux with hostile profiles and real programs. The fixes:

- **`--security-opt seccomp=/dev/zero` hung, its memory growing; a FIFO hung too**
  (`open` waits for a writer). Stage 3 had solved this for `config.json`: open
  non-blocking, check the opened file is regular, read with a limit. That code is now
  shared (`small_file.rs`), and `Profile::from_file` uses it with a 1 MiB limit
  (Docker's profile is 13 KiB). Unit-tested with a FIFO, `/dev/zero`, a directory and
  a file one byte too large.
- **A refusal by the filter was blamed on the step.** Without `no_new_privs`, a
  profile refusing `prctl` failed with "dropping CAP_DAC_READ_SEARCH from the
  capability bounding set: Operation not permitted", and a refused `execve` with a
  bare "cannot execute". The failure context now knows when the filter went on
  (`FilterStart`, on both Shipyard's side and the init's), and an EPERM from a step
  after it adds "(the seccomp profile, in force by then, may have refused it)".
  Unit-tested both ways (before the filter, without one, another errno: nothing added).
- **Documented** (seccomp's own semantics, the same as Docker's and libseccomp's):
  - arguments are compared as 64-bit numbers, so a negative `int` arrives
    sign-extended (`AT_FDCWD` is `0xffffffffffffff9c`; a rule on `0xffffff9c` never
    matches, checked);
  - `"errnoRet": 0` makes a call look successful without running it (an `rm` that
    leaves the file, checked);
  - a profile refusing both `execve` and `write` leaves the failure unreported:
    exit 125, no message (checked; the report itself is a `write`).

**Checked, and fine:**

- Refused `exit`/`exit_group`: glibc's `_exit` falls back to a trap instruction
  (exit 133), so nothing hangs.
- Kill by default, allowing only `execve`: exit 159 (SIGSYS); `SCMP_ACT_KILL` the same
  for a single-threaded program.
- A 9309-instruction profile: refused with its length (exit 125).
- An empty rule list (only the architecture check), `SCMP_ACT_LOG`,
  `SECCOMP_FILTER_FLAG_LOG` and `SPEC_ALLOW`.
- A non-root user with an ambient capability, without `no_new_privs` (the early
  install runs the switch of user under the filter).
- A bundle without a PID namespace (the clean image becomes the command): filtered,
  its rules applied.
- Real programs under Docker's profile: Python (threads, `subprocess`, sockets), Perl,
  git, curl, tar, find, ps, dd, getent. `io_uring_setup` and `userfaultfd` get EPERM,
  as Docker intends.
- **The cost:** about 20 ns per system call (2 million one-byte `read`s: 0.43 s
  unfiltered, 0.47 s filtered), thanks to the binary search.
- **Persisted state:** `state.json` holds no `Security`, so the new field changes
  nothing for containers created by an older build (the setup memfd is read by the
  same binary that wrote it).
- **Under x86-64 emulation,** the whole workspace passes after the clean-code
  refactors.

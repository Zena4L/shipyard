# Stage 4 notes: users and security

The engineering record of stage 4: what was built, why it's built that way, what the kernel
was checked to do, what can go wrong and how that's handled, and what was left for later
stages. For how to *use* it, see [Capabilities and privileges](../user/run.md#capabilities-and-privileges),
[System calls (seccomp)](../user/run.md#system-calls-seccomp),
[User namespaces](../user/run.md#user-namespaces), [Rootless](../user/run.md#rootless),
[Rootless bundles](../user/bundles.md#rootless-bundles), [The OCI runtime](../user/runtime.md)
and [Exit codes](../user/exit-codes.md).

## Outcome

Stage 4 asked for rootless containers (user namespaces and ID maps), capabilities,
`no_new_privs` and seccomp ([ROADMAP](../ROADMAP.md),
[FEATURES §4](../FEATURES.md#4-users-and-security--stage-4)). On Linux, every isolated
container is now confined: a minimal set of capabilities, `no_new_privs`, Docker's seccomp
profile compiled by Shipyard's own BPF compiler, and a session keyring of its own. And
anyone can run one: without root, `shipyard run`, `shipyard run --bundle` and
`shipyard-runtime` make the container **rootless**, in a user namespace whose root is the
caller.

| Requirement | Where | Tested by |
|---|---|---|
| Capabilities: Docker's 14, `--cap-add`/`--cap-drop`, a bundle's five sets exactly | `security/capability.rs`, `security/confinement.rs` (`Capabilities`, `Confinement`), `process/child.rs` (`confine_before_switch`, `confine_after_switch`), `oci.rs` | `security.rs` (the default set, adding and dropping, a non-root user's ambient set, a bundle's sets both ways, a bundle without sets); `runtime.rs`; unit tests |
| `no_new_privs`, on by default: setuid programs and file capabilities give nothing | `confinement.rs`, `child.rs` | `no_new_privs_disarms_setuid_programs`, `no_new_privs_disarms_file_capabilities_too` |
| seccomp: Docker's profile compiled to BPF in Rust; profiles of one's own; `unconfined` | `security/seccomp/` (`profile.rs`, `compile.rs`, `bpf.rs`), `security/syscalls.rs`, `child.rs` (`install_filter`) | the property test and the Docker-profile unit tests; `the_default_filter_is_dockers`, `a_profile_of_ones_own`, `without_no_new_privs_the_filter_goes_on_first`, `a_bundles_seccomp_is_its_own`, `under_emulation_a_filter_is_refused_with_a_hint` |
| User namespaces: `--uidmap`/`--gidmap`, a bundle's `user` namespace and mappings | `id_map.rs`, `process/user_namespace.rs` (`write_maps`), `spawn.rs`, `child.rs` (`become_namespace_root`), `rootfs.rs` (`bind_host_device`, `bind_host_sys`), `init.rs` (`check_mapped`) | `userns.rs` (16 tests); `id_map.rs` unit tests |
| Rootless `shipyard run`, automatic: the full map through `newuidmap`, else the caller's own ID | `user_namespace.rs` (`choose_rootless_maps`, `MapWriter`, `run_map_helper`), `id_map.rs` (`subordinate_ranges`, `IdMaps::rootless`), the CLI's `run.rs` | `rootless.rs` (15 tests, as a normal user) |
| Rootless bundles and runtime; state in `$XDG_RUNTIME_DIR/shipyard` | `oci.rs` (`load_bundle_for_caller`, `add_user_namespace`), `container/mod.rs` (`choose_root`), `container/lifecycle.rs` | `rootless.rs`'s bundle tests; `a_rootless_container_lives_its_life`, `rootless_state_is_in_the_runtime_dir` |
| The init confined (`CAP_KILL`, `no_new_privs`, the container's filter) before its command goes on | `init.rs` (`start_command`, `confine_self`, `give_up`), `spawn.rs` (`init_outcome`) | `the_init_keeps_only_cap_kill`, `the_init_runs_under_the_containers_filter`, `a_profile_refusing_the_inits_wait_ends_the_container`, `a_profile_killing_the_init_says_so`, `a_profile_refusing_the_inits_kill_ends_the_container` |
| A session keyring per container | `init.rs` (`join_session_keyring`) | `a_container_has_its_own_session_keyring`, `a_rootless_container_has_its_own_session_keyring` |
| Terminals, rootless | (no new code) | `terminal.rs`'s 15 `ISOLATED_TESTS`, run as a normal user |

**Moved out of stage 4**, each to the stage that gives it meaning:

| Item | Now in | Why |
|---|---|---|
| Device access control | stage 5 | an eBPF program attached to the container's cgroup, which exists from stage 5 |
| Joining a user namespace; capabilities and filters for a joined process | stage 8 (`exec`) | who joins first, and the credentials after, need their own design |
| Idmapped mounts for root filesystems | stage 6 | the image store is where root filesystems get their owners |
| A `sysfs` of the container's own | stage 7 | a user namespace can mount one only with its own network namespace |
| AppArmor, SELinux, Landlock | later (FEATURES) | not needed for Docker parity in the stages ahead |

Paths below are relative to `crates/shipyard-runtime/src/` and `crates/shipyard-cli/`.

## Architecture

```
shipyard run (as root: rootful; as anyone else: rootless) / run --bundle / shipyard-runtime create
  run.rs: Security { capabilities, no_new_privileges, seccomp } (Docker's 14, on, Docker's
          profile); a user namespace: --uidmap for root, rootless_id_maps() for anyone else
          (the full map through the helpers, else the caller's own ID and a warning)
  oci.rs: load_bundle_for_caller: the bundle's own, or one added for a rootless caller
  └─ spawn()                                                         spawn.rs
       ├─ with maps: /proc must be our own (proc_is_ours), else ForeignProc
       ├─ clone(NEWUSER|NEWPID|NEWNS|…) → the child waits for "go"
       ├─ write_maps (user_namespace.rs), while the child is still dumpable:
       │    Root → uid_map, gid_map; OwnIds → setgroups "deny", then the maps;
       │    Helpers → newuidmap, newgidmap (10 s each, stderr drained)
       │    on failure: SIGKILL, reap, the error
       └─ "go"

the cloned child (after-fork rules; child.rs)
  setgroups_allowed_here (/proc/self/setgroups) → become_namespace_root (setgroups(0) if
  allowed, setresgid(0), setresuid(0)) → parent-death signal → re-exec

the clean image (init.rs: ordinary Rust, non-dumpable)
  cap_last_cap and setgroups_allowed (the host's /proc) → set_up (rootfs: /dev nodes bound
  in a user namespace, /sys a read-only rbind with tmpfs covers, devpts gid=5 if mapped)
  → join_session_keyring → names (the container's /etc) → check_mapped → groups refused
  where setgroups is denied → Confinement::new (the sets, the bounding drop list, capset's
  words, ambient, the filter compiled for this arch, bounding set and kernel)
  → the command's plan → the go pipe → [gate]
  → fork the command (PID 2): it waits on the go pipe
  → confine_self: the bounding set to CAP_KILL, capset, no_new_privs, the container's
    filter (refused: the command is killed, the failure reported as its own)
  → "go" → become_init (a refused wait4, rt_sigtimedwait or kill: the "gave up" mark,
    exit 125; killed by its filter: shipyard reports that)

the command (command_main; after-fork rules)
  [without no_new_privs: the filter first, while CAP_SYS_ADMIN is held]
  → PR_SET_KEEPCAPS (non-root) → PR_CAPBSET_DROP each → setgroups (if allowed), setresgid,
    setresuid, verify → capset → ambient raise → PR_SET_NO_NEW_PRIVS
  → [with no_new_privs: the filter last] → execve
```

## Decisions and why

**Made with the user:**

- **Rootless is automatic.** A non-root caller of an isolated run gets a user namespace,
  as with Podman; root keeps rootful containers, now confined. `--no-isolation` opts
  out. Root inside is the default user for every isolated run (Docker's), so a rootless
  container's root is its caller.
- **The full map, else the caller's own ID** (Podman's model). With `newuidmap`,
  `newgidmap` and a range in `/etc/subuid` and `/etc/subgid`, container 0 is the caller
  and IDs from 1 up are its ranges, in order. Without them, only container 0 is mapped,
  `setgroups` is denied, and a warning says what's missing.
- **seccomp is compiled by Shipyard's own code, in Rust.** Profiles (Docker's, OCI's
  `linux.seccomp`, files) become classic BPF with no C library; the compiler and its
  property test run on macOS too.
- **`no_new_privs` is on by default** (Docker's is off). `--security-opt
  no-new-privileges=false` turns it off.
- **A non-root user's `--cap-add` capabilities are raised ambient** (Podman's rule), so
  they last across `exec`. Docker gives such a user nothing.
- **A bundle without `process.capabilities` gets none,** with a warning (runc leaves
  root's full set).
- **Only isolated runs are confined.** With `--no-isolation` the security flags are
  errors.
- **Under x86-64 emulation, refuse with a hint.** The kernel refuses every filter there
  (probe S5). The run fails and points to `--security-opt seccomp=unconfined`; Shipyard
  never drops a filter on its own.
- **A call name another architecture has is skipped; one no architecture has is an
  error.** One profile serves both architectures, and a typo is caught.
- **Calls a profile doesn't name get its default action** (EPERM in Docker's profile),
  as it says. No ENOSYS stub for newer calls (runc has one).
- **Foreign architectures are killed:** 32-bit programs, and x32 on x86-64.
- **`--uidmap CONTAINER:HOST:SIZE` and `--gidmap`** (Podman's flags) are root's; without
  root the maps are chosen for you.
- **A rootless bundle without a user namespace gets one,** with a warning (runc refuses
  it). **A bundle's own mappings are allowed** for a non-root caller: its own single ID
  written directly, anything else through the helpers, which grant only its ranges.
- **A non-root `shipyard-runtime` keeps its state in `$XDG_RUNTIME_DIR/shipyard`**
  (runc's and Podman's choice); without it, `--root` must be given.
- **Rootless tests skip where rootless can't work, except in CI**, where
  `SHIPYARD_TEST_ROOTLESS=required` turns a skip into a failure.
- **The init runs under the container's own filter** (step 7, reversing step 3), as
  Docker's init does: whatever reaches PID 1 from inside meets the same rules.
- **A session keyring per container** (step 8), carried from stage 3.

**Engineering:**

- **The model** (`security/`, cross-platform):
  - `Capability` and `CapabilitySet`: the kernel's 41 names, in kernel order, as bit
    masks; `CAP_NET_ADMIN`, `NET_ADMIN` and `net_admin` all parse;
  - `Security { capabilities, no_new_privileges, seccomp }` in `Isolation.security`;
  - `Capabilities::Exact(sets)` (a bundle's) or `Default { kept, added }` (the CLI's).
    `resolve(uid)` gives the five sets once the user is known: root gets bounding,
    effective and permitted `kept ∪ added` and nothing inheritable (Docker, since
    CVE-2022-24769); anyone else gets bounding `kept ∪ added`, and `added` as
    effective, permitted, inheritable and ambient.
- **`Confinement` is everything the command's last steps need, worked out before the
  fork:** the bounding drop list (every capability up to the kernel's
  `/proc/sys/kernel/cap_last_cap` that isn't kept, read at run time, so a newer
  kernel's unknown ones go too), `capset`'s six words, the ambient list,
  `no_new_privs`, and the compiled filter. The after-fork code only reads it.
- **The order is the probe's** (C1–C5, N1, S1): keep-caps for a non-root user, the
  bounding drop while `CAP_SETPCAP` is still held, the switch of user, `capset`,
  ambient, `no_new_privs`, then the filter. Without `no_new_privs` the filter goes on
  first, while `CAP_SYS_ADMIN` is still held (as runc does), and the profile must allow
  the setup's own calls.
- **`--cap-add ALL` means all the kernel has;** a capability named that it lacks is
  refused with its name. A kernel with more than 64 is refused (the sets are 64-bit).
  `CapabilitySets::check` applies `capset(2)`'s rules when a bundle is read, so a set
  the kernel would refuse is named, not a bare EPERM at start.
- **The syscall tables** (`scripts/gen-syscalls.py` → `security/syscalls.rs`): both
  architectures in every build, from the libc crate's numbers (glibc's and musl's
  together, agreeing where both name a call), the kernel's unified numbers for calls
  since Linux 5.1, and x86-64 calls libc lacks (`EXTRA`, such as `uretprobe`), checked
  entry by entry against Linux 7.3's tables: 328 calls on aarch64, 386 on x86-64. A list
  of every name any architecture has (573) tells a typo from a foreign call.
- **The profile** (`profile.rs`): Docker's JSON, which is OCI's `linux.seccomp` plus
  `includes`/`excludes` (capabilities, architectures, `minKernel`), `archMap` and
  `comment`. `null` lists read as empty. `validate()` checks names, actions (`TRACE`
  and `NOTIFY` refused), argument indexes, errnos, flags, capabilities and kernel
  versions, naming the rule (`syscalls[3]: …`). **A profile file is strict** (an
  unknown field is an error: it might be a restriction); **a bundle's follows the
  spec** (unknown fields ignored, each with a warning). Files are read like
  `config.json`: non-blocking, regular files only, at most 1 MiB (`small_file.rs`,
  shared).
- **Docker's profile is vendored** (`docker-default.json`, from `moby/profiles` v0.2.3,
  Apache-2.0) and parsed at run time. One change of Shipyard's (stage 7.5, F8): the
  `socket` rule allowing domains `> 40` also requires the argument's high half to be 0
  (the kernel reads the `int` domain on 32 bits, so `0x1_0000_0028` would otherwise get
  `AF_VSOCK` past it). A test checks every allowing `>`, `>=` and `!=` in the profile has
  that guard.
- **The compiler** (`compile.rs`, pure): Docker's conditions resolved as Docker does;
  the architecture checked first (native, else kill; on x86-64, the x32 bit, except a
  tracer's `-1`); a binary search over call numbers (about a dozen comparisons, where a
  chain would be up to 300); each call's rules, **the most restrictive first** (kill,
  trap, errno, log, allow: the kernel's own precedence between filters); 64-bit argument
  comparisons as the high half, then the low. Docker's profile is 544 instructions on
  aarch64 and 714 on x86-64.
- **The assembler** (`bpf.rs`) writes with labels and resolves them from the end: jumps
  only go forward, so everything a jump can reach is placed when the jump is. A
  conditional jump further than 255 goes through a `ja`; above 4096 instructions the
  profile is refused with its length.
- **The maps** (`id_map.rs`): `IdMap { container, host, size }`, `IdMaps { uids, gids }`,
  validated as the kernel would (1 to 340 ranges, none empty or past the last ID, no
  overlap inside or outside, container 0 mapped for users and groups). The parent
  writes them right after the clone, one `write` each, while the child is still
  dumpable (U2), before the go byte; a failure kills and reaps the child there.
- **The child becomes the namespace's root before the re-exec** (`setresuid(0)` and
  friends, raw): `execve` recomputes capabilities from the effective uid, and host
  root's uid is the overflow uid inside, so the clean image would start with none. And
  before the parent-death signal, which a credential change clears.
- **Inside a user namespace** (U6, U7, U9): `/dev`'s nodes are empty files with the
  host's nodes bound onto them; `/sys` is a recursive bind of the host's, made
  read-only with stage 3's `add_restrictions`, and each outermost mount the host has
  inside it is covered with an empty read-only tmpfs; `devpts` gets `gid=5` only if
  group 5 is mapped.
- **`setgroups` is read, not inferred** (step 7): from `/proc/self/setgroups`, in the
  cloned child (raw calls), the clean image and the parent of a non-isolated run. It's
  denied by the single-ID writer, by `newgidmap` without a group range, and by
  inheritance (a namespace inside one that denies it). **Where it's denied,
  supplementary groups asked for are refused** (`GroupsDenied`), never silently left
  out.
- **The helpers** (`run_map_helper`): found on `PATH`, run with a clean environment,
  given 10 s (they run while every signal is blocked), their stderr drained as they run
  (4 KiB kept), a killed one waited for a second at most, and one that can't be waited
  for reported without signalling its PID.
- **The init confines itself before its command goes on** (step 7): the command waits on
  a go pipe while the init drops to `CAP_KILL`, sets `no_new_privs` and installs the
  container's filter. If the kernel refuses the filter, the command is killed and the
  failure reported as its own (with the emulation hint). A profile that refuses the
  init's `wait4`, `rt_sigtimedwait` or `kill` makes it end the container with a "gave
  up" mark on the status pipe (`InitGaveUp`); one that kills it is reported as such
  (`InitKilledBySeccomp`), believed only with a PID namespace.
- **The session keyring** (step 8): `keyctl(KEYCTL_JOIN_SESSION_KEYRING, NULL)` in the
  clean image, before the start gate. ENOSYS (no keyrings) and EPERM (an outer filter,
  which the container inherits: K4) leave nothing to share; anything else (EDQUOT) is
  `ChildStep::SessionKeyring`.
- **One way to load a bundle as its caller runs it** (`oci::load_bundle_for_caller`),
  used by `run --bundle`, `create` and the monitor, so they can't disagree.
- **The state root** (`choose_root`, pure): `/run/shipyard` for root;
  `$XDG_RUNTIME_DIR/shipyard` for anyone else, where the directory must be absolute, a
  directory and the caller's; else `NoStateRoot`, which asks for `--root`. Anywhere but
  Linux, `LinuxOnly` first.
- **Failures say who and what refused:** an EPERM from a step after the filter went on
  adds "(the seccomp profile, in force by then, may have refused it)" (`FilterStart`);
  an EACCES in a user namespace names the host user the container's root is; a refused
  `CLONE_NEWUSER` names the setting (`kernel.apparmor_restrict_unprivileged_userns`,
  `kernel.unprivileged_userns_clone`, `user.max_user_namespaces`); a failing helper's
  words come with what to check. New `ChildStep`s 38–51.

**Considered and not done, with the reason:**

- **libseccomp:** a C library, and Shipyard's own compiler is testable on every platform
  against a reference evaluator.
- **An ENOSYS stub for calls newer than the profile** (runc's): the user chose the
  profile's own answer.
- **Leaving the init unfiltered** (step 3's choice): it kept the init's supervision safe
  from a user's profile, but left PID 1 the one unfiltered process in the container.
  Step 7 filters it, and makes every way a profile can break it visible.
- **Checking the filter before the start gate**, in a throwaway child, so `create` would
  report Rosetta rather than `start`: that child takes PID 2 in the new namespace (and
  so would a thread), pushing the command to PID 3.
- **Idmapped mounts** (`MOUNT_ATTR_IDMAP`) to give a root filesystem the range's owners:
  with the image store (stage 6). Until then the owner is the caller's business
  (`chown -R`).
- **A `/tmp` default for a rootless state root:** a guessable path invites races.
- **`--no-new-keyring`:** nothing needs to share its caller's keys yet.

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


**Session keyrings** (`keyrings(7)`, `keyctl(2)`), added in step 8, natively and under
emulation:

| # | Behaviour | Result | Consequence |
|---|---|---|---|
| K1 | `keyctl(KEYCTL_JOIN_SESSION_KEYRING, NULL)` | a new keyring, with a new serial | One call gives the container its own |
| K2 | a child after `fork` and `exec` | has the new one | The command inherits the init's |
| K3 | uid 65534 in a new user namespace, no maps | its keyring before was **root's** (a switch of user keeps the session keyring); joining works | The sharing is real, and the fix needs no privilege |
| K4 | under a filter refusing `keyctl` with EPERM | joining and reading both EPERM | An outer filter that refuses it leaves nothing to share: safe to go on |
| K5 | an otherwise unused uid adds keys until refused, then joins | EDQUOT after 1999; joining then EDQUOT | The key quota (`kernel.keys.maxkeys`) is the one real failure |

Under emulation K1, K3 and K5 are the same; K4 can't be checked there (no filter
installs), and K2's `exec` of `/proc/self/exe` fails under the emulator itself (Shipyard
re-executes through a descriptor instead).

**Also checked during the reviews:** with `no_new_privs`, a root process's effective
set after `exec` is what it was given; without it, root gets its whole bounding set back
as effective and permitted (the file capabilities of any program count as all for uid 0,
`capabilities(7)`), which is why a bundle asking root for less than its bounding set gets
it only with `noNewPrivileges`. Glibc's `_exit` with `exit_group` refused falls back to a
trap instruction (exit 133). A blocked, ignored SIGCHLD still makes the kernel reap
children itself, and survives `exec` (TLPI §26.3.3).

## macOS and Linux differences

| Topic | Linux | macOS |
|---|---|---|
| Capabilities, `no_new_privs`, seccomp, user namespaces | as above | `--cap-add`, `--cap-drop`, `--security-opt`, `--uidmap`, `--gidmap` are errors, like the other isolation flags |
| Capability names, the syscall tables, the profile and the compiler | — | all built and unit-tested on macOS too (the property test included) |
| `shipyard-runtime` | as above | `LinuxOnly` for every call, before any question of a state root |
| musl | builds | — |
| x86-64 emulation (OrbStack's Rosetta) | every seccomp filter refused: isolated runs fail with a hint, unless `seccomp=unconfined` (the test support adds it, and the filter's tests skip); `mount_setattr` ENOSYS, so a user namespace's `/sys` takes stage 3's remount fallback; Rosetta reads `/proc/sys/vm/mmap_min_addr` to start each program, so a bundle masking `/proc/sys` stops it (exit 133); with a start gate, the refused filter shows at `start` | — |

## Failure-mode register

| Worst case | Handling | Test |
|---|---|---|
| A newer kernel's capabilities, unknown to Shipyard, kept | dropped up to `cap_last_cap`, read at run time | the bounding-drop unit tests (kernels at 37, 40, 45) |
| `--cap-add ALL` on a kernel before 5.9 | `ALL` means all the kernel has | unit test with a kernel at 37 |
| A bundle's capability sets the kernel would refuse (a bare EPERM at start) | `CapabilitySets::check` when read, naming the rule and capability | OCI unit tests |
| A bundle masking `/proc/sys` can't run (`cap_last_cap` unreadable) | read first, from the host's `/proc` | `a_bundle_may_mask_proc_sys` |
| Root inside reading the init's descriptors | no `CAP_SYS_PTRACE`, and the init non-dumpable (P1) | `root_inside_cant_reach_the_inits_descriptors` |
| A non-root command never getting signals | the init keeps `CAP_KILL` | `the_init_still_forwards_signals_to_a_non_root_command` |
| Forgery tests passing without forging | they must print `reached-the-pipe` | the three forged-record tests |
| `seccomp=/dev/zero` or a FIFO hangs the run | non-blocking open, regular files only, 1 MiB | `small_file.rs` and profile unit tests |
| A call the filter can't name (newer than libc) refused for every program | the tables add the kernel's numbers and `EXTRA` | the table tests on both architectures |
| A refusal by the filter blamed on the step | "(the seccomp profile, in force by then, may have refused it)" | `failures_under_a_filter_mention_it` |
| 32-bit or x32 calls slipping past a 64-bit filter | the architecture and x32 checks first, kill | Docker-profile unit tests, the property test |
| A filter refused by the kernel (Rosetta, before 4.14) | the run fails with the hint; never unfiltered | `under_emulation_a_filter_is_refused_with_a_hint` |
| A profile over 4096 instructions | refused with its length | assembler unit tests |
| Maps written to the wrong process (`/proc` of another PID namespace) | `ForeignProc` before the clone | `a_foreign_proc_is_refused` |
| A map write failing after the clone (a nested Shipyard mapping IDs its namespace lacks) | the child killed and reaped; the error, with the hint | by hand (`unshare -U -r`, chapter 18); maps the kernel would refuse for other reasons are refused before the clone (`maps_the_kernel_would_refuse_are_refused`) |
| The clean image starting with no capabilities in a user namespace | `setresuid(0)` inside before the re-exec | every user-namespace test (the mutant fails them all) |
| The host's mounts in `/sys` visible inside | covered with empty read-only tmpfs | `the_hosts_mounts_in_sys_stay_out` |
| `--uidmap 0:0:N`: a namespace protecting nothing | allowed, with a warning (CLI and bundles) | `mapping_host_root_warns` |
| "Permission denied" without saying who | names the host user the container's root is | `a_permission_error_names_the_mapped_user` |
| A hung `newuidmap` (hung NFS) leaves shipyard deaf to Ctrl-C | 10 s, then killed; reaped for a second at most | `a_hung_helper_is_stopped` |
| A helper writing more than a pipe holds passes for hung | stderr drained while it runs, 4 KiB kept | `a_chatty_helper_isnt_taken_for_hung` |
| A helper that can't be waited for (SIGCHLD ignored): a reused PID killed | reported, nothing signalled | by reasoning (unreachable with shipyard's own SIGCHLD handling) |
| Subordinate ranges the kernel would refuse | cut or validated; the single ID with the reason | `choose_rootless_maps` unit tests |
| `create` and the monitor loading a bundle differently | one `load_bundle_for_caller` | `a_rootless_container_lives_its_life` |
| A monitor started with SIGCHLD ignored losing the exit | the monitor restores the default | `a_caller_ignoring_sigchld_doesnt_lose_the_exit` |
| `setgroups` denied by inheritance (`unshare -U -r`) or `newgidmap` | read from `/proc/self/setgroups` | `an_inherited_setgroups_deny_is_respected` |
| `--group-add` silently not applied under the single map | refused (`GroupsDenied`) | `without_helpers_only_root_inside_is_mapped` |
| The command running beside an init with root's power | the go pipe: the init confines itself first | by the code's order (the window is too small to test) |
| A profile refusing `wait4`: a container nobody can reap | the init gives up, `InitGaveUp` | `a_profile_refusing_the_inits_wait_ends_the_container` |
| A profile killing the init passed off as the command's SIGSYS | `InitKilledBySeccomp`, only with a PID namespace | `a_profile_killing_the_init_says_so`, `init_outcome` unit tests |
| A profile refusing `kill`: signals lost until SIGKILL | the init gives up at the first one | `a_profile_refusing_the_inits_kill_ends_the_container` |
| The caller's session keyring reachable from a container | a new one per container | `a_container_has_its_own_session_keyring`, `a_rootless_container_has_its_own_session_keyring` |
| Too many keyrings for one user | `SessionKeyring`, naming `kernel.keys.maxkeys` | the message's unit test |

## Known limitations (by design, for stage 4)

- **Not a hardened boundary yet:** the container shares the host's network (stage 7) and
  has no resource limits of its own (stage 5).
- **`/sys` is the host's, read-only,** in a user namespace (until stage 7). With
  `--cap-add SYS_ADMIN`, root inside can unmount the covers over its submounts and read
  (not write) what they hid, such as the host's cgroup tree.
- **With the single-ID map,** the command keeps the caller's supplementary groups on the
  host (`nogroup` inside, still counted outside), and can't be given others.
- **A profile must allow what the init needs** (`rt_sigtimedwait`, `wait4`, `kill`, and
  `write` and `exit_group` to say so): one refusing `write` or `exit_group` ends the
  container without a message.
- **Calls newer than a profile get its default action** (EPERM in Docker's), not ENOSYS,
  so a program can't fall back by itself.
- **32-bit programs don't run under a filter;** `seccomp=unconfined` runs them.
- **Rootless limits only go down:** raising a hard limit needs root on the host.
- **Files a subordinate ID creates** in the caller's root filesystem can't be removed by
  the caller directly; root inside can.
- **Each container takes one key** from its user's quota (`kernel.keys.maxkeys`).
- **Joined processes** (`setns`, internal for now) keep the joiner's capabilities and run
  unfiltered until `exec` (stage 8).
- **Recorded residuals:**
  - a helper stuck in the kernel is left unreaped after its kill;
  - the order "the init confines itself before the command goes on" is held by the code,
    not by a test;
  - `/dev` and `/etc/passwd` are still opened by name (non-blocking, so a FIFO can't hang
    the init, but a device node there would be opened);
  - `SeccompError::Architecture` and `BuiltIn` share a generic message (both rare);
  - one rootless test needs util-linux 2.38 (`unshare --map-current-user`).

## Carried forward

| Stage | Item |
|---|---|
| 5 (cgroups) | Device access control (eBPF); the cgroup namespace. |
| 6 (images) | Idmapped mounts, so an image's root filesystem belongs to the user namespace's range. |
| 7 (networking) | A `sysfs` of the container's own, which makes the `/sys` covers moot; `net.*` sysctls. |
| 8 (`exec`) | Joining a user namespace; capabilities, `no_new_privs`, a filter and the keyring for joined processes. |
| 14 (hardening) | Opening `/dev` and `/etc/passwd` through descriptors (`O_PATH`, check, reopen); distinct messages for the rare seccomp errors; fuzzing the profile parser and the compiler; the OCI conformance suite. |

**Resolved from stage 3:**

- **Capabilities, `noNewPrivileges`, seccomp:** confinement makes the init's non-dumpable
  flag effective (P1).
- **User namespaces:** `/dev` from bound host nodes and `/sys` from a host bind (U6, U7),
  as the stage 3 probes predicted.
- **runc's new session keyring**, by default (step 8).
- **A FIFO at `/etc/passwd`** can't hang the init (opened non-blocking); the rest of that
  item is carried to stage 14.

## Tests

| Kind | Where | What |
|---|---|---|
| Unit | `crates/shipyard-runtime/src/**`, `crates/shipyard-cli/src/**` | capability names and sets, `resolve`, the bounding drop and `capset` words, `CapabilitySets::check`; the syscall tables on both architectures; the profile (validation, null lists, `from_oci`'s fields); the assembler (offsets, far jumps, every gap from 250 to 261, too long); a classic-BPF interpreter running the compiled programs, **a property test** (2000 random profiles against a reference evaluator) and Docker's profile on both architectures; ID maps (parsing, validation, rendering, subordinate ranges, the rootless and single maps); the writer choice, `choose_rootless_maps`, the refusal explanations; `add_user_namespace`, `choose_root`; `init_outcome`; every new failure message |
| Confinement | `crates/shipyard-cli/tests/security.rs` | 21 root tests and 1 as any user: the default set and `NoNewPrivs`, adding and dropping, a non-root user's ambient set, setuid and file capabilities disarmed, the init's `CAP_KILL` and `/proc/1`, bundles' exact sets, a masked `/proc/sys`, the default filter, a profile of one's own, the early install, a bundle's filter, emulation's hint, the init under the filter and the profiles that break it, the session keyring |
| User namespaces | `crates/shipyard-cli/tests/userns.rs` | 15 root tests and 1 as any user: maps and uid 0 inside, files on the host owned by the range, the host's `/etc/shadow` out of reach, `--gidmap`, unmapped IDs, a root filesystem (bound `/dev`, read-only `/sys` and its covers, devpts), the host-root warning, a permission error naming the user, a foreign `/proc`, maps the kernel would refuse, a bundle's user namespace |
| Rootless | `crates/shipyard-cli/tests/rootless.rs` | 15 tests as a normal user: the full and single maps, files owned by the caller and its range, `--group-add` refused, an inherited `setgroups` denial, a root filesystem, confinement, exit codes and signals, hung, chatty and failing helpers, bundles, the session keyring |
| Runtime | `crates/shipyard-cli/tests/runtime.rs` | adds: bundles' capabilities and filter, the rootless lifecycle and state root, a caller with SIGCHLD ignored |
| Terminal | `crates/shipyard-cli/tests/terminal.rs` | the 15 `ISOLATED_TESTS` now also run as a normal user, rootless (single map: the fake shell clears the environment) |
| Kernel | `scripts/probes/security.c` | the checks above |

**Test mechanics worth knowing:**

- **`rootless(Rootless::Single | Full)`** (test support) says whether a rootless test can
  run here: a normal user, `unshare --user` working, and for `Full` the helpers and a
  range. It skips with the reason, unless `SHIPYARD_TEST_ROOTLESS=required`.
- **`for_this_kernel`** adds `--security-opt seccomp=unconfined` right after `run` under
  Rosetta, so the rest of the suite still runs there.
- **The test image** (`scripts/test-image/Dockerfile`): `rust:1-bookworm` plus `uidmap`
  and a `tester` user with `100000:65536` in `/etc/subuid` and `/etc/subgid`, tagged
  with the file's hash so `test-linux.sh` rebuilds it only when it changes.
- **Reading the kernel's state from inside:** the tests read `/proc/self/status`
  (`CapBnd`, `CapEff`, `CapAmb`, `NoNewPrivs`, `Seccomp`) and `/proc/1/status` for the
  init; the session keyring through `python3`'s `ctypes` (there's no `keyctl` tool in
  the image), under `seccomp=unconfined`, since Docker's profile refuses `keyctl`.
- **`stat -c %u /proc/self` reads the symlink** (whose owner is a kernel detail); the
  tests use `stat -L`.
- **Stand-in helpers** (`FakeHelpers`) on `PATH` make hung, chatty and failing
  `newuidmap`s.

**Mutation-checked.** Every important behaviour was removed on a scratch copy
(`rsync -rlp`), each with its own target directory, and its tests confirmed to fail:

| Step | Mutations, each caught |
|---|---|
| 2 | no bounding drop; no `no_new_privs`; no ambient raise; the init keeping every capability; the init dropping `CAP_KILL`; root given a non-root user's sets; the forgery tests without `SYS_PTRACE` |
| 3 | no architecture check; x32 allowed; the arguments' high half ignored; no precedence; `includes.caps` ignored; the assembler's trampoline off by one; no install; always installed last |
| 4 | maps not written; no switch to the namespace's root; `mknod` in a user namespace; no mapped-ID check; `gid=5` always |
| 5 | no `setgroups` deny; the helpers never used; `setgroups` called where denied; the default user not 0; the root check kept |
| 6 | mappings ignored; the consistency check removed; the automatic namespace not added; the default root always `/run/shipyard`; the monitor loading the bundle plainly |
| 7 | the child ignoring a `setgroups` denial (caught once the `--uidmap` case was added); the clean image's groups check removed; the parent ignoring the denial; the helper's stderr not drained; `reap` returning on any error; the monitor keeping SIGCHLD ignored; the init unfiltered; the init ignoring a failed `kill`; an init's SIGSYS taken for the command's; the groups refusal before the caller's rights |
| 8 | the keyring join removed |

**Verified at the end of the stage:** fmt; clippy on macOS, aarch64, x86-64 and musl
with warnings as errors; rustdoc as CI runs it (`--document-private-items`, a Linux
target); both generated references; the macOS suite; `scripts/test-linux.sh` (clippy,
everything as a normal user with the rootless and terminal tests, then everything as
root); the whole workspace under x86-64 emulation; the probe natively and under
emulation.

**CI:** the Linux job lifts Ubuntu's AppArmor restriction on unprivileged user
namespaces, installs `uidmap`, gives the runner a range, and runs the suite with
`SHIPYARD_TEST_ROOTLESS=required`; the `root` job runs the root tests. The Linux job's
steps were rehearsed in `ubuntu:24.04` as a normal user (step 7); the runner's AppArmor
is its own kernel's, which only a push checks.

## Review history

Each step had a clean-code review and a worst-case ("Murphy's law") review; step 7 added
a full review by three read-only reviewers with separate briefs (the security model end
to end, the after-fork and clean-image rules, the docs against behaviour), every finding
reproduced on Linux before it was fixed.

- **Step 1 (probe, capability names, syscall table):**
  - review: the probe's filter lost two do-nothing instructions and numbers each one
    with where its jumps lead; one `COUNT` drives the capability table;
  - worst case: **the probe left a setuid-root copy of itself in `/tmp` that anyone
    could run** as root (its only check was `geteuid`): it now only reports when run
    setuid, and lives on a tmpfs in its own mount namespace, so it can't outlive the
    probe; **the syscall table missed real calls** (names only one libc had; everything
    since Linux 6.5): a union of both, plus the kernel's numbers; the emulated
    `mount_setattr` ENOSYS, found by diffing the full outputs.
- **Step 2 (capabilities, `no_new_privs`):**
  - review: one `prctl` wrapper; no boolean arguments in a row; one step, one meaning
    (`TooManyCapabilities`);
  - worst case: a bundle masking `/proc/sys` couldn't run; sets the kernel would refuse
    failed late with a bare EPERM; `--cap-add ALL` on older kernels; `no_new_privs`
    disarming file capabilities too (documented, with `--cap-add NET_RAW` as the way
    out); the forgery tests that passed without forging.
- **Step 3 (seccomp):**
  - review: the filter's placement stated once (`filter_before_setup`,
    `filter_before_exec`); the compiler's parts named (`check_architecture`, `search`,
    `check_rules`, `Returns`); `any_holds` and `all_hold` say Docker's two rules;
    `describe_confinement`;
  - worst case: `seccomp=/dev/zero` and FIFOs hung (`small_file.rs`); refusals by the
    filter blamed on the step (`FilterStart`); sign-extended arguments, `errnoRet: 0`
    and a refused `execve` plus `write` documented. The filter costs about 20 ns a call.
- **Step 4 (user namespaces):**
  - review: `NamespaceRootStep` and `UnmappedId` replace bare numbers on both sides of the
    report pipe; `IdMaps::each`; `RunArgs::id_maps`;
  - worst case: the host's mounts in `/sys` (covered); host root mapped silently
    (warned); permission errors that didn't say who; a nested Shipyard's EPERM explained;
    a foreign `/proc` (`ForeignProc`).
- **Step 5 (rootless `run`):**
  - review: map writing moved to `user_namespace.rs`; `choose_rootless_maps` pure;
    `RootlessMaps`;
  - worst case: a hung helper froze shipyard (10 s deadline); ranges the kernel would
    refuse (cut, validated, the single ID with the reason); a failing helper's words
    now come with what to check.
- **Step 6 (rootless bundles and runtime):**
  - the first rootless run showed `create` and the monitor loading the bundle
    differently: one `load_bundle_for_caller`;
  - review: `id_maps`' guards; `MappingConfig`;
  - worst case: a bundle mapping host root warns too.
- **Step 7 (terminals, CI, docs, the full review):** the terminal tests run rootless;
  CI was rehearsed; the docs were checked line by line against behaviour. The review:
  - **the command could run beside an init with root's power:** the go pipe;
  - **the init wasn't filtered:** it is now (the user's choice), and a profile that
    breaks it ends the container visibly; a pre-check of the filter was tried and
    dropped (it took PID 2);
  - **`setgroups` was inferred:** read now; `--group-add` refused where denied;
  - **the helpers' wait:** a bounded stop, stderr drained, no kill of an unwaitable PID;
  - **a monitor with SIGCHLD ignored** lost the container's exit;
  - clean-code: `confine_self` only confines; the go pipe only with an init;
    `UserSwitch::as_namespace_allows`; `StderrDrain`; `is_setup_failure`;
  - worst case: **a profile that kills the init** was passed off as the command's SIGSYS
    (`InitKilledBySeccomp`); **a profile refusing `kill`** lost signals until SIGKILL
    (the init gives up); a non-root caller's groups refusal now says "needs root" first.
- **Step 8:** the session keyring (probe K1–K5, `ChildStep::SessionKeyring`), these notes
  rewritten, and the book's chapters 16–19. Writing chapter 17 found **errors that said
  their reason twice:** a misspelled call in a profile printed "\"mkdirta\" isn't a system
  call …" twice, and an unreadable `config.json` "Permission denied" twice, in `shipyard`
  and `shipyard-runtime`. Three variants (`SeccompError::InRule`, `BundleError::Read`,
  `ContainerError::Io`) put the reason in their message *and* in a field named `source`,
  which `thiserror` makes the error's cause, and the CLI prints every cause. The field is
  `error` now (the message keeps it); a unit test for each checks there's no cause. The
  recheck of chapter 7 found **the user-namespace note on a non-permission EACCES:**
  running a directory said "/tmp: is a directory (in its user namespace, the container's
  root is host user 1000, …)". `exec`'s EACCES now gets the note only where it's about
  permissions (`describe_exec`); unit-tested both ways. The clean-code review: the keyring tests' shared
  helper returns `[&'static str; 3]` (its script built once, in a `LazyLock`), so neither
  test converts a `Vec<String>`; `keyring_of_a_command` says what it measures (it runs
  without isolation too); the keyring join has its comment in the clean image's setup. The
  worst-case review: **the probe's K5 filled `nobody`'s key quota**, which is
  kernel-wide, not per container: run on a host as documented, that's the quota real
  services share, and a second run straight after found it still full (the kernel frees
  the keys some seconds later). K5 now uses an otherwise unused uid, and says so when a
  run moments before still holds the quota. **Checked, and fine:** a command switched to
  another user (`--user 1000`, rootful and rootless, the single map too) can add and read
  keys in its root-made keyring (it *possesses* it); a second container of the same user
  can't read the first's key by its serial (EACCES); a full quota fails `run` and
  `create` with the message (exit 125 and 1), leaving nothing behind, and
  `--no-isolation` is unaffected; no other error repeats its cause (every `#[from]` is
  `transparent`, and `Error::Sys` keeps its errno out of the message). Not automated:
  filling a test user's quota would fail other rootless tests running alongside.

**Lessons worth keeping:**

- **Order is the security.** Drop the bounding set before the switch, write the maps
  before anything makes the child non-dumpable, install the filter last with
  `no_new_privs` and first without, and confine the init before its command goes on.
- **Read the kernel's state; don't infer it.** `setgroups` can be denied three ways, two
  of them invisible from the maps.
- **Whatever you confine, make every refusal visible.** A filter on your own supervisor
  turns a refused call into a hang, a silent exit or a misattributed signal unless each
  path says what happened.
- **Never block every signal without a deadline,** and never wait without a bound for a
  process that may be stuck in the kernel.
- **Dispositions survive `exec`:** an ignored SIGCHLD makes the kernel reap your children.
- **Emulation is a different kernel to your program:** no seccomp, no `mount_setattr`,
  and an emulator that reads `/proc/sys` itself.
- **A test that passes without doing its job proves nothing:** the forgery tests that
  never reached the pipe, the mutant that survived until a test made a user namespace.
- **Two readers of the same input must be one function** (`create` and the monitor).

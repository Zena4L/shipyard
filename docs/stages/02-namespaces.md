# Stage 2 notes: namespaces

> **In progress.** This file grows with each step and becomes the full stage notes (same
> format as [stage 1](01-process-lifecycle.md)) when the stage is done.

## Decisions made before starting

- **On Linux, `shipyard run` isolates by default:** new PID, mount, UTS and IPC
  namespaces. That needs root until rootless support (stage 4). `--no-isolation` keeps
  the stage-1 behaviour. macOS keeps the stage-1 behaviour until the VM (stage 12).
- **Our own small init is always PID 1** in the container; the command is PID 2.
- **Joining a running container (`setns`)** is a runtime primitive with tests. The CLI
  comes once containers have IDs (stages 3, 8, 9).

## Kernel checks

Checked before building on them, with [`scripts/probes/namespaces.c`](../../scripts/probes/namespaces.c),
on Linux 7.0 (OrbStack, arm64) in a `rust:1-bookworm` container: privileged for the
namespace checks, unprivileged for the seccomp check.

| Behaviour | Result | Consequence |
|---|---|---|
| A namespace's PID 1 gets signals it **blocks**, even without a handler | Yes: SIGINT, SIGUSR1 and SIGTERM sent from outside all arrived through `sigwaitinfo`, with `si_pid` 0 (the sender isn't in the namespace) | The init blocks every signal and waits with `sigwaitinfo` |
| PID 1 gets an **unblocked** signal that has no handler | No: SIGUSR2 was silently dropped | Never rely on default actions in the init |
| `clone3` under Docker's default seccomp | `ENOSYS` | Fall back to the raw `clone` syscall |
| `clone(CLONE_NEWPID)` without privileges | `EPERM` | Report "needs root" with `--no-isolation` advice |
| A fresh `proc` mount in a new mount + PID namespace | Shows only the namespace's processes; the host's mount table is unchanged afterwards | The init mounts `/proc` after making `/` private |
| Propagation of `/` | Private inside Docker; systemd hosts use shared | Always remount `/` recursively private first |

## Step 2: the clone path, the handshake, mounts and hostname

- **`clone3`, falling back to `clone`** (`process/clone.rs`): raw system calls with no
  stack, so they behave like `fork`. The fallback covers kernels before 5.3 and Docker's
  seccomp profile (ENOSYS); EPERM means no privileges.
- **The setup handshake** (`process/channel.rs`): a "go" pipe from parent to child. The
  child waits for it before setting up. The parent keeps its end open until `exec`, so the
  child can tell whether the parent died (the pipe hangs up). This replaced the
  `getppid()` check, which returns 0 inside a PID namespace.
- **In the child, inside the new namespaces** (`process/child.rs`), before switching
  user:
  1. a session of its own (`setsid`);
  2. `/` remounted recursively private, before any other mount;
  3. a fresh `/proc`;
  4. the hostname.
- **The command is PID 1 until step 3's init.** Like Docker without `--init`, it ignores
  signals it doesn't handle; a stop request then ends with SIGKILL after
  `--stop-timeout`.
- **Tests:**
  - `crates/shipyard-cli/tests/isolation.rs`: root-only, plus the "needs root" error for
    other users;
  - every other suite runs with `--no-isolation`, since it tests process behaviour;
  - `scripts/test-linux.sh` makes `/` shared before the root pass, as on systemd hosts.
    Otherwise Docker's private `/` would hide a mount leak.
- **Mutation-checked:** removing the private remount, the `/proc` mount, the hostname,
  the PID namespace or the parent-death signal each fails at least one test.
  **Mutation runs need their own build cache.** Cargo decides what to rebuild by file
  timestamps, so a mutated binary built in the shared cache, at the same source path, was
  reused by the next normal run and failed a correct test.

### Review fixes (step 2)

- **The raw `clone` call now passes full-width null pointers.** It had passed `0`s
  through C's `...`, where a 32-bit integer leaves the upper half of a 64-bit argument
  undefined. The flags are converted bit for bit (`cast_unsigned`), not by absolute value.
  The fallback path has its own test, since normal hosts never take it.
- **`clone3` failing with EPERM** (older seccomp profiles) also falls back to `clone`.
- **As root, an EPERM** means a container without privileges or a security policy, not
  "needs root"; the errors and hints now say which.

### Known limitations and carried forward

- **Not a security boundary yet:** root on the host's files. The fresh `/proc` is mounted
  *over* the host's, which root inside could unmount. **Stage 3** (`pivot_root`) must
  leave the old root, and that `/proc` with it, unreachable.
- **If `/` isn't a mount point** (a plain `chroot`), making it private fails with EINVAL.
- **The command is PID 1 until step 3**, so unhandled signals (Ctrl-C to `sleep`) are
  ignored.

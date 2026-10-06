# Running commands

```sh
shipyard run [OPTIONS] COMMAND [ARGS...]
```

`shipyard run` starts a command the way a container runtime does: with a clean
environment and exactly the user, working directory and limits you ask for.

> **Status:** on Linux, `run` isolates the command in its own process IDs, mounts,
> hostname and IPC (see [Isolation](#isolation)). With `--rootfs` it also gets a
> root filesystem of its own (see [Root filesystem](#root-filesystem)); without it, it
> sees the host's files. It always shares the host's network: networking comes in a
> later version. On macOS the command isn't isolated yet.

## Arguments

Shipyard's options go **before** the command. Everything from the command onward
belongs to the command:

```sh
shipyard run -e A=1 ls -l --env X     # ls receives: -l --env X
```

A mistyped option before the command is an error, so it is never mistaken for
the command. Use `--` if the command itself starts with `-`:

```sh
shipyard run -- --program-starting-with-dash
```

Arguments are passed through byte for byte, including ones that aren't valid UTF-8.

## Isolation

On Linux, `shipyard run` starts the command in new **namespaces**, so it gets
its own view of:

| What | Inside the container |
|---|---|
| Process IDs | The command sees only its own processes (`ps`, `/proc`). It's PID 2: PID 1 is a small init (see below). |
| Mounts | Mounts made inside stay inside; the host never sees them. Mounts the host makes or removes later do reach the container, so a disk unmounted on the host isn't kept busy by it. |
| Hostname | Its own copy; set it with `--hostname`. |
| IPC | Its own System V IPC objects and POSIX message queues. |

Without `--rootfs`, the command sees the host's files. It always sees the host's
network: networking comes in a later version.

- **Needs root.** Creating namespaces needs root privileges (support for running
  without root comes later). Otherwise `run` stops before starting anything, with
  exit code 125 and a message saying so.
- **Refused by the system.** Even as root, namespaces can be refused, for example
  inside a container started without privileges, or by a security policy
  (seccomp, AppArmor, SELinux). `run` then exits with 125 and says the system
  refused; use `--no-isolation` there.
- **`--no-isolation`** runs the command without namespaces: it shares the host's
  processes, mounts, hostname and IPC, and doesn't need root.
- **`--hostname NAME`** sets the hostname inside: letters, digits and hyphens in
  dot-separated parts (like `web-1` or `db.internal`), at most 64 characters, no
  part starting or ending with a hyphen. Not with `--no-isolation`.
- **`--domainname NAME`** sets the NIS domain name inside (what `domainname`
  prints), with the same rules as the hostname. The host's is unchanged. Not
  with `--no-isolation`.
- **`--sysctl KEY=VALUE`** sets a kernel setting for the container, such as
  `--sysctl kernel.shmmax=1073741824`. Repeatable. Only settings the container
  owns are allowed: the shared-memory, message-queue and semaphore ones
  (`kernel.shmmax`, `kernel.shmall`, `kernel.shmmni`, `kernel.shm_rmid_forced`,
  `kernel.msgmax`, `kernel.msgmnb`, `kernel.msgmni`, `kernel.sem`, and
  `fs.mqueue.*`). The kernel keeps a separate copy of those for each
  container, so the host's stay as they are. Any other setting would change
  the whole machine, so it's refused, including network settings (`net.*`),
  until containers get networking of their own. Use `--hostname` and
  `--domainname` for those two. Not with `--no-isolation`.
- **A small init is PID 1.** The process with PID 1 in a container is special:
  the system ignores any signal it has no handler for. So Shipyard runs its own
  small init as PID 1, and the command as PID 2, where signals work normally.
  The init passes every signal on to the command, cleans up exited processes
  ("zombies") in the container, and when the command exits, stops everything
  else left in the container and reports the command's exit status exactly.
  It shows up as `shipyard-init` in `ps`. It holds none of Shipyard's command
  line or environment, so `-e` values passed to Shipyard can't be read from it
  inside the container.
- **Terminal:** see [On a terminal, isolated](#on-a-terminal-isolated) below.
- **If Shipyard is killed**, the whole container goes with it.
- **Not a security boundary yet.** The command runs as root, with all of root's
  privileges; isolation keeps it from *seeing* and *disturbing* the rest of the
  system by accident, not from breaking out on purpose. Running untrusted code
  safely also needs dropped capabilities and seccomp, which come in later
  versions.
- If `/` isn't a mount point (for example inside a plain `chroot`), isolation
  fails with "making the container's mounts private: Invalid argument"; use
  `--no-isolation` there.
- **macOS:** commands aren't isolated yet (as with `--no-isolation`);
  `--hostname`, `--domainname`, `--sysctl` and `--rootfs` are errors there.

### On a terminal, isolated

An isolated command runs in a session of its own. It's never in the terminal's
foreground group and has no *controlling terminal*, though its input and output
are still the terminal. So:

- **Ctrl-C and Ctrl-\\** reach it through Shipyard, exactly once. Ctrl-C stops
  a `sleep` as usual, and Shipyard then ends as if interrupted, so a shell loop
  around it stops too.
- **Resizing the window** is passed on (SIGWINCH), and the command sees the new
  size.
- **Ctrl-Z stops only Shipyard.** The command keeps running, and its output keeps
  appearing. `fg` resumes Shipyard. If the command finishes meanwhile, Shipyard
  reports it once resumed.
- **Closing the terminal** ends the command (exit code 129), unless the command
  ignores hangups. Then it keeps running, and Shipyard with it.
- **A command that stops itself** (`kill -STOP $$`, or a debugger) doesn't stop
  Shipyard, so the terminal seems to hang. Press Ctrl-Z (Shipyard stops and
  your shell comes back), then `fg`: that resumes both. Ctrl-Z typed into a
  program like `vim` or `less` inside doesn't do this; the system ignores that
  request for an isolated command.
- **Programs that open `/dev/tty` fail.** That's how `sudo`, `ssh` and others ask
  for a password, so they can't prompt. Use `--no-isolation` for those, until
  full terminal support comes in a later version.
- **With `--rootfs`, the terminal has no name inside.** The container's `/dev/pts`
  is its own, so your terminal isn't in it: `tty` prints "not a tty", and programs
  that look the terminal up by name can't find it. Reading and writing it, Ctrl-C
  and resizing work as above.
- **Don't run an isolated command that reads the terminal in the background.**
  Nothing stops it from reading (as a background job normally would be), so it
  takes keys you type into your shell.

Use `--no-isolation` for full job control.

## Root filesystem

```sh
sudo shipyard run --rootfs ./rootfs sh
```

(To describe a whole container in one file instead, use an OCI bundle: see
[OCI bundles](bundles.md).)

With `--rootfs DIR`, the directory DIR becomes the container's `/`. The command
sees only what's in it: nothing of the host's files is reachable from inside. DIR
must hold the programs you run and everything they need (their libraries, for
example). Any directory works, such as an unpacked image or a `debootstrap`. Pulling
images comes in a later version.

The program and the working directory (`-w`) are looked up inside DIR, with the
container's `PATH`.

Shipyard adds what every container needs:

| Path | What it is |
|---|---|
| `/proc` | The container's own processes only. |
| `/sys` | Read-only. |
| `/dev` | A fresh, small directory with only `null`, `zero`, `full`, `random`, `urandom` and `tty`, the links `fd`, `stdin`, `stdout`, `stderr` and `ptmx`, and its own `pts` (terminals), `shm` (shared memory) and `mqueue` (message queues). The host's devices aren't there. |

Shipyard creates `/proc`, `/sys` and `/dev` in DIR if they're missing. DIR itself
is never changed otherwise, and what's mounted inside never shows on the host. If
something is mounted on DIR's `proc`, `sys` or `dev` on the host (as in a chroot
set up by hand), the container gets its own in their place and can't reach the
host's.

**Hidden and read-only paths.** Some kernel files would tell the container about
the host, or let it change the host's settings, so they're hidden or read-only (the
same ones as Docker):

- **Hidden** (they read as empty): `/proc/acpi`, `/proc/asound`,
  `/proc/interrupts`, `/proc/kcore`, `/proc/keys`, `/proc/latency_stats`,
  `/proc/timer_list`, `/proc/timer_stats`, `/proc/sched_debug`, `/proc/scsi`,
  `/sys/firmware`, `/sys/devices/virtual/powercap`.
- **Read-only:** `/proc/bus`, `/proc/fs`, `/proc/irq`, `/proc/sys`,
  `/proc/sysrq-trigger`.

**`--read-only`** makes the root filesystem itself read-only inside: the command
can't change anything in DIR. `/dev` (including `/dev/shm`) and `/proc` stay as
they are. The mount's other options, such as `nosuid` or `noexec`, are kept.

**Limits for now:**

- `/sys` shows the host's network devices, until containers get networking of their
  own.
- Directories mounted *inside* DIR on the host come along into the container, but
  `--read-only` doesn't make them read-only.
- There's no `/etc/hosts`, `/etc/resolv.conf` or `/etc/hostname` from Shipyard
  yet, and no volumes: only what DIR has.

**Errors** (exit code 125, before the command starts):

- DIR doesn't exist, or isn't a directory.
- DIR is `/`, the host's own root. Leave out `--rootfs` to share the host's files.
- DIR's `proc`, `sys` or `dev` is a symbolic link. Shipyard refuses to mount on
  it, because it could point out of DIR.
- They're missing and DIR is on a read-only file system, so they can't be
  created. Create them in DIR.

**If the program doesn't start:**

- **"found, but its interpreter or loader is missing"** (exit code 127): the
  program is in DIR, but a library it needs, or its `#!` interpreter, isn't. Or the
  program's path is a symbolic link to an absolute path that's missing inside DIR
  (Debian's `/usr/bin/awk` → `/etc/alternatives/awk`, say): Shipyard looks the
  program up on the host to explain the error, where such a link may resolve.
- **"mounted noexec"** (exit code 126): DIR is on a file system that forbids
  running programs (on some systems, `/tmp`). Put it somewhere else.

## Environment

The command does **not** inherit your shell's environment. It gets only:

1. the variables you set with `-e/--env`, and
2. defaults for anything you didn't set:

| Variable | Default |
|---|---|
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` |
| `HOME` | The user's home directory from `/etc/passwd` (the container's; see [User and groups](#user-and-groups)). If the user has no entry: `/root` for user 0, otherwise `/`. |

Your values always win over the defaults.

| Form | Effect |
|---|---|
| `-e KEY=VALUE` | Sets `KEY`. Only the first `=` splits, so `-e A==b` sets `A` to `=b`. |
| `-e KEY=` | Sets `KEY` to an empty string. |
| `-e KEY` | Copies `KEY` from your shell. If it isn't set there, it is skipped. A name with spaces is rejected, since it's almost certainly a mistyped `KEY=VALUE`. |

If you set the same variable twice, the last value wins.

These are rejected:

- an empty name (`-e =x`)
- a NUL byte in a name or value

**Values are never shown**: not in logs (even with `--debug`) and not in error
messages. Shipyard shows variable names only, so secrets such as `-e TOKEN=…`
stay out of your terminal history and CI logs.

## Working directory

| Option | Default |
|---|---|
| `-w, --workdir DIR` | `/` |

The path must be absolute.

## User and groups

| Option | Meaning |
|---|---|
| *(none)* | Run as the user who ran `shipyard`. |
| `-u USER` | Run as `USER`: a name such as `postgres`, or a number. |
| `-u USER:GROUP` | Run as `USER`, with primary group `GROUP` (a name or a number). |
| `--group-add GROUP` | Add a supplementary group (a name or a number). Repeatable; duplicates are ignored. |

**Names are looked up in the container's own files**, `/etc/passwd` and
`/etc/group`: its root filesystem's with `--rootfs`, otherwise the host's. Like
Docker:

- **A user name** must be in `/etc/passwd`. It gives the user ID, the primary group
  and the home directory.
- **A user number** is used as it is. If `/etc/passwd` has an entry for it, that
  entry's group and home directory are used; otherwise the group is `0`.
- **A group name** must be in `/etc/group`.
- **The user's groups:** in a container, the groups that list the user in
  `/etc/group` are added too, plus any `--group-add`. Not with `--no-isolation`,
  where `/etc/group` is the host's.
- **`HOME`** is the user's home directory, unless you set it with `-e HOME=…`.

A name that isn't there stops the run (exit code 125), saying which name and which
file. So does a name whose ID in the file is `4294967295`, which the kernel would
read as "leave unchanged". If the files can't be read, only names fail: numbers
still work, `HOME` falls back to its default, and no groups are added. The files are read directly, not through the system's user database. So on a
host whose users come from a directory service (LDAP, or macOS's own), only the
users listed in `/etc/passwd` can be named; numbers always work.

These are rejected:

- **ID `4294967295`.** The kernel reads it as "leave unchanged", so allowing it
  could silently leave the command running as root.
- **Names with spaces, `:`, `,` or `/`**, or that start with `-` or `+`.
- **Too many supplementary groups.** The limit is 15 on macOS (the system's 16
  slots include the primary group) and 65536 on Linux.

Switching to another user or group, or adding groups, requires running
`shipyard` as root. Otherwise it fails before starting, and says what differs:
for example `-u 501` alone may ask for group `0`, so use `-u 501:20` to keep
your own group.

When running as root, the command gets **only** the groups you ask for; root's
own supplementary groups are always dropped.

## Resource limits

```sh
shipyard run --ulimit nofile=1024:2048 --ulimit core=0 my-server
```

`--ulimit NAME=SOFT[:HARD]` sets a limit:

- **Soft limit**: the limit the kernel enforces.
- **Hard limit**: the ceiling the command may raise its soft limit to. If you give
  only a soft value, the hard limit is set to the same value.
- **Values:** a number, `unlimited`, or `-1` (also unlimited).
- **The soft limit can't be above the hard limit.**
- **Repeats:** if you set the same limit twice, the last one wins.

Names are case-insensitive, and accept both Docker style (`nofile`) and OCI style
(`RLIMIT_NOFILE`):

| Name | Limits | Platforms |
|---|---|---|
| `as` | address space (virtual memory), bytes | all |
| `core` | core dump size, bytes (`0` disables core dumps) | all |
| `cpu` | CPU time, seconds | all |
| `data` | data segment (heap), bytes | all |
| `fsize` | largest file created, bytes | all |
| `memlock` | memory locked into RAM, bytes | all |
| `nofile` | open file descriptors | all |
| `nproc` | processes for the user | all |
| `stack` | main thread stack, bytes | all |
| `locks` | file locks | Linux |
| `msgqueue` | POSIX message queue bytes | Linux |
| `nice` | ceiling for raising priority | Linux |
| `rss` | resident memory, bytes (advisory) | Linux |
| `rtprio` | real-time priority ceiling | Linux |
| `rttime` | real-time CPU time, microseconds | Linux |
| `sigpending` | queued signals | Linux |

On macOS, the largest finite value is `9223372036854775806` (2⁶³ − 2). `rss` is
Linux-only because macOS treats it as another name for `as`.

`nofile=unlimited` on macOS: recent versions accept it as is. Older ones refuse
an unlimited soft limit, and the command then gets the most the system allows
per process (`sysctl kern.maxfilesperproc`). On Linux, `unlimited` is refused before
anything starts (exit 125), because the kernel caps `nofile` at `fs.nr_open`;
use a number instead.

## Inherited state

The command starts clean, whatever state Shipyard was started in:

- **Signals:** every signal is back to its default action and none are blocked.
  (Shipyard itself ignores `SIGPIPE`, as Rust programs do; the command doesn't.)
  The one exception: if Shipyard was started ignoring `SIGHUP`, `SIGINT` or
  `SIGQUIT`, the command keeps ignoring it. That's how `nohup shipyard run …`
  and `shipyard run … &` in a script keep protecting the command from hangups
  and Ctrl-C.
- **File descriptors:** only stdin, stdout and stderr are passed on. Any other
  open descriptor is closed when the command starts.
- **umask:** `022`.
- **`TERM`:** when Shipyard runs in a terminal, your `TERM` is passed on (unless
  you set one with `-e`), so full-screen programs like `top` and `vim` work.
- **On Linux:** if Shipyard is killed with `SIGKILL`, the command is killed too.
  (On macOS it can be left running in that one case.) Isolated, the whole
  container goes. With `--no-isolation`, one exception: a command run as
  another user (`--user`) that then starts a setuid program loses that
  protection (the system clears it for such programs), and the program can be
  left running.

## Signals and stopping

Shipyard stays with the command until it has exited, and passes signals on.

Isolated commands on a terminal behave differently: see
[On a terminal, isolated](#on-a-terminal-isolated). This section describes
`--no-isolation` (and macOS) on a terminal, and every run without one.

**From the terminal.** Ctrl-C, Ctrl-\\ and Ctrl-Z reach the command directly,
just as without Shipyard, and `fg`/`bg` work as usual. Shipyard doesn't pass
them on a second time. If Ctrl-C ends the command, Shipyard also stops as if
interrupted, so a shell loop around `shipyard run` stops too (unless Shipyard
itself was started ignoring Ctrl-C).

**Stopped jobs.** If the command stops itself (for example with
`kill -STOP $$`, or under a debugger) rather than through Ctrl-Z, Shipyard stops
too, so your shell shows the job as stopped instead of the terminal seeming to
hang. Some shells then say "stopped (signal)" rather than "suspended". `fg`
resumes both. If only Shipyard is resumed (`kill -CONT <shipyard>`), it resumes
the command as well.

Shipyard doesn't stop itself when nothing could resume it:

- **while it's stopping the command** (after `SIGTERM`): it has to stay awake
  to kill the command when `--stop-timeout` runs out;
- **when no job-control shell is above it** (the shell that started it has
  exited, or under `ssh -t host cmd`, `script`, or an IDE's run pane).

A job stopped with Ctrl-Z ignores `kill <shipyard>` until it's resumed, like
any stopped program. The `--stop-timeout` clock keeps running while it's
stopped.

**Sent to Shipyard.** A signal sent to Shipyard's process ID (`kill`, `timeout`,
a CI runner, a service manager) is passed on to the command, with these
exceptions:

- **`SIGTERM` stops the command gracefully, like `docker stop`:** the command
  gets the `--stop-signal`, and if it hasn't exited after `--stop-timeout`, it's
  killed with `SIGKILL`.
- **Not passed on:**
  - job-control signals (`SIGTSTP`, `SIGCONT`, …), which the shell handles for
    both;
  - signals about Shipyard's own state (crashes, `SIGPIPE`, `SIGCHLD`).
- **Signals Shipyard was started ignoring** (for example under `nohup`) stay
  ignored and aren't passed on.
- **Signals 32 and 33** (Linux): the C library keeps them for itself, so
  Shipyard can't catch them, and either one ends Shipyard at once (and an
  isolated command with it). Don't send them to Shipyard.
- **Signals the command sends to Shipyard** aren't bounced back to it. (On
  macOS this is best effort: if two signals arrive at the same instant, the
  system can report the wrong sender for one of them.)
- **On macOS, in a terminal:** while Shipyard runs in the foreground, the
  signals a terminal can send (`SIGINT`, `SIGQUIT`, `SIGHUP`, `SIGWINCH`,
  `SIGINFO`) aren't passed on even when sent with `kill`. macOS reports a
  terminal's Ctrl-C exactly like a `kill`, and the terminal has already
  delivered it to the command. Send them to the command itself instead, or use
  `SIGTERM`. Linux tells the two apart, so this doesn't apply there.

Shipyard catches signals even if the program that started it had blocked
them, and catches `SIGTERM` even if it was started ignoring it. So `SIGTERM`
always stops it, and it always notices when the command exits.

A signal sent to Shipyard's whole *process group* (as `timeout` does, and
`kill -SIG -<pgid>`) reaches the command twice when Shipyard runs in a terminal
without isolation: once directly and once passed on. (An isolated command isn't
in Shipyard's group, so it gets it once.) A signal that arrives before the command has
finished starting gets the command's default reaction (for most signals, it
exits).

| Option | Default | Meaning |
|---|---|---|
| `--stop-signal SIGNAL` | `SIGTERM` | Signal sent to ask the command to exit |
| `--stop-timeout SECONDS` | `10` | How long to wait before forcing it with `SIGKILL` |

The stop signal can't be `SIGSTOP`, `SIGTSTP`, `SIGTTIN`, `SIGTTOU` or `SIGCONT`:
those pause or resume the command instead of ending it. Nor can it be `SIGCHLD`
or signals 32 and 33, which the container's init can't pass on.

Signals can be given as:

- a number: `15`
- a name, with or without the `SIG` prefix and in any case: `TERM`, `SIGTERM`, `sigterm`
- on Linux, a real-time signal: `RTMIN` (34), `RTMIN+3`, `RTMAX-2`, `RTMAX` (64).
  These use the kernel's numbers, the same as Docker, so `RTMIN+3` is always
  signal 37.

## Cleanup

When the command exits, Shipyard cleans up what it left behind:

- **Isolated** (Linux): when the command exits, the container's init exits and
  the system kills everything else left in the container.
- **Without a terminal** (CI, services, scripts), and always when isolated: the
  command runs in its own process group, and anything still running in that
  group is killed.
- **On Linux:** every leftover process is killed, even ones that left the
  command's group (for example with `setsid`). Shipyard registers as their parent
  (a "child subreaper") to find them. Processes that were already Shipyard's
  children when it started (as in `helper & exec shipyard run app`) aren't the
  command's, and are left running.
- **On macOS, from a terminal:** background processes the command started can be
  left running. Full isolation (a later version) removes this gap.
- **Laptop sleep:** if the computer sleeps during `--stop-timeout`, the sleep
  doesn't count toward the timeout.

## Exit codes

See [Exit codes](exit-codes.md). In short: `run` returns the command's own exit
code, `128 + N` if signal N killed it, and `125`–`127` if it never ran.

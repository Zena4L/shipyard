# Running commands

```sh
shipyard run [OPTIONS] COMMAND [ARGS...]
```

`shipyard run` starts a command the way a container runtime does: with a clean
environment and exactly the user, working directory and limits you ask for.

> **Status:** `run` starts the command with exactly the settings below and
> reports how it ended. It doesn't isolate it yet: the command sees the host's
> filesystem, processes and network. Isolation (namespaces, a separate
> filesystem) comes in a later version.

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

## Environment

The command does **not** inherit your shell's environment. It gets only:

1. the variables you set with `-e/--env`, and
2. defaults for anything you didn't set:

| Variable | Default |
|---|---|
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` |
| `HOME` | `/root` when running as user 0, otherwise `/` |

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
| `-u UID` | Run as user `UID`, with primary group `0`. |
| `-u UID:GID` | Run as user `UID`, with primary group `GID`. |
| `--group-add GID` | Add a supplementary group. Repeatable; duplicates are ignored. |

IDs must be numbers. User and group *names* such as `nobody` aren't supported yet,
because names are looked up in the container's own `/etc/passwd`.

These are rejected:

- **ID `4294967295`.** The kernel reads it as "leave unchanged", so allowing it
  could silently leave the command running as root.
- **Too many supplementary groups.** The limit is 15 on macOS (the system's 16
  slots include the primary group) and 65536 on Linux.

Switching to another user or group, or adding groups, requires running
`shipyard` as root. Otherwise it fails before starting, and says what differs:
for example `-u 501` alone asks for group `0`, so use `-u 501:20` to keep your
own group.

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
per process (`sysctl kern.maxfilesperproc`). On Linux, `unlimited` is always refused,
because the kernel caps `nofile` at `fs.nr_open`; use a number instead.

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
  (On macOS it can be left running in that one case.)

## Signals and stopping

Shipyard stays with the command until it has exited, and passes signals on.

**From the terminal.** Ctrl-C, Ctrl-\\ and Ctrl-Z reach the command directly,
just as without Shipyard, and `fg`/`bg` work as usual. Shipyard doesn't pass
them on a second time. If Ctrl-C ends the command, Shipyard also stops as if
interrupted, so a shell loop around `shipyard run` stops too (unless Shipyard
itself was started ignoring Ctrl-C).

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
them, so it can always be stopped and always notices when the command exits.

A signal sent to Shipyard's whole *process group* (as `timeout` does, and
`kill -SIG -<pgid>`) reaches the command twice when Shipyard runs in a terminal:
once directly and once passed on. A signal that arrives before the command has
finished starting gets the command's default reaction (for most signals, it
exits).

| Option | Default | Meaning |
|---|---|---|
| `--stop-signal SIGNAL` | `SIGTERM` | Signal sent to ask the command to exit |
| `--stop-timeout SECONDS` | `10` | How long to wait before forcing it with `SIGKILL` |

The stop signal can't be `SIGSTOP`, `SIGTSTP`, `SIGTTIN`, `SIGTTOU` or `SIGCONT`:
those pause or resume the command instead of ending it.

Signals can be given as:

- a number: `15`
- a name, with or without the `SIG` prefix and in any case: `TERM`, `SIGTERM`, `sigterm`
- on Linux, a real-time signal: `RTMIN` (34), `RTMIN+3`, `RTMAX-2`, `RTMAX` (64).
  These use the kernel's numbers, the same as Docker, so `RTMIN+3` is always
  signal 37.

## Cleanup

When the command exits, Shipyard cleans up what it left behind:

- **Without a terminal** (CI, services, scripts): the command runs in its own
  process group, and anything still running in that group is killed.
- **On Linux:** every leftover process is killed, even ones that left the
  command's group (for example with `setsid`). Shipyard registers as their parent
  (a "child subreaper") to find them.
- **On macOS, from a terminal:** background processes the command started can be
  left running. Full isolation (a later version) removes this gap.
- **Laptop sleep:** if the computer sleeps during `--stop-timeout`, the sleep
  doesn't count toward the timeout.

## Exit codes

See [Exit codes](exit-codes.md). In short: `run` returns the command's own exit
code, `128 + N` if signal N killed it, and `125`–`127` if it never ran.

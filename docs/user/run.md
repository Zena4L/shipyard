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

## Inherited state

The command starts clean, whatever state Shipyard was started in:

- **Signals:** every signal is back to its default action and none are blocked.
  (Shipyard itself ignores `SIGPIPE`, as Rust programs do; the command doesn't.)
- **File descriptors:** only stdin, stdout and stderr are passed on. Any other
  open descriptor is closed when the command starts.
- **umask:** `022`.
- **On Linux:** if Shipyard is killed, the command is killed too.

## Stopping

**Ctrl-C** and **Ctrl-\\** go straight to the command, which handles them as it
would without Shipyard. If Ctrl-C ends the command, Shipyard also stops as if
interrupted, so a shell loop around `shipyard run` stops too.

> **Not yet supported:** signals sent to Shipyard's own process ID (for example
> `kill -INT <pid>`, or `timeout -s INT`) aren't passed on to the command yet, and
> on macOS killing Shipyard with `SIGTERM` leaves the command running. Signal
> forwarding comes in the next version. `--stop-signal` and `--stop-timeout`
> are accepted now and take effect then.

| Option | Default | Meaning |
|---|---|---|
| `--stop-signal SIGNAL` | `SIGTERM` | Signal sent to ask the command to exit |
| `--stop-timeout SECONDS` | `10` | How long to wait before forcing it with `SIGKILL` |

Signals can be given as:

- a number: `15`
- a name, with or without the `SIG` prefix and in any case: `TERM`, `SIGTERM`, `sigterm`
- on Linux, a real-time signal: `RTMIN` (34), `RTMIN+3`, `RTMAX-2`, `RTMAX` (64).
  These use the kernel's numbers, the same as Docker, so `RTMIN+3` is always
  signal 37.

## Exit codes

See [Exit codes](exit-codes.md). In short: `run` returns the command's own exit
code, `128 + N` if signal N killed it, and `125`–`127` if it never ran.

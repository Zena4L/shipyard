# The OCI runtime: `shipyard-runtime`

`shipyard run --bundle` runs a container in one go. `shipyard-runtime` runs one in
steps, the way the [OCI runtime command line](https://github.com/opencontainers/runtime-tools/blob/master/docs/command-line-interface.md)
describes and `runc` implements. Container engines and the OCI conformance tests drive
a runtime this way, and you can too:

```sh
sudo shipyard-runtime create --bundle ./mybundle web   # set up, not running yet
sudo shipyard-runtime state web                        # "status": "created"
sudo shipyard-runtime start web                        # its program runs
sudo shipyard-runtime kill web TERM                    # ask it to stop
sudo shipyard-runtime delete web                       # remove it once stopped
```

The bundle is the same as for `shipyard run --bundle`; see [OCI bundles](bundles.md)
for what its `config.json` may contain, with one more requirement: a `pid` namespace.
Without one, the runtime couldn't find or stop all of the container's processes (that
needs cgroups, which come later), so `create` refuses the bundle. What the bundle asks
for that is ignored is said on stderr (`shipyard-runtime: warning: …`) and in the
`--log`. Like any isolated run, it needs root and Linux. On macOS, `shipyard-runtime`
says so and exits.

## The commands

| Command | What it does |
|---|---|
| `create [--bundle DIR] [--pid-file FILE] ID` | Sets the container up from the bundle in DIR (default: the current directory) and stops just before its program runs. Returns once it's *created*. `--pid-file` writes the PID of its init (its PID 1, as seen from the host); if it can't be written, `create` fails and the container is removed. |
| `start ID` | Lets a *created* container's program run. |
| `state ID` | Prints the container's state as JSON (below). |
| `kill ID [SIGNAL]` | Sends a signal (default `TERM`; a name like `KILL` or `SIGKILL`, or a number) to a *created* or *running* container. Not `STOP` (see below). |
| `delete [--force] ID` | Removes a *stopped* container. A *created* one is killed first. A *running* one only with `--force`, which kills it first. |

Every command takes `--root DIR`: where the containers' state is kept (default
`/run/shipyard`). `--log FILE` and `--log-format text|json` also write errors and
warnings to FILE, as `runc`'s callers expect.

A command that fails prints `shipyard-runtime: …` and exits with code 1. A command
line it can't parse (an unknown option, a missing ID, a signal it doesn't know)
prints `error: …` and the usage, and exits with code 2, before anything is logged.

## A container's life

```
create ──► created ──start──► running ──(it exits, or kill)──► stopped ──delete──► gone
              └─────────────────── delete (kills it) ─────────────────────┘
```

`state` prints:

```json
{
  "ociVersion": "1.2.0",
  "id": "web",
  "status": "running",
  "pid": 41237,
  "bundle": "/home/me/mybundle",
  "annotations": { "org.example.owner": "me" }
}
```

- **`status`** is one of `creating`, `created`, `running` and `stopped`.
- **`pid`** is the container's init, while it's created or running. A PID the system
  has since given to another process is never reported as the container's: Shipyard
  checks that it's still the same process.

## How it works

Shipyard has no daemon. `create` starts a small **monitor** process for the container
(`shipyard-monitor` in `ps -f`):

- **It stays the container's parent** after `create` returns.
- **It records how the container ended,** for `state`.
- **It takes the container down with it if it's killed.** So don't kill a monitor
  unless you mean to stop its container.
- **It runs in a session of its own:** closing the terminal `create` ran in doesn't
  affect it, and neither does a Ctrl-C meant for `create`.
- **It keeps nothing else of `create`'s:** files or pipes `create` was given (other
  than its standard input, output and error) are closed. A program reading such a
  pipe sees its end when `create` exits.

The container's **standard input, output and error are `create`'s**. Redirect them
when you create it:

```sh
sudo shipyard-runtime create --bundle ./mybundle web > web.log 2>&1
```

**Created from a terminal, a container keeps that terminal.** Its program writes to
it, and reads from it, after `create` and `start` have returned and your shell has the
terminal back: you and the container then take turns at the keys you type. Redirect
its input too (`< /dev/null`) unless it should read the terminal.

Until `start`, the container is fully set up: its namespaces, root filesystem and
mounts, user and limits. Only its program hasn't run. Any problem with the setup
makes `create` fail, and leaves nothing behind.

**After `start`, problems go to the container's own stderr.** A program that can't
be found then, for example, writes `shipyard: … command not found` there, and the
container stops with exit code 127.

## The state directory

Each container has a directory `ROOT/ID` (only root can read it):

| File | What it is |
|---|---|
| `state.json` | The bundle, the init's PID and the annotations. |
| `exec.fifo` | Where `start` signals the waiting container; removed once started. |
| `exit` | How the container ended, once it has. |

Commands on the same container take turns: each locks the directory while it reads
and changes it. `create` holds the lock only while it fills the directory in, not while
the container is set up. Meanwhile `state` says `creating`, and `delete` can remove a
container whose setup hangs: it stops the setup too. A command that waits more than 10
seconds for the lock (another one stopped while holding it, say) gives up: `container
ID is busy`.

A container deleted and created again at once is the new one: the old container's
monitor writes nothing into the new one's directory.

**The state root must be yours alone:** a real directory (not a symbolic link), owned
by the user running the command (root, for `create`), that nobody else can write to.
Otherwise every command refuses it, since another user could plant files where the
runtime reads and deletes.

**IDs** start with a letter or digit, then letters, digits, `_`, `.`, `+` or `-`, at
most 255 characters.

## Differences from runc

- **Signals while *created*.** Other than `KILL`, they're held until the container is
  started, then delivered. With `runc`, a `TERM` kills a created container. To get rid
  of a created container, use `delete` or `kill ID KILL`.
- **`delete` of a *created* container** kills it, as `runc` does. The OCI
  specification says deleting a container that isn't stopped must fail.
- **`kill ID STOP` is refused.** `STOP` can't be caught or passed on, so it would stop
  the container's init, not its program. Send `TSTP` to stop the program (most programs
  stop on it); pausing a whole container comes with cgroups.
- **A `pid` namespace is required** (see above).
- **Not yet:**
  - terminals (`--console-socket`);
  - `exec`, `pause` and `resume`;
  - `list`, `ps` and `run`.

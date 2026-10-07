# The OCI runtime: `shipyard-runtime`

`shipyard run --bundle` runs a container in one go. `shipyard-runtime` runs one in
steps, the way the [OCI runtime command line](https://github.com/opencontainers/runtime-tools/blob/master/docs/command-line-interface.md)
describes and `runc` implements. Container engines and the OCI conformance tests drive
a runtime this way, and you can too:

```sh
shipyard-runtime create --bundle ./mybundle web   # set up, not running yet
shipyard-runtime state web                        # "status": "created"
shipyard-runtime start web                        # its program runs
shipyard-runtime kill web TERM                    # ask it to stop
shipyard-runtime delete web                       # remove it once stopped
```

The bundle is the same as for `shipyard run --bundle`; see [OCI bundles](bundles.md)
for what its `config.json` may contain. What the bundle asks
for that is ignored is said on stderr (`shipyard-runtime: warning: …`) and in the
`--log`. It needs Linux: on macOS, `shipyard-runtime` says so and exits.

**Without root, containers are rootless**, as with `shipyard run`: the bundle runs in
a user namespace (added if it has none, with a warning). See
[Rootless bundles](bundles.md#rootless-bundles).

**A container has a cgroup of its own,** `/sys/fs/cgroup/shipyard/runtime/ID` as root,
`shipyard/runtime/ID` below the cgroup subtree delegated to you rootless, or where the
bundle's `linux.cgroupsPath` says (see [Cgroups](bundles.md#cgroups)). It's
made by `create`, with the bundle's `linux.resources` limits written in before anything
runs there, recorded in the state, and removed when the container ends; `delete`
removes it too, killing anything left in it. A cgroup that exists already is never
taken over: with the default path, an ID can be used by one container at a time across
every `--root`, and a second one fails with "the cgroup … exists already". A container
the kernel's OOM killer ended has "(out of memory)" in its exit record; one ended by
`kill … KILL` or `delete` doesn't, though the OOM killer struck one of its processes
before.

**A rootless container lives below your systemd user manager** (`user@UID.service`), so
it ends when that does: at your last logout, unless lingering keeps it running
(`loginctl enable-linger`). `delete` still works afterwards, and from any of your
sessions, an SSH login included: the state records where its cgroup was.

**A bundle without a `pid` namespace** needs that cgroup: without either, the runtime
couldn't find or stop all of the container's processes, so `create` refuses it where
the container can't have one (saying why). Rootless, a bundle with a root filesystem
needs a `pid` namespace anyway (see [Cgroups](bundles.md#cgroups)). With the cgroup, `kill` signals the
container's first process, as `runc` does, and `delete --force` ends every process
in the container.

## The commands

| Command | What it does |
|---|---|
| `create [--bundle DIR] [--pid-file FILE] ID` | Sets the container up from the bundle in DIR (default: the current directory) and stops just before its program runs. Returns once it's *created*. `--pid-file` writes the PID of its init (its PID 1, as seen from the host); if it can't be written, `create` fails and the container is removed. |
| `start ID` | Lets a *created* container's program run. |
| `state ID` | Prints the container's state as JSON (below). |
| `kill [--all] ID [SIGNAL]` | Sends a signal (default `TERM`; a name like `KILL` or `SIGKILL`, or a number) to a *created*, *running* or *paused* container: to its init, which passes it on to the program, or with `--all` (`-a`) to every process in its cgroup. Not `STOP` (see below). |
| `delete [--force] ID` | Removes a *stopped* container. A *created* one is killed first. A *running* or *paused* one only with `--force`, which kills it first. |
| `pause ID` | Freezes every process of a *created* or *running* container (`cgroup.freeze`): it's *paused* until `resume`. |
| `resume ID` | Lets a *paused* container run again: it's *created* or *running*, as it was before. |
| `update [-r FILE \| FLAGS] ID` | Changes a *created*, *running* or *paused* container's limits, as given; the rest stay. The limits come from a `linux.resources` JSON object (`-r FILE`, `-` for stdin), read as a bundle's, or from `shipyard run`'s limit flags (`--memory`, `--cpus`, `--pids-limit`, …; runc's `--cpu-share` too). Unlike `shipyard run`, `--memory` alone leaves swap as it is; `--memory-swap` alone (memory plus swap) counts from the container's memory limit. |
| `ps [--format table\|json] ID` | Lists the processes in the container's cgroup, by host PID: a table (PID, user ID, command), or a JSON array of PIDs. |
| `events [--stats] [--interval DURATION] ID` | Prints the container's use as JSON lines, runc's format: `{"type":"stats","id":…,"data":{"cpu":…,"memory":…,"pids":…,"blkio":…}}` every `--interval` (default `5s`; `500ms`, `1m`), and `{"type":"oom","id":…}` when the OOM killer killed a process in it since the last. Ends when the container stops (or is deleted). `--stats` prints once. |

Every command takes `--root DIR`: where the containers' state is kept. The default is
`/run/shipyard` for root, and `$XDG_RUNTIME_DIR/shipyard` for anyone else (as with
runc and Podman: a directory of yours that your login session creates, and removes when
you log out). Without `XDG_RUNTIME_DIR` (after `su`, say), or with one that isn't an
absolute path to a directory of yours, give `--root`: the command says so rather than
guess a place. So `sudo shipyard-runtime …` and
`shipyard-runtime …` keep separate state: each sees only its own containers ("no
container with ID …" from the other). `--log FILE` and `--log-format text|json` also
write errors and warnings to FILE, as `runc`'s callers expect.

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

- **`status`** is one of `creating`, `created`, `running`, `paused` and `stopped`.
- **`pid`** is the container's init, while it's created, running or paused. A PID the system
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
shipyard-runtime create --bundle ./mybundle web > web.log 2>&1
```

**Created from a terminal, a container keeps that terminal.** Its program writes to
it, and reads from it, after `create` and `start` have returned and your shell has the
terminal back: you and the container then take turns at the keys you type. Redirect
its input too (`< /dev/null`) unless it should read the terminal.

Until `start`, the container is fully set up: its namespaces, root filesystem and
mounts, user and limits. Only its program hasn't run. Any problem with the setup
makes `create` fail, and leaves nothing behind.

**A bundle's hooks run at their phases** (see [Hooks](bundles.md#hooks)):
`prestart`, `createRuntime` and `createContainer` during `create`, before it returns;
`startContainer` in the container once `start` lets it go; `poststart` from `start`,
after that; `poststop` from `delete`, once the container is gone (and from a `create`
that fails). They're the ones `create` read: changing the bundle afterwards changes
nothing. A failing `poststart` or `poststop` hook is a warning
(`shipyard-runtime: warning: the poststart hook …`), and the command still succeeds.
`start` and `delete` run them without holding the container's lock, so a hook can
call `state`.

**A container's device rules** (see [Devices](run.md#devices)) are attached to its
cgroup by `create`, before anything runs in it, so they apply to its init and to the
hooks it runs (`createContainer`, `startContainer`) as well as to its program. `update`
doesn't change them (`runc`'s doesn't either).

Deleting a container while `create` is still running its hooks stops those hooks too,
before `poststop` runs.

**After `start`, problems go to the container's own stderr.** A program that can't
be found then, for example, writes `shipyard: … command not found` there, and the
container stops with exit code 127.

## The state directory

Each container has a directory `ROOT/ID` (only its owner can read it):

| File | What it is |
|---|---|
| `state.json` | The bundle, the init's PID, the annotations, the cgroup and the hooks. |
| `exec.fifo` | Where `start` signals the waiting container; removed once started. |
| `exit` | How the container ended, once it has. |

Commands on the same container take turns: each locks the directory while it reads
and changes it. `create` holds the lock only while it fills the directory in, not while
the container is set up. Meanwhile `state` says `creating`, and `delete` can remove a
container whose setup hangs: it stops the setup too. A command that waits more than 15
seconds for the lock (another one stopped while holding it, say) gives up: `container
ID is busy`.

A container deleted and created again at once is the new one: the old container's
monitor writes nothing into the new one's directory.

**The state root must be yours alone:** a real directory (not a symbolic link), owned
by the user running the command, that nobody else can write to.
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
  the container's init, not its program. Use `pause` to stop a whole container, or send
  `TSTP` to stop the program (most programs stop on it).
- **A paused container** takes `kill` (`KILL` ends it at once; other signals wait until
  `resume`) and `delete --force`; `start` and a second `pause` are refused.
- **The commands that use the cgroup** (`pause`, `resume`, `update`, `ps`, `events`,
  `kill --all`) need the container to have one: where it has none (rootless without a
  delegated subtree, cgroup v1, a read-only tree), they're refused, saying so.
- **`startContainer` hooks run after `start` returns,** in the container, before its
  program: `poststart` may run while they still do. A failing `startContainer` hook
  doesn't fail `start`; the container ends with exit code 125 instead, the hook's
  failure on its standard error. (`runc` runs them before `start` returns.)
  They run as the container's root with no capability but `CAP_KILL`, not as
  `process.user` with its capabilities: their programs are the image's (see
  [Hooks](bundles.md#hooks)).
- **A `create` that fails runs `poststop`,** as the specification says (once the
  container has been destroyed), and adds each `poststop` failure to its error as a
  `warning:` line.
- **`ps`'s table** is read from `/proc` by Shipyard itself, without the host's `ps`;
  arguments for `ps` aren't taken. `--format json` is runc's.
- **`events`** reads the OOM count every `--interval`: an OOM kill that ends the
  container shows in its exit record instead.
- **A `pid` namespace is required** where the container can't have a cgroup, and
  rootless (see above).
- **Not yet:**
  - terminals (`--console-socket`);
  - `exec`;
  - `list` and `run`.

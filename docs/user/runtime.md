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

**Conformance.** The OCI runtime conformance suite
([runtime-tools](https://github.com/opencontainers/runtime-tools)) is part of
`shipyard-runtime`'s tests. Each of its tests passes, or fails for a stated reason:
- a field Shipyard refuses rather than half-applies (see
  [What's refused](bundles.md#whats-refused));
- runc's behaviour where the suite expects otherwise;
- a bug in the test itself;
- or a known gap still to fix.

A few aren't run: the cgroup tests read cgroup v1 only (Shipyard is cgroup v2), and
some hang or race in the suite itself. The list, test by test, is
[`scripts/conformance/expected.txt`](https://github.com/Zena4L/shipyard/blob/main/scripts/conformance/expected.txt).

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
every `--root`, and a second one fails with "the cgroup … exists already". A `create`
killed while it made the cgroup leaves it behind, noted in its state: `delete` of that
container removes it if it's empty (it never kills what's there), and the ID can be
created again. A container
the kernel's OOM killer ended has "(out of memory)" in its exit record ("(the host or a
parent cgroup ran out of memory)" if it never reached its own limit); one ended by
`kill … KILL` or `delete` doesn't, though the OOM killer struck one of its processes
before.

**A rootless container's monitor stays where its maker ran**: in your login session's
scope (an SSH login), or below your systemd user manager (`user@UID.service`, under
`systemd-run --user --scope`). Made in a login session, at logout it runs on with
logind's `KillUserProcesses=no` (as on Ubuntu 24.04): its monitor keeps your session
from closing, and with it your user manager and `$XDG_RUNTIME_DIR`. With
`KillUserProcesses=yes`, logind ends the session's processes at logout, the monitor
among them (and its container with it). Made under your user manager, it lasts as
long as that does: past your last logout only with linger (`loginctl enable-linger`). `delete` still works afterwards, and from any of your
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
| `create [--bundle DIR] [--pid-file FILE] [--console-socket SOCKET] [--log-dir DIR [--log-max-size SIZE] [--log-max-files N] [--stdin [--stdin-once]]] [--preserve-fds N] [--hold-fd N[=LABEL]]… [--exit-command PROGRAM [--exit-command-arg ARG]…] [--companion PROGRAM [--companion-arg ARG]…] ID` | Sets the container up from the bundle in DIR (default: the current directory) and stops just before its program runs. Returns once it's *created*. `--pid-file` writes the PID of its init (its PID 1, as seen from the host); if it can't be written, `create` fails and the container is removed. `--console-socket`: see [Terminals](#terminals); `--log-dir`: see [Logs](#logs); `--hold-fd`, `--exit-command` and `--companion`: see [Holds and an exit command](#holds-and-an-exit-command). |
| `logs [--follow] [--tail N] [--since TIME] [--timestamps] ID` | Prints a container's logs (one created with `--log-dir`; see [Logs](#logs)). |
| `attach [--detach-keys KEYS] [--no-stdin] ID` | Attaches to a container created with `--log-dir`: its output from now on, and your typing (see [Attach](#attach)). |
| `exec [-d] [-t] [--console-socket SOCK] [-p FILE] [--cwd DIR] [-e K=V]… [-u UID[:GID]] [-g GID]… [-c CAP]… [--no-new-privs] [--pid-file FILE] ID [COMMAND…]` | Runs a command inside a *created* or *running* container, confined as its own process is (see [Exec](#exec)). |
| `cp [-a] [-L] SRC DEST` | Copies files into or out of a *created*, *running* or *paused* container: `ID:PATH` on one side, a host path or `-` (a tar stream) on the other (see [Copying files](#copying-files)). |
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

A command that fails prints `shipyard-runtime: …` and exits with code 1 (`exec` and
`attach`: with 255, see [Exec](#exec) and [Attach](#attach)). A command
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
  Between the container's program ending and its monitor having cleaned up (its cgroup
  removed, its output written to the logs), usually a fraction of a second and a few
  seconds at most, it's *stopping*: `state` says `stopped` (OCI's word for a container
  whose process has exited), and the commands that need it running (`exec`, `cp`, `kill`,
  `pause`, `resume`, `update`, `ps`, `events`) refuse it as stopping (`container ID is
  stopping`). `delete` removes it at once, finishing the cleanup itself.
- **`pid`** is the container's init, while it's created, running or paused. A PID the system
  has since given to another process is never reported as the container's: Shipyard
  checks that it's still the same process.

## How it works

Shipyard has no daemon. `create` starts a small **monitor** process for the container
(`shipyard-monitor` in `ps -f`):

- **It stays the container's parent** after `create` returns.
- **It's nobody's child:** it's forked once more as it starts, so `create` leaves no
  child behind (a program that creates containers and lives on, say an API server,
  collects no zombie when a monitor ends). It's adopted by init, or by the nearest
  subreaper (`systemd --user`, say).
- **It records how the container ended,** for `state`.
- **It takes the container down with it if it's killed.** So don't kill a monitor
  unless you mean to stop its container. (A process that escaped the container's
  init, in a bundle without a `pid` namespace, lives on; with logs, its next write
  fails with a broken pipe, as nothing reads its output any more.)
- **With `--log-dir`, it keeps the container's output** in logs (see [Logs](#logs)).
- **It runs in a session of its own:** closing the terminal `create` ran in doesn't
  affect it, and neither does a Ctrl-C meant for `create`.
- **It keeps nothing else of `create`'s:** files or pipes `create` was given (other
  than its standard input, output and error, and those passed on with
  `--preserve-fds` or socket activation: see
  [Passing descriptors](#passing-descriptors)) are closed. A program reading such a
  pipe sees its end when `create` exits.

The container's **standard input, output and error are `create`'s** (unless its
output goes into logs: see [Logs](#logs)). Redirect them when you create it:

```sh
shipyard-runtime create --bundle ./mybundle web > web.log 2>&1
```

**Created from a terminal, a container keeps that terminal** (without `--log-dir`). Its program writes to
it, and reads from it, after `create` and `start` have returned and your shell has the
terminal back: you and the container then take turns at the keys you type. Redirect
its input too (`< /dev/null`) unless it should read the terminal.

Until `start`, the container is fully set up: its namespaces, root filesystem and
mounts, user and limits. Only its program hasn't run. Any problem with the setup
makes `create` fail, and leaves nothing behind.

**A bundle's hooks run at their phases** (see [Hooks](bundles.md#hooks)):
`prestart`, `createRuntime` and `createContainer` during `create`, before it returns;
`startContainer` in the container once `start` lets it go; `poststart` from `start`,
once the program has started (after the `startContainer` hooks: `start` waits for the
program's `execve`, up to 5 seconds plus those hooks' timeouts, then runs them anyway
with a warning; a `startContainer` hook without a timeout can make it run them before
the program starts); `poststop` from `delete`, once the container is gone (and from a `create`
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

## Terminals

A bundle that asks for a terminal (`process.terminal: true`, as every config `runc spec`
writes does) gets one the way `runc` gives it, which is what containerd's and Podman's
shims expect (or, with `--log-dir`, its terminal's output goes into the logs: see
[Logs](#logs)):

```sh
shipyard-runtime create --bundle ./mybundle --console-socket /run/my-shim/console.sock web
```

- **You listen on a UNIX socket** and name it with `--console-socket`.
  `shipyard-runtime` connects to it, as you.
- **The container's terminal is made inside it,** from its own `/dev/pts`, during
  `create`.
- **Its master is sent to the socket** as a passed file descriptor (`SCM_RIGHTS`),
  with a name as the message's bytes (`/dev/ptmx`; `runc` sends the path it opened,
  which receivers may use as the file's name and otherwise ignore). Nothing is
  awaited back; the connection is closed once it's sent. Whatever is written to the
  master is typed into the container, and its output comes out of it. Resize it
  with `TIOCSWINSZ`.
- **The terminal is the program's standard input, output and error,** and its
  controlling terminal, from `start`. `create`'s own standard input, output and
  error aren't the container's: they reach their end when `create` returns. A
  failure after `start` (a program that can't be executed, say) is explained on the
  terminal.
- **Its size** is `process.consoleSize` (`height` rows, `width` columns), or 24 by
  80 without one (`runc` leaves it 0 by 0, which full-screen programs can't draw
  in).

A socket path longer than a socket address can hold (108 bytes) works too: it's
reached through its directory.

These are refused, before anything is made:

- a bundle that asks for a terminal, without `--console-socket` or `--log-dir`
  ("there's no console socket to send it to");
- `--console-socket` with `--log-dir` ("a console socket and a log directory both");
- `--console-socket` for a bundle that asks for none ("the bundle asks for no
  terminal");
- a `--console-socket` path that isn't a socket ("not a socket"), or that nothing
  listens on.

## Logs

Instead of `create`'s own standard output and error, a container's output can be kept
in logs, which `logs` reads back:

```sh
shipyard-runtime create --bundle ./mybundle --log-dir /var/lib/myengine/web-logs web
shipyard-runtime start web
shipyard-runtime logs --follow web
```

- **`--log-dir DIR` turns it on.** `create` makes DIR, so it must not exist yet (its
  parent must): no two containers ever share one, and nothing else can be in it. It's
  yours alone (mode 0700, its files 0600). **The logs outlive the container:** `delete`
  leaves them, and `logs --log-dir DIR` reads them after it. Removing them is yours to
  do. Keep DIR where the container can't reach it (not inside its root filesystem or
  its mounts).
- **What the container gets:** its standard output and error go to the logs, not to
  `create`'s, which reach their end when `create` returns. Its standard input is
  empty (`/dev/null`), unless `--stdin` keeps it open for `attach` (see
  [Attach](#attach)). A bundle that asks for a terminal needs no console socket: the
  terminal's output goes into the logs, as its standard output (with the terminal's
  `\r` before each newline). `--console-socket` and `--log-dir` can't go together.
- **Rotation:** at most `--log-max-files N` files (default 5, from 2 to 100), each of at
  most `--log-max-size SIZE` (default `10m`, from `64k` to `1g`), plus one line: at
  most about 50 MiB per container by default. The oldest goes first.
- **The files** are `container.log` (the newest) and `container.log.1` …
  `container.log.N-1` (older and older), in the CRI format Kubernetes uses (as
  containerd and Podman write it), which `crictl`, fluent-bit's `cri` parser and the
  like read as they are. Each line is `TIME STREAM TAG TEXT`:

  ```text
  2026-10-10T06:16:19.123456789Z stdout F hello
  2026-10-10T06:16:19.123500000Z stderr P a line longer than 16 KiB is cut,
  ```

  `TIME` is when Shipyard read it (UTC, nanoseconds), `STREAM` `stdout` or `stderr`,
  `TAG` `F` for a line's end or `P` for part of one (a line longer than 16 KiB, or one
  the program hasn't ended for a second, a prompt say). Beside them, `log.json` says
  the format, its version and the rotation; a Shipyard that doesn't know the version
  refuses to read them rather than misread them.
- **`logs ID`** prints the container's output: what it wrote on stdout to stdout, on
  stderr to stderr, lines cut into parts joined again.
  - `--follow` (`-f`) goes on printing as more comes (parts of lines too, as they're
    written), across rotations, and ends once the container has ended and everything
    it wrote is printed.
  - `--tail N`: only the last N lines.
  - `--since TIME`: only lines from TIME on, RFC 3339 (`2026-10-10T06:16:19Z`) or a
    duration ago (`10m`, `2h`, `30s`).
  - `--timestamps` (`-t`): each line starts with its time.
  - `--log-dir DIR` instead of an ID reads the logs in DIR (a container deleted since);
    `--follow` then goes on until interrupted. It needs no state root.

  `--follow` looks for more five times a second: output that rotates through every
  file in between (more than the files hold, written in under a fifth of a second) is
  gone before it's read, as with any follower; the rest is printed in order. Give
  the logs more room (`--log-max-size`, `--log-max-files`) for a container that
  writes that fast.

  `--follow` also ends if the monitor has died (nothing more can come), or if the ID
  now names another container (deleted and created again, even with the same
  `--log-dir`). If `container.log` was deleted by hand, it waits for the new one the
  next rotation makes. `logs | head` ends quietly when `head` has had enough.

  `logs ID` reads only the directory the container was created with: one put at its
  path since (the real one moved or deleted) is refused, saying it "isn't the log
  directory its container was created with", rather than shown as the container's.

  Times are the host's clock when each line was read: a clock set back meanwhile
  makes later lines look older, and `--since` judges each line by its own time.

  A line that isn't a record (torn by a crash, say) is skipped, with a warning saying
  how many.
- **What can be lost:** a line written while the disk is full (it's dropped, the files
  are left as they are, and each stays whole; when a new file can't be made, the logs
  go on in the one they have), and whatever is written after the monitor has died.
  What was written before is there, whole. A file-size limit (`ulimit -f`) lower than
  `--log-max-size` rotates early instead: the line goes into a new file.
- **A `create` that fails** (its `--pid-file` included) removes the log directory it
  made, unless something was written to it (a hook that failed, say): then it's kept,
  and the error says where. Only that directory: one put at its path since is left
  alone.

## Attach

A container created with `--log-dir` can be attached to: its output from the moment you
attach (the past is in `logs`), and, if it takes input, your typing.

```sh
shipyard-runtime create --bundle ./mybundle --log-dir ./web-logs --stdin web
shipyard-runtime start web
shipyard-runtime attach web        # Ctrl-P Ctrl-Q to detach
```

- **`create --stdin`** keeps the container's standard input open (a pipe, or its
  terminal) for attached clients to type into; without it, attach only shows output.
  Several clients may type: what they type is interleaved. **Detaching leaves stdin
  open**, so you can attach again; **`--stdin-once`** closes it when the first client
  to type goes (it detaches, its input ends, or it's dropped), once what it typed has
  been written, so the program sees the end of its input: `shipyard-runtime attach web <
  input.txt` feeds a file through. A client that only watches (`--no-stdin`) doesn't
  count. `--stdin-once` can't go with a terminal (closing a terminal is a hang-up, not
  an end of input).
- **A container with a terminal:** your terminal goes raw while attached (every key,
  Ctrl-C included, goes to the container's terminal), is sized as your window and
  follows it, and is put back as it was when you detach or the container ends. The
  detach keys work one key at a time only there; with a container without a terminal,
  your terminal sends what you type a line at a time, so the keys reach the container's
  monitor when you press Enter (as with Docker). Its flow control is off while you're
  attached (otherwise it would take Ctrl-Q for itself, and Ctrl-P Ctrl-Q could never
  detach), and back on when attach ends, Ctrl-C included. Your terminal is put back
  however attach ends, killed (`kill`, a hang-up) too; only `kill -9` can't be
  caught.
- **Detaching:** Ctrl-P Ctrl-Q, or the keys `--detach-keys` gives: `ctrl-` and a letter
  or one of `@[\]^_`, or a character, separated by commas (`ctrl-a,d`); empty
  (`--detach-keys ""`) turns detaching off. The keys never reach the container; keys
  that start them and then don't are passed on.
- **`attach` ends** with the container, with its exit code (128 plus the signal's
  number, for one killed), even if it ended just as you attached; with 0 when you
  detach; with **255** when attach itself fails (no such container, one that can't be
  attached to, the monitor killed: the container goes with it), as `exec`'s own
  failures. `attach ID | head` ends quietly.
- **`--no-stdin`** only watches the output.
- **Who may attach:** only the container's owner (or root). The socket (`attach.sock` in
  the state directory) is the owner's alone, and the container's monitor checks who
  connected (`SO_PEERCRED`) whatever the socket's mode.
- **Limits:** at most 16 clients at once (a 17th is refused, saying so), and as many
  connections waiting to be let in (one more is refused, saying so). A client that falls
  more than 256 KiB behind the container's output (a slow link, a stopped `attach`, or a
  terminal slower than a flood) is let go, told so, so that no client ever slows the
  container. Typing beyond 64 KiB that the container hasn't read yet is dropped, so that
  detaching always works: a container that reads slowly may lose some of a large file
  piped into `attach`. A connection that doesn't say hello within 5 seconds is closed;
  `attach` gives up if the monitor doesn't answer within 10 (it's still running the
  container's setup hooks, say).
- **Versions:** a newer `shipyard-runtime` attaches to a container an older one created
  (they agree on the older protocol).

## Exec

`exec` runs a command inside a container that's *created* or *running*, as runc's
does:

```sh
shipyard-runtime exec web ps        # in the container's namespaces and cgroup
shipyard-runtime exec -d web sleep 1000
shipyard-runtime exec --process process.json web
```

- **It joins everything the container has:** its user, PID, mount, UTS, IPC, network
  and cgroup namespaces, and it starts in the container's cgroup, so the container's
  limits, device rules, `pause` and `kill --all` cover it. It dies with the container.
- **It's confined as the container's own process is:** the same capabilities,
  `no_new_privs`, seccomp filter, limits, environment, working directory and user,
  unless you change them. These come from the container's process as it was started
  (`exec.json`, below), not from `config.json` again: editing the bundle afterwards
  changes nothing.
- **You can narrow it, never widen it.** `--env`, `--cwd`, `--user`,
  `--additional-gids`, `--no-new-privs`, `--cap`, or a runc `process.json`
  (`--process`) replace the container's values. But a capability outside the
  container's bounding set, `noNewPrivileges` off when the container has it on, a
  limit above the container's, or a lower OOM score adjustment is refused (the
  container's are what its process really has, the limits it inherited included). A
  `process.json` that lists some limits keeps the container's for the others. The
  seccomp filter is always the container's. As with runc, `--cap` adds to the
  bounding, effective and permitted sets; a user other than root keeps it across its
  program's start only if the container's process has inheritable capabilities.
- **Supplementary groups** (`--additional-gids`) are refused in a container whose user
  namespace doesn't allow setting them (a rootless one with a single mapping).
- **Users are numbers** (`--user 1000:1000`), as the container numbers them.
- **Exit codes:** the command's own (128 plus the signal's number, for one killed);
  **255** when `exec` itself fails (no such container, a refused option, a program
  that isn't there), as runc. (A command that itself exits 255 looks the same.)
  `--detach` (`-d`) returns 0 once the command runs and leaves it running: it keeps
  `exec`'s standard input and output, files and pipes (with `-t`, its terminal
  instead; give it others if nothing should wait for it), and whoever inherits it must
  reap it (a shell or init does). `--pid-file` (with `-d`) names it.
- **Never your terminal** (without `-t`): where `exec`'s standard input, output or
  error is a terminal, the command gets a pipe that `exec` relays instead (`/dev/null`
  with `-d`, where nothing relays), so nothing it leaves behind in the container can
  keep your terminal, to read what you type to your shell later or change its
  settings. Files and pipes are passed on as they are, as runc does. (runc passes a
  terminal on too.) For a terminal, use `-t`. Started in the background (`exec … &`),
  the command's stdin is `/dev/null`, so `exec` never stops to read your terminal; in
  the foreground, what you type goes to the command a line at a time.
- **Refused:** a *paused* container (the command would be frozen before it ran; `exec`
  holds the container's lock until it runs, so a `pause` waits that long), a *stopped*
  one, one whose cgroup isn't the one made for it, and one created by a Shipyard from
  before `exec` (recreate it). A command stopped or frozen from inside the container
  before it runs, or not running within 10 seconds, is killed: `exec` fails rather
  than hold the container's lock.
- **Rootless:** your containers' `exec` joins their user namespace (root inside is
  you). Your login's cgroup must be delegated to you, as a desktop session's is: from
  an SSH session it's refused, saying to run it as `systemd-run --user --scope
  shipyard-runtime exec …`.
- **Hidden until it runs:** until the command starts, nothing in the container can read
  `exec`'s memory, its descriptors or its environment, nor trace it, and it holds no
  descriptor of the host's but those passed on with `--preserve-fds` (CVE-2016-9962,
  CVE-2024-21626). Its program is a sealed
  copy of Shipyard's, not the file itself (CVE-2019-5736): on a host that won't make
  or run one (`vm.memfd_noexec` at 2, or a security module's policy), it runs the file,
  with a warning, only where nobody in the container could write it: on a read-only
  mount, or with its owner not a user inside, no group that may write it a group inside
  or one of yours (a rootless container with a single ID mapped keeps your groups), and
  no write for others;
  else `exec` is refused, saying it "needs a sealed copy of shipyard's binary". A
  root container without a user namespace of its own (its root is host root) is
  always refused there, and so is a rootless one when Shipyard is installed in your
  home (it's yours, and root inside is you): install it where root owns it
  (`/usr/local/bin`). A hard file-size limit (`ulimit -f`) smaller than Shipyard's
  binary makes no copy either (as for `create`: see [Running commands](run.md)). A host
  with `fs.suid_dumpable` set to 1 is refused (set it to 0 or 2).
- **Into a *created* container:** what the command leaves behind is reaped only once
  the container has started (its init waits at the gate until then).
- **A terminal** (`-t`, or a `process.json` with `terminal: true`): one of the
  command's own, made inside the container from its own `/dev/pts`, as runc does.
  Without `--console-socket` it's relayed to `exec`'s own: your terminal goes raw (every
  key, Ctrl-C included, reaches the command) and is put back after; the command's
  terminal follows your window's size (else the `process.json`'s `consoleSize`, else
  24x80). If `exec`'s input isn't a terminal, it's relayed as it is (a terminal never
  sees an end of input there, as with runc: `exec -t ID cat < file` doesn't end by
  itself). Started in the background (`exec -t … &`), it relays output only, so that
  its command starts at once (it holds the container's lock until then). With
  `--console-socket SOCK`, the terminal goes to that socket instead (runc's protocol:
  the master, named `/dev/ptmx`, nothing answered), whose receiver holds it; `-d -t`
  needs one, as runc does, and a console socket needs a terminal. A container that
  replaced its `/dev/pts/ptmx` with anything but its devpts's own (a link, a file,
  another device) gets no terminal: `exec` refuses, without opening it.
- **Not yet:** user and group names.
- **Descriptors:** `--preserve-fds N`, and socket activation (see
  [Passing descriptors](#passing-descriptors)).

## Passing descriptors

A container's process gets only its standard input, output and error (or its
terminal) of what its caller has open, unless the caller passes more on, as runc's
callers do:

```sh
shipyard-runtime create --preserve-fds 2 web 3<config.json 4>>audit.log
shipyard-runtime exec --preserve-fds 1 web ./reload 3<new.conf
```

- **`--preserve-fds N`** (`create` and `exec`) passes N more descriptors on, at the
  same numbers: 3 to 3+N-1, or the N after socket activation's. Every one must be open, or the command is refused before
  anything is made ("descriptor N isn't open"). At most 1024, and about half the
  caller's soft descriptor limit (`ulimit -n`): raise it to pass more.
- **Socket activation** (systemd's `sd_listen_fds`): when the caller's environment
  has `LISTEN_FDS=n` and `LISTEN_PID` is the caller's own PID, descriptors 3 to
  3+n-1 are passed on first (then `--preserve-fds`'s), and the container's process
  gets `LISTEN_FDS=n`, `LISTEN_FDNAMES` (if set), and `LISTEN_PID` set to **its own
  PID**, as `sd_listen_fds` expects. Its own `LISTEN_*` variables (the bundle's) are
  replaced. A `LISTEN_PID` that isn't the caller's (inherited from a shell, say) is
  ignored: nothing is passed. A bundle that says `LISTEN_FDS` itself, with its sockets
  given by `--preserve-fds` (as Podman does it), gets its own PID as `LISTEN_PID` too,
  in place of the bundle's.
- **Only the container's process holds them** once it runs: `create`, the monitor
  and the container's init close their copies (a socket the process closes is then
  closed, unless something else holds it: a service manager keeps its own). Between
  `create` and `start`, the init holds them. Hooks never get them.
- **Under systemd,** `LISTEN_PID` names the unit's main process, so that must be the
  runtime itself, not a shell around it:

  ```ini
  [Service]
  Type=forking
  PIDFile=/run/web.pid
  ExecStart=/usr/local/bin/shipyard-runtime create --bundle /srv/web --pid-file /run/web.pid web
  ExecStartPost=/usr/local/bin/shipyard-runtime start web
  ExecStopPost=/usr/local/bin/shipyard-runtime delete --force web
  ```

  (`shipyard run` can be `ExecStart` itself: see [run](run.md).)

## Holds and an exit command

A caller that keeps resources for a container (a lock that says an address is taken,
say) and goes away before the container does can hand them to the container's
monitor, which holds them while the container lives, and then to a command of the
caller's that cleans up after it, as Podman's conmon does:

```sh
shipyard-runtime create --hold-fd 9=lock:web --exit-command /usr/local/bin/cleanup \
    --exit-command-arg web web 9<>/run/my/web.lock
```

- **`--hold-fd N[=LABEL]`** (repeatable): the monitor keeps descriptor N open from
  `create` until the container has ended. LABEL names it (letters, digits, `:`, `_`,
  `.` and `-`, at most 128; `fd-N` if not given): the exit command is told the labels,
  and a hold can be let go of by its label while the container runs (below). Two
  holds may share a label (two ports, say), but then neither can be let go of by it.
  At most 2100, with labels of at most 96 KiB all told. The container never has it
  (nor do its hooks). N
  must be open, above 2, given once, not one of those passed to the container
  (`--preserve-fds`), and a file, a directory or a socket (not a pipe or a device):
  "--hold-fd N: not a descriptor the monitor can hold (it must be open, above 2, given
  once, not passed to the container, and a file, a directory or a socket)". A lock
  taken with `flock` stays held as long as the monitor has its copy, whatever the
  caller does with its own, short of unlocking it: an unlock through any copy
  releases it for all.
- **`--exit-command PROGRAM`** (an absolute path), with `--exit-command-arg ARG` for
  each argument: run once the container has ended and its end is recorded (`state`
  says stopped), however it ended (`delete --force` too). It runs in a session of its
  own, in `/`, with the caller's environment, its output discarded, the held
  descriptors at 3, 4, … in the order given (`SHIPYARD_HOLDS` says how many,
  `SHIPYARD_HOLD_LABELS` their labels, comma-separated, in the same order), and how
  the container ended in `SHIPYARD_EXIT_RECORD` (the `exit` record's JSON); nothing
  else is open. The monitor doesn't wait for it, and keeps its own copies of the held
  descriptors until it has exited: what they keep alive (a `flock`'s lock) is free only
  once both have closed them. `SHIPYARD_MONITOR_PID` gives the monitor's PID: while the
  command's parent is that PID, the monitor is still there. It's kept in `state.json`
  (`exit_command`), never taken from the bundle. It may find the container's state
  gone (`delete --force` doesn't wait for it), and an ID may be reused meanwhile: it
  shouldn't act on the container by its ID alone.
- **An exit command that can't be started** (a missing program, say) fails silently:
  the holds then close with the monitor.
- **Without an exit command,** the holds are closed once the container has ended.
- **A monitor that's killed** (SIGKILL) closes them at once, and no exit command
  runs: a caller's own sweep must find what was left (a lock nobody holds).
- **A `create` that fails** runs no exit command: the caller still has its own
  copies.
- **While the container runs** (with `--log-dir`: through its attach socket), a
  program of the same user (or root) can give the monitor another descriptor to hold
  (a file, a directory or a socket), or have it let go of one, by its label. It's
  Shipyard's own extension of the attach protocol (version 3: a *holder*, which
  passes the descriptor with `SCM_RIGHTS`), for Shipyard's engine, with no command of
  its own here. One held since `create` reaches the exit command after those given at
  `create`, with its label; one let go of doesn't. A label held already, more than 64
  held, a descriptor of another kind (a pipe, a device), or a hold while the container
  is starting or once it has begun to end, is refused.

A caller can also give the monitor a **companion**: a program that runs beside the
container for its whole life (Shipyard's engine runs a container's health checks so):

```sh
shipyard-runtime create --companion /usr/local/bin/checker --companion-arg web web
```

- **`--companion PROGRAM`** (an absolute path), with `--companion-arg ARG` for each
  argument: started once the container is created (held at its gate: the program
  waits for the start itself, if it must), in a session of its own, in `/`, with the
  caller's environment, its output discarded, and none of the held descriptors.
- Once the container's init has ended, **before its end is recorded**, the monitor
  kills the companion's process group (SIGKILL: what it started in that group goes
  with it) and reaps it. A monitor that's killed takes it too (its parent-death
  signal is SIGKILL). What it started in a group of its own is its own to end: give
  it a parent-death signal.
- One that ends early, or can't be started, is left so: the container runs on.
- It's kept in `state.json` (`companion`), never taken from the bundle.

## Copying files

`cp` copies files into and out of a container that's *created*, *running* or *paused*,
with Docker's forms:

```sh
shipyard-runtime cp web:/etc/nginx/nginx.conf .     # out
shipyard-runtime cp ./site web:/usr/share/nginx/    # in: a directory, whole
shipyard-runtime cp web:/var/log - | tar -t         # out, as a tar stream on stdout
tar -c data | shipyard-runtime cp - web:/srv         # in, from a tar stream on stdin
```

- **Which side is which:** `ID:PATH` is the container's (its path from its root,
  whether or not it starts with `/`); anything else is a host path, and `-` a tar
  stream. A host path with a colon in it needs `./` or `/` in front (`./a:b`).
- **Docker's path rules:**

  | Source | Destination | Result |
  |---|---|---|
  | a file | doesn't exist | made, with the destination's name (its directory must exist; a destination ending in `/` must be an existing directory) |
  | a file | a file | overwritten |
  | a file | a directory | copied into it |
  | a directory | doesn't exist | made, with the destination's name |
  | a directory | a file | an error |
  | a directory | a directory | copied into it; `SRC/.` copies what's in it instead |

  A destination ending in `/.` is a directory, as one ending in `/`. A tar stream given
  with `-` is extracted into the destination directory, as it is, and must end with
  tar's end-of-archive marker (one cut off is refused, though what came before it
  stays). Out to `-`, the stream holds the source under its own name (`cp web:/var/log
  -`: `log/…`), and isn't written to a terminal. What's there already is replaced, a
  directory merged; a directory never replaces a file, nor the other way round, and a
  device, FIFO or socket is never replaced at all (`cp web:/f /dev/null` is refused,
  where an unlink would replace the device with a file). A
  destination that's a symlink is replaced by the copy, not followed (Docker follows
  it). A copy that fails leaves what it copied so far, but not an empty directory it
  made for it.
- **Owners:** files copied in belong to the container's root, unless `--archive`
  (`-a`) keeps their owners (an owner the container has no ID for becomes its root,
  and the file loses its setuid and setgid bits). Files copied out belong to you,
  without setuid or setgid bits; in a stream (`-`) they keep the container's owners and
  modes.
- **Links:** a symlink is copied as a symlink, unless it's the source itself and you
  give `--follow-link` (`-L`), or the source ends with `/` or `/.`. Inside the
  container, a link is resolved as the container sees it: `/data -> /etc` is the
  container's `/etc`, never the host's, even if it's swapped while the copy runs
  (CVE-2018-15664). Hard links within what's copied stay hard links.
- **What isn't copied:** device nodes (in either direction: a warning says how many
  were left out), sockets, extended attributes (so no file capabilities: but a `-`
  stream's own `user.*` attributes and file capabilities are applied in the
  container), and what's in the kernel's own filesystems (`/proc`, `/sys`, `/dev/pts`, cgroupfs and the like: a
  directory there is copied empty). A FIFO is copied as a FIFO, never read. A file that
  grows while it's copied is copied at the size it had. Nothing is ever written into
  an existing file in place: a file is replaced by a new one, so writing into
  `/proc/sys` is refused.
- **Who does the work:** inside the container, a short-lived helper: Shipyard's own
  binary (a sealed copy, as for [Exec](#exec)), never one of the container's
  (CVE-2019-14271), joined to the container's user and mount namespaces, as root
  there with only the capabilities that read and write files, `no_new_privs`, and
  non-dumpable. It holds no descriptor of the host's but its standard streams, the
  pipe it reports on, and its own `/proc/self/fd`, and dies with `cp`. It isn't in the
  container's PID namespace or cgroup: nothing in a container with a PID namespace of
  its own can see or signal it (one sharing the host's can signal it, so stop a copy,
  never trace it), and a *paused* container can be copied. On the
  host, `cp` only opens your paths, and extracts what comes out of the container with
  the same safe extraction as an image's layers.
- **Rootless:** the helper is root in your container's user namespace, so files owned
  by your subordinate IDs, which you can't read from the host, are copied too.
- **Refused:** a *stopped* container (`shipyard cp` copies from one, by its record:
  see [Containers](containers.md#copying-files-cp)), one with no mount namespace of its
  own (its files are the host's), and copying between two containers.
- **Limits:** a directory of more than a million names, or a tree more than 256
  directories deep, is refused; files that are mostly holes (sparse) are copied whole;
  memory for what a copy keeps track of is capped at 1 GiB (some ten million entries).
- **Not yet:** user names in `--archive` (owners are numbers).

## The state directory

Each container has a directory `ROOT/ID` (only its owner can read it):

| File | What it is |
|---|---|
| `state.json` | The bundle, the init's PID, the annotations, the cgroup, the hooks, where the logs are (`--log-dir`), the exit command (`--exit-command`) and the companion (`--companion`). It has a `version`. |
| `exec.fifo` | Where `start` signals the waiting container; removed once started. |
| `exec.started` | Made only for a bundle with `poststart` hooks: held open by the container until its command has started, so `start` runs them after that. |
| `exit` | How the container ended, once it has: JSON with a `version`, a `description` (`exited (exit code 3)`), the `code`, what the out-of-memory killer did (`oom`) and when it ended (`finished`). |
| `killed` | Left by `kill` with SIGKILL, `kill --all` or `delete`: the container's end was theirs, not the out-of-memory killer's. An empty marker: that it's there is all it says. |
| `attach.sock` | Where `attach` connects (a container created with `--log-dir`). |
| `exec.json` | The container's process as it was started: what `exec` starts from. |
| `plan.json` | Only for a container created from a plan rather than a bundle (`create_planned`, as Shipyard's engine does: an image's overlay as its root, users by name): what it runs, checked as a bundle is. `state` then gives this directory as the bundle, and `exec` runs as the plan's user, its names looked up in the container's own `/etc/passwd` and `/etc/group`. Read only by the container's own monitor, so its version is never read across upgrades. |

**Upgrading.** Every file here that holds data has a version (markers and sockets need
none), and Shipyard reads the previous one:
containers made by the previous version keep working after an upgrade, even while they
run (their monitor is the previous version's, and writes what it always wrote). A file
written by a newer Shipyard is refused (`state.json has version 2, which this Shipyard
doesn't know`) rather than misread: an older Shipyard can't show or delete a container a newer one made
(delete it with the newer one). It still finds the exit codes in the newer exit records.

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
- **`startContainer` hooks run after `start` lets the container go,** in the
  container, before its program. With `poststart` hooks, `start` returns only once they
  have run (it waits for the program to start); without, it may return while they still
  run. A failing `startContainer` hook
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
- **`LISTEN_PID`** is the container's process's own PID, not runc's `1`: Shipyard's
  init is PID 1 in the container, and `sd_listen_fds` compares `LISTEN_PID` with the
  process's PID.
- **Not yet:** `list` and `run`.

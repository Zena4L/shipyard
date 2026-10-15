<!-- Generated from shipyard-runtime's flag definitions. Do not edit by hand.
     Regenerate: UPDATE_DOCS=1 cargo test -p shipyard-cli runtime_reference -->

# shipyard-runtime reference

This document contains the help content for the `shipyard-runtime` command-line program.

**Command Overview:**

* [`shipyard-runtime`↴](#shipyard-runtime)
* [`shipyard-runtime create`↴](#shipyard-runtime-create)
* [`shipyard-runtime logs`↴](#shipyard-runtime-logs)
* [`shipyard-runtime attach`↴](#shipyard-runtime-attach)
* [`shipyard-runtime exec`↴](#shipyard-runtime-exec)
* [`shipyard-runtime cp`↴](#shipyard-runtime-cp)
* [`shipyard-runtime start`↴](#shipyard-runtime-start)
* [`shipyard-runtime state`↴](#shipyard-runtime-state)
* [`shipyard-runtime kill`↴](#shipyard-runtime-kill)
* [`shipyard-runtime pause`↴](#shipyard-runtime-pause)
* [`shipyard-runtime resume`↴](#shipyard-runtime-resume)
* [`shipyard-runtime ps`↴](#shipyard-runtime-ps)
* [`shipyard-runtime events`↴](#shipyard-runtime-events)
* [`shipyard-runtime update`↴](#shipyard-runtime-update)
* [`shipyard-runtime delete`↴](#shipyard-runtime-delete)

## `shipyard-runtime`

Runs containers from OCI bundles, one lifecycle step per call

**Usage:** `shipyard-runtime [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `create` — Create a container from a bundle, set up and waiting for `start`
* `logs` — Print a container's logs (one created with --log-dir): what it wrote on stdout to stdout, on stderr to stderr
* `attach` — Attach to a container created with --log-dir: its output from now on, and your typing (with `create --stdin`). Ctrl-P Ctrl-Q detaches; otherwise it ends with the container, with its exit code
* `exec` — Run a command inside a created or running container: in its namespaces and cgroup, confined as its own process is (what isn't given is its process's). Exits with the command's code, or 255 if it couldn't run
* `cp` — Copy files into or out of a created, running or paused container: `cp ID:SRC DEST`, `cp SRC ID:DEST`, with `-` for a tar stream on stdout or stdin. Directories are copied whole
* `start` — Run a created container's program
* `state` — Print a container's state as JSON
* `kill` — Send a signal to a container (default TERM)
* `pause` — Freeze a created or running container's processes
* `resume` — Thaw a paused container
* `ps` — List the processes in a container's cgroup, by host PID
* `events` — Print a container's resource use as JSON lines, and OOM kills, until it stops
* `update` — Change a container's resource limits (created, running or paused)
* `delete` — Remove a stopped (or created) container

###### **Options:**

* `--root <DIR>` — Directory for the containers' state [default: `/run/shipyard` for root, `$XDG_RUNTIME_DIR/shipyard` for anyone else]
* `--log <FILE>` — Also write errors and warnings to FILE (as runc's callers ask for)
* `--log-format <FORMAT>` — The log file's format: text or json

  Default value: `text`

  Possible values: `text`, `json`




## `shipyard-runtime create`

Create a container from a bundle, set up and waiting for `start`

**Usage:** `shipyard-runtime create [OPTIONS] <ID>`

###### **Arguments:**

* `<ID>` — The container's ID

###### **Options:**

* `-b`, `--bundle <DIR>` — The bundle: a directory with config.json

  Default value: `.`
* `--pid-file <FILE>` — Write the container's init PID to FILE
* `--console-socket <SOCKET>` — A UNIX socket to send the container's terminal to (its pseudo-terminal's master), as runc does; needed when the bundle asks for a terminal (`process.terminal`)
* `--log-dir <DIR>` — Keep the container's output in logs in DIR, read with `logs`: CRI-format files, rotated. `create` makes DIR (it mustn't exist); it outlives the container. The container then doesn't get `create`'s stdout and stderr, and its stdin is empty unless --stdin keeps it open
* `--log-max-size <SIZE>` — The most a log file holds before it's rotated (64k to 1g) [default: 10m]
* `--log-max-files <N>` — How many log files are kept, the newest among them (2 to 100) [default: 5]
* `--stdin` — Keep the container's stdin open for `attach` to type into, instead of empty (/dev/null). Needs --log-dir
* `--stdin-once` — Close the container's stdin when the first client to type goes (detaches, its input ends, or it's dropped), once what it typed has been written
* `--preserve-fds <N>` — Pass N more descriptors on to the container's process, from 3 up (after socket activation's, if `LISTEN_FDS` and `LISTEN_PID` give it any), as runc does

  Default value: `0`
* `--hold-fd <N[=LABEL]>` — Have the container's monitor hold descriptor N (open in this process, above 2 and above those --preserve-fds passes on) until the container has ended, then give it to the exit command, or close it: what keeps the caller's resources (a lock's, say) alive once the caller has gone. The container never has it. LABEL (letters, digits, : _ . and -; fd-N if not given) names it: the exit command is told the labels in `SHIPYARD_HOLD_LABELS`. Repeatable
* `--exit-command <PROGRAM>` — Run PROGRAM (an absolute path) once the container has ended and its end is recorded: in a session of its own, with this command's environment, its output discarded, the held descriptors at 3 and up (`SHIPYARD_HOLDS` says how many, `SHIPYARD_HOLD_LABELS` their labels, comma-separated, in order), how the container ended in `SHIPYARD_EXIT_RECORD` (JSON), and the monitor's PID in `SHIPYARD_MONITOR_PID` (the monitor holds its copies of the descriptors until it exits). Not waited for
* `--exit-command-arg <ARG>` — An argument for the exit command (repeatable, in order)
* `--companion <PROGRAM>` — Run PROGRAM (an absolute path) beside the container: started once it's created (the program waits for its start, if it needs to), in a session of its own, with this command's environment, its output discarded and none of the held descriptors; killed (SIGKILL), with its whole process group, once the container has ended, before its end is recorded (and if the monitor dies). For health checks
* `--companion-arg <ARG>` — An argument for the companion (repeatable, in order)



## `shipyard-runtime logs`

Print a container's logs (one created with --log-dir): what it wrote on stdout to stdout, on stderr to stderr

**Usage:** `shipyard-runtime logs [OPTIONS] [ID]`

###### **Arguments:**

* `<ID>` — The container's ID

###### **Options:**

* `-f`, `--follow` — Then print more as it comes, until the container has ended (with --log-dir, until interrupted)
* `--tail <N>` — Only the last N lines
* `--since <TIME>` — Only lines from TIME on: RFC 3339 (2026-10-10T06:16:19Z), or a duration ago (10m, 2h, 30s)
* `-t`, `--timestamps` — Start each line with the time it was written (RFC 3339)
* `--log-dir <DIR>` — Read the logs in DIR instead of a container's (one deleted since, say)



## `shipyard-runtime attach`

Attach to a container created with --log-dir: its output from now on, and your typing (with `create --stdin`). Ctrl-P Ctrl-Q detaches; otherwise it ends with the container, with its exit code

**Usage:** `shipyard-runtime attach [OPTIONS] <ID>`

###### **Arguments:**

* `<ID>` — The container's ID

###### **Options:**

* `--detach-keys <KEYS>` — The keys that detach, Docker's way: `ctrl-` and a letter (or one of @[\]^_), or a character, separated by commas; empty: none

  Default value: `ctrl-p,ctrl-q`
* `--no-stdin` — Don't pass your input on (only watch the output)



## `shipyard-runtime exec`

Run a command inside a created or running container: in its namespaces and cgroup, confined as its own process is (what isn't given is its process's). Exits with the command's code, or 255 if it couldn't run

**Usage:** `shipyard-runtime exec [OPTIONS] <ID> [COMMAND]...`

###### **Arguments:**

* `<ID>` — The container's ID
* `<COMMAND>` — The command and its arguments

###### **Options:**

* `-p`, `--process <FILE>` — A runc process.json: the whole process, instead of COMMAND
* `-d`, `--detach` — Return once the command runs, leaving it running (it isn't stopped with this one)
* `-t`, `--tty` — Give the command a terminal, made in the container: relayed to this one (raw, and sized as it), or with --console-socket sent there
* `--console-socket <SOCKET>` — Send the command's terminal's master to this UNIX socket (runc's protocol), instead of relaying it; needs a terminal (-t, or the process.json's). With -d -t, required
* `--pid-file <FILE>` — With --detach, write the command's PID (as the host sees it) to FILE
* `--cwd <DIR>` — The working directory, inside the container
* `-e`, `--env <KEY=VALUE>` — Set an environment variable (KEY=VALUE) over the container's; may be given more than once
* `-u`, `--user <UID[:GID]>` — The user, UID or UID:GID, as the container numbers them
* `-g`, `--additional-gids <GID>` — A supplementary group, instead of the container's; may be given more than once
* `-c`, `--cap <CAP>` — Add a capability (`SYS_PTRACE`, or `CAP_SYS_PTRACE`), within the container's bounding set; may be given more than once
* `--no-new-privs` — Set `no_new_privs` (on anyway if the container has it)
* `--preserve-fds <N>` — Pass N more descriptors on to the command, from 3 up (after socket activation's, if `LISTEN_FDS` and `LISTEN_PID` give it any), as runc does

  Default value: `0`



## `shipyard-runtime cp`

Copy files into or out of a created, running or paused container: `cp ID:SRC DEST`, `cp SRC ID:DEST`, with `-` for a tar stream on stdout or stdin. Directories are copied whole

**Usage:** `shipyard-runtime cp [OPTIONS] <SRC> <DEST>`

###### **Arguments:**

* `<SRC>` — ID:PATH, a host path, or - (a tar stream on stdin)
* `<DEST>` — ID:PATH, a host path, or - (a tar stream on stdout)

###### **Options:**

* `-a`, `--archive` — Files copied in keep their owners (where the container has those IDs), instead of becoming the container's root's
* `-L`, `--follow-link` — Follow SRC if it's a symlink



## `shipyard-runtime start`

Run a created container's program

**Usage:** `shipyard-runtime start <ID>`

###### **Arguments:**

* `<ID>` — The container's ID



## `shipyard-runtime state`

Print a container's state as JSON

**Usage:** `shipyard-runtime state <ID>`

###### **Arguments:**

* `<ID>` — The container's ID



## `shipyard-runtime kill`

Send a signal to a container (default TERM)

**Usage:** `shipyard-runtime kill [OPTIONS] <ID> [SIGNAL]`

###### **Arguments:**

* `<ID>` — The container's ID
* `<SIGNAL>` — A name (TERM, SIGKILL) or a number

  Default value: `TERM`

###### **Options:**

* `-a`, `--all` — Signal every process in the container's cgroup, not only its init



## `shipyard-runtime pause`

Freeze a created or running container's processes

**Usage:** `shipyard-runtime pause <ID>`

###### **Arguments:**

* `<ID>` — The container's ID



## `shipyard-runtime resume`

Thaw a paused container

**Usage:** `shipyard-runtime resume <ID>`

###### **Arguments:**

* `<ID>` — The container's ID



## `shipyard-runtime ps`

List the processes in a container's cgroup, by host PID

**Usage:** `shipyard-runtime ps [OPTIONS] <ID>`

###### **Arguments:**

* `<ID>` — The container's ID

###### **Options:**

* `--format <FORMAT>` — table, or json (an array of PIDs)

  Default value: `table`

  Possible values:
  - `table`:
    PID, user ID and command, one process per line
  - `json`:
    A JSON array of PIDs (what engines read)




## `shipyard-runtime events`

Print a container's resource use as JSON lines, and OOM kills, until it stops

**Usage:** `shipyard-runtime events [OPTIONS] <ID>`

###### **Arguments:**

* `<ID>` — The container's ID

###### **Options:**

* `--stats` — Print once and exit
* `--interval <DURATION>` — How often (such as 5s or 500ms)

  Default value: `5s`



## `shipyard-runtime update`

Change a container's resource limits (created, running or paused)

**Usage:** `shipyard-runtime update [OPTIONS] <ID>`

###### **Arguments:**

* `<ID>` — The container's ID

###### **Options:**

* `-r`, `--resources <FILE>` — Read the limits from FILE, a linux.resources JSON object (- for stdin), instead of flags
* `-m`, `--memory <SIZE>` — The most memory the container may use (such as 512m or 2g); past it, the kernel's OOM killer stops a process (Linux; rootless where a cgroup subtree is delegated)
* `--memory-reservation <SIZE>` — Memory kept for the container when the host runs short (cgroup v2's memory.low)
* `--memory-swap <SIZE>` — Memory plus swap (needs --memory, except -1): -1 for unlimited swap. Without it, the container may swap as much as --memory, as with Docker
* `--cpus <NUMBER>` — How many CPUs' worth of time the container may use (such as 1.5)
* `--cpu-period <MICROSECONDS>` — The CPU period, in microseconds (1000 to 1000000; default 100000)
* `--cpu-quota <MICROSECONDS>` — CPU time per period, in microseconds (-1 or 0 for no limit)
* `-c`, `--cpu-shares <SHARES>` — The container's CPU share against others, in Docker's units (1024 is the default share)
* `--cpuset-cpus <LIST>` — The CPUs the container may run on (such as 0-3 or 0,2)
* `--cpuset-mems <LIST>` — The memory nodes the container may use (such as 0 or 0-1)
* `--pids-limit <NUMBER>` — The most processes (and threads) in the container, its init included (-1 or 0 for no limit)
* `--blkio-weight <WEIGHT>` — The container's disk-time share, in Docker's units (10 to 1000)
* `--device-read-bps <DEVICE:RATE>` — Limits reads from a block device, in bytes per second (/dev/sda:10mb). Repeatable
* `--device-write-bps <DEVICE:RATE>` — Limits writes to a block device, in bytes per second. Repeatable
* `--device-read-iops <DEVICE:RATE>` — Limits reads from a block device, in operations per second (/dev/sda:1000). Repeatable
* `--device-write-iops <DEVICE:RATE>` — Limits writes to a block device, in operations per second. Repeatable



## `shipyard-runtime delete`

Remove a stopped (or created) container

**Usage:** `shipyard-runtime delete [OPTIONS] <ID>`

###### **Arguments:**

* `<ID>` — The container's ID

###### **Options:**

* `-f`, `--force` — Kill a running container first




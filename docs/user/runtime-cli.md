<!-- Generated from shipyard-runtime's flag definitions. Do not edit by hand.
     Regenerate: UPDATE_DOCS=1 cargo test -p shipyard-cli runtime_reference -->

# shipyard-runtime reference

This document contains the help content for the `shipyard-runtime` command-line program.

**Command Overview:**

* [`shipyard-runtime`↴](#shipyard-runtime)
* [`shipyard-runtime create`↴](#shipyard-runtime-create)
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
* `--console-socket <SOCKET>` — A socket for the container's terminal (not supported yet)



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




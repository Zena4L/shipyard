<!-- Generated from the CLI's flag definitions. Do not edit by hand.
     Regenerate: UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference -->

# Command reference

This document contains the help content for the `shipyard` command-line program.

**Command Overview:**

* [`shipyard`↴](#shipyard)
* [`shipyard run`↴](#shipyard-run)
* [`shipyard doctor`↴](#shipyard-doctor)

## `shipyard`

A lightweight container engine

**Usage:** `shipyard [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `run` — Run a command in a container: on Linux, isolated in namespaces, optionally with its own root filesystem
* `doctor` — Show how this host will run containers

###### **Options:**

* `-D`, `--debug` — Show debug logs (same as --log-level debug)
* `--log-level <LEVEL>` — How much to log

  Possible values: `error`, `warn`, `info`, `debug`, `trace`




## `shipyard run`

Run a command in a container: on Linux, isolated in namespaces, optionally with its own root filesystem

**Usage:** `shipyard run [OPTIONS] [COMMAND]...`

###### **Arguments:**

* `<COMMAND>` — The command to run, followed by its arguments

###### **Options:**

* `-e`, `--env <KEY[=VALUE]>` — Set an environment variable, or copy KEY from your shell if no value is given
* `-w`, `--workdir <DIR>` — Working directory inside the container (absolute path)

  Default value: `/`
* `-u`, `--user <USER[:GROUP]>` — Run as this user and group: names (from the container's /etc/passwd and /etc/group) or numbers
* `--group-add <GROUP>` — Add a supplementary group: a name or a number
* `--ulimit <NAME=SOFT[:HARD]>` — Set a resource limit, e.g. nofile=1024:2048 or core=0
* `--oom-score-adj <N>` — The command's OOM score adjustment, -1000 to 1000: how willing the kernel's OOM killer is to pick it (lowering it needs root)
* `--device <HOST[:CONTAINER][:PERMS]>` — Give the container the host's device HOST (a directory: every device below it), at CONTAINER inside (HOST's own path if not given), for PERMS, from rwm (all three if not given). Repeatable (Linux)
* `--device-cgroup-rule <RULE>` — Let the container use a kind of device: "TYPE MAJOR:MINOR ACCESS", such as "c 1:3 rwm" or "b *:* r" (a for any type, * for any number). Repeatable (Linux)
* `--stop-signal <SIGNAL>` — Signal sent to stop the command

  Default value: `SIGTERM`
* `--stop-timeout <SECONDS>` — Seconds to wait after the stop signal before killing the command

  Default value: `10`
* `--no-isolation` — Run without isolation: the command sees the host's processes, mounts, hostname and IPC, as you. On Linux, commands are isolated by default (rootless, in a user namespace, without root); on macOS they always run like this for now
* `--hostname <NAME>` — Hostname inside the container (Linux)
* `--domainname <NAME>` — NIS domain name inside the container (Linux)
* `--sysctl <KEY=VALUE>` — Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824 (Linux; IPC settings only)
* `--rootfs <DIR>` — Use DIR as the container's root filesystem: it becomes `/` inside, with its own /proc, /sys and /dev (Linux)
* `--read-only` — Make the container's root filesystem read-only (with --rootfs)
* `--cap-add <CAP>` — Give the command a capability on top of the default set, like `NET_ADMIN` (or `CAP_NET_ADMIN`); `ALL` for every one. Repeatable (Linux)
* `--cap-drop <CAP>` — Take a capability out of the default set; `ALL` for every one. Repeatable (Linux)
* `--security-opt <OPTION>` — A security option: `no-new-privileges=false` lets setuid programs inside gain privileges (they can't by default); `seccomp=FILE` filters system calls with a profile of your own, `seccomp=unconfined` not at all (Docker's profile by default). Repeatable (Linux)
* `--uidmap <CONTAINER:HOST:SIZE>` — Run in a user namespace: container IDs CONTAINER.. are host IDs HOST.., SIZE of them, e.g. 0:100000:65536 (root inside is host user 100000). Repeatable (Linux, root only: without root, the maps are chosen for you)
* `--gidmap <CONTAINER:HOST:SIZE>` — The same for group IDs; the --uidmap ranges if not given (Linux)
* `--bundle <DIR>` — Run the OCI bundle in DIR: its config.json says what to run and how, so no COMMAND or container options are given (Linux)
* `-m`, `--memory <SIZE>` — The most memory the container may use (such as 512m or 2g); past it, the kernel's OOM killer stops a process (Linux, root)
* `--memory-reservation <SIZE>` — Memory kept for the container when the host runs short (cgroup v2's memory.low)
* `--memory-swap <SIZE>` — Memory plus swap (needs --memory): -1 for unlimited swap. Without it, the container may swap as much as --memory, as with Docker
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



## `shipyard doctor`

Show how this host will run containers

**Usage:** `shipyard doctor`




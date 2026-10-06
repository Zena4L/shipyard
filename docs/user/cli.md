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
* `--stop-signal <SIGNAL>` — Signal sent to stop the command

  Default value: `SIGTERM`
* `--stop-timeout <SECONDS>` — Seconds to wait after the stop signal before killing the command

  Default value: `10`
* `--no-isolation` — Run without isolation: the command sees the host's processes, mounts, hostname and IPC. On Linux, commands are isolated by default (which needs root); on macOS they always run like this for now
* `--hostname <NAME>` — Hostname inside the container (Linux)
* `--domainname <NAME>` — NIS domain name inside the container (Linux)
* `--sysctl <KEY=VALUE>` — Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824 (Linux; IPC settings only)
* `--rootfs <DIR>` — Use DIR as the container's root filesystem: it becomes `/` inside, with its own /proc, /sys and /dev (Linux)
* `--read-only` — Make the container's root filesystem read-only (with --rootfs)
* `--bundle <DIR>` — Run the OCI bundle in DIR: its config.json says what to run and how, so no COMMAND or container options are given (Linux)



## `shipyard doctor`

Show how this host will run containers

**Usage:** `shipyard doctor`




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

* `run` — Run a command in a container: on Linux, isolated in namespaces (no separate filesystem yet)
* `doctor` — Show how this host will run containers

###### **Options:**

* `-D`, `--debug` — Show debug logs (same as --log-level debug)
* `--log-level <LEVEL>` — How much to log

  Possible values: `error`, `warn`, `info`, `debug`, `trace`




## `shipyard run`

Run a command in a container: on Linux, isolated in namespaces (no separate filesystem yet)

**Usage:** `shipyard run [OPTIONS] <COMMAND>...`

###### **Arguments:**

* `<COMMAND>` — The command to run, followed by its arguments

###### **Options:**

* `-e`, `--env <KEY[=VALUE]>` — Set an environment variable, or copy KEY from your shell if no value is given
* `-w`, `--workdir <DIR>` — Working directory inside the container (absolute path)

  Default value: `/`
* `-u`, `--user <UID[:GID]>` — Run as this numeric user and group (group defaults to 0)
* `--group-add <GID>` — Add a supplementary group (numeric)
* `--ulimit <NAME=SOFT[:HARD]>` — Set a resource limit, e.g. nofile=1024:2048 or core=0
* `--stop-signal <SIGNAL>` — Signal sent to stop the command

  Default value: `SIGTERM`
* `--stop-timeout <SECONDS>` — Seconds to wait after the stop signal before killing the command

  Default value: `10`
* `--no-isolation` — Run without isolation: the command sees the host's processes, mounts, hostname and IPC. On Linux, commands are isolated by default (which needs root); on macOS they always run like this for now
* `--hostname <NAME>` — Hostname inside the container (Linux)



## `shipyard doctor`

Show how this host will run containers

**Usage:** `shipyard doctor`




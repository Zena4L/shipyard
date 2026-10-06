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
* [`shipyard-runtime delete`↴](#shipyard-runtime-delete)

## `shipyard-runtime`

Runs containers from OCI bundles, one lifecycle step per call

**Usage:** `shipyard-runtime [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `create` — Create a container from a bundle, set up and waiting for `start`
* `start` — Run a created container's program
* `state` — Print a container's state as JSON
* `kill` — Send a signal to a container (default TERM)
* `delete` — Remove a stopped (or created) container

###### **Options:**

* `--root <DIR>` — Directory for the containers' state

  Default value: `/run/shipyard`
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

**Usage:** `shipyard-runtime kill <ID> [SIGNAL]`

###### **Arguments:**

* `<ID>` — The container's ID
* `<SIGNAL>` — A name (TERM, SIGKILL) or a number

  Default value: `TERM`



## `shipyard-runtime delete`

Remove a stopped (or created) container

**Usage:** `shipyard-runtime delete [OPTIONS] <ID>`

###### **Arguments:**

* `<ID>` — The container's ID

###### **Options:**

* `-f`, `--force` — Kill a running container first




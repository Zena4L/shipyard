<!-- Generated from the CLI's flag definitions. Do not edit by hand.
     Regenerate: UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference -->

# Command reference

This document contains the help content for the `shipyard` command-line program.

**Command Overview:**

* [`shipyard`↴](#shipyard)
* [`shipyard run`↴](#shipyard-run)
* [`shipyard pull`↴](#shipyard-pull)
* [`shipyard images`↴](#shipyard-images)
* [`shipyard rmi`↴](#shipyard-rmi)
* [`shipyard image`↴](#shipyard-image)
* [`shipyard image ls`↴](#shipyard-image-ls)
* [`shipyard image prune`↴](#shipyard-image-prune)
* [`shipyard volume`↴](#shipyard-volume)
* [`shipyard volume create`↴](#shipyard-volume-create)
* [`shipyard volume ls`↴](#shipyard-volume-ls)
* [`shipyard volume inspect`↴](#shipyard-volume-inspect)
* [`shipyard volume rm`↴](#shipyard-volume-rm)
* [`shipyard volume prune`↴](#shipyard-volume-prune)
* [`shipyard system`↴](#shipyard-system)
* [`shipyard system df`↴](#shipyard-system-df)
* [`shipyard doctor`↴](#shipyard-doctor)

## `shipyard`

A lightweight container engine

**Usage:** `shipyard [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `run` — Run a command in a container: on Linux, isolated in namespaces, optionally with its own root filesystem
* `pull` — Pull an image into the local store (and extract it, on Linux), so a run starts at once and needs no registry
* `images` — List images in the local store
* `rmi` — Remove one or more images from the local store (reference-counted; shared layers survive)
* `image` — Manage images
* `volume` — Manage named volumes
* `system` — System-wide information and maintenance
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
* `-w`, `--workdir <DIR>` — Working directory inside the container (absolute path): the image's, or `/`, if not given
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
* `--image <REF>` — Run a pulled image: its layers become the container's root filesystem, stacked read-only under a writable layer with overlayfs, and its baked-in command, environment, working directory and user are the defaults (Linux). Give a reference like `alpine:3.20` or `ghcr.io/owner/app@sha256:...`
* `--entrypoint <COMMAND>` — Run COMMAND instead of the image's entrypoint (and without its default arguments); `""` runs the command alone
* `--pull <WHEN>` — When to pull the image: `missing` (if the store doesn't have it), `always`, or `never` (the store's, or an error)

  Default value: `missing`

  Possible values:
  - `missing`:
    Only if the store doesn't have it whole
  - `always`:
    Every time (a tag may have moved)
  - `never`:
    Never: the store's, or nothing

* `--read-only` — Make the container's root filesystem read-only (with --rootfs or --image)
* `--cap-add <CAP>` — Give the command a capability on top of the default set, like `NET_ADMIN` (or `CAP_NET_ADMIN`); `ALL` for every one. Repeatable (Linux)
* `--cap-drop <CAP>` — Take a capability out of the default set; `ALL` for every one. Repeatable (Linux)
* `--security-opt <OPTION>` — A security option: `no-new-privileges=false` lets setuid programs inside gain privileges (they can't by default); `seccomp=FILE` filters system calls with a profile of your own, `seccomp=unconfined` not at all (Docker's profile by default). Repeatable (Linux)
* `--uidmap <CONTAINER:HOST:SIZE>` — Run in a user namespace: container IDs CONTAINER.. are host IDs HOST.., SIZE of them, e.g. 0:100000:65536 (root inside is host user 100000). Repeatable (Linux, root only: without root, the maps are chosen for you)
* `--gidmap <CONTAINER:HOST:SIZE>` — The same for group IDs; the --uidmap ranges if not given (Linux)
* `--bundle <DIR>` — Run the OCI bundle in DIR: its config.json says what to run and how, so no COMMAND or container options are given (Linux)
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
* `--volume <SOURCE:DEST[:ro]>` — Mount a named volume or host path into the container: `SOURCE:DEST[:ro]`. A bare SOURCE (no slash) is a named volume, created on first use; a path is bind-mounted. Repeatable (Linux, with a root filesystem)



## `shipyard pull`

Pull an image into the local store (and extract it, on Linux), so a run starts at once and needs no registry

**Usage:** `shipyard pull [OPTIONS] <IMAGE>`

###### **Arguments:**

* `<IMAGE>` — The image, like `alpine:3.20` or `ghcr.io/owner/app@sha256:...`

###### **Options:**

* `-q`, `--quiet` — Print nothing but the image's digest



## `shipyard images`

List images in the local store

**Usage:** `shipyard images`



## `shipyard rmi`

Remove one or more images from the local store (reference-counted; shared layers survive)

**Usage:** `shipyard rmi [OPTIONS] <IMAGE>...`

###### **Arguments:**

* `<IMAGE>` — The image references to remove

###### **Options:**

* `-f`, `--force` — Untag an image even while a running container uses it (its layers stay until the container ends and a prune collects them)



## `shipyard image`

Manage images

**Usage:** `shipyard image <COMMAND>`

###### **Subcommands:**

* `ls` — List images in the local store
* `prune` — Remove unreferenced images, layers and blobs from the store



## `shipyard image ls`

List images in the local store

**Usage:** `shipyard image ls`



## `shipyard image prune`

Remove unreferenced images, layers and blobs from the store

**Usage:** `shipyard image prune`



## `shipyard volume`

Manage named volumes

**Usage:** `shipyard volume <COMMAND>`

###### **Subcommands:**

* `create` — Create a named volume
* `ls` — List named volumes
* `inspect` — Show a volume's details
* `rm` — Remove one or more named volumes (never one a running container uses)
* `prune` — Remove every named volume no running container uses (asks first)



## `shipyard volume create`

Create a named volume

**Usage:** `shipyard volume create <NAME>`

###### **Arguments:**

* `<NAME>` — The volume's name



## `shipyard volume ls`

List named volumes

**Usage:** `shipyard volume ls`



## `shipyard volume inspect`

Show a volume's details

**Usage:** `shipyard volume inspect <NAME>`

###### **Arguments:**

* `<NAME>` — The volume's name



## `shipyard volume rm`

Remove one or more named volumes (never one a running container uses)

**Usage:** `shipyard volume rm <VOLUME>...`

###### **Arguments:**

* `<VOLUME>` — The volumes to remove



## `shipyard volume prune`

Remove every named volume no running container uses (asks first)

**Usage:** `shipyard volume prune [OPTIONS]`

###### **Options:**

* `-f`, `--force` — Don't ask (needed where there's no terminal to ask on)



## `shipyard system`

System-wide information and maintenance

**Usage:** `shipyard system <COMMAND>`

###### **Subcommands:**

* `df` — Show the store's disk usage



## `shipyard system df`

Show the store's disk usage

**Usage:** `shipyard system df`



## `shipyard doctor`

Show how this host will run containers

**Usage:** `shipyard doctor`




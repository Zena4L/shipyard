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
* [`shipyard network`↴](#shipyard-network)
* [`shipyard network create`↴](#shipyard-network-create)
* [`shipyard network ls`↴](#shipyard-network-ls)
* [`shipyard network inspect`↴](#shipyard-network-inspect)
* [`shipyard network rm`↴](#shipyard-network-rm)
* [`shipyard network prune`↴](#shipyard-network-prune)
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
* `network` — Manage networks (bridges containers share; Linux, root)
* `system` — System-wide information and maintenance
* `doctor` — Check whether this host can run containers, and what to fix

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
* `--no-isolation` — Run without isolation: the command sees the host's processes, mounts, hostname, IPC and network, as you. On Linux, commands are isolated by default (rootless, in a user namespace, without root); on macOS they always run like this for now
* `--hostname <NAME>` — Hostname inside the container (Linux)
* `--domainname <NAME>` — NIS domain name inside the container (Linux)
* `--sysctl <KEY=VALUE>` — Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824 (Linux): IPC settings, or net.* with a network of its own (any --network but host)
* `--network <MODE>` — The container's network: bridge (the default network: root's default), pasta (a network of its own through pasta: the default without root), host (the host's), none (one of its own with only a loopback interface), or a network's name (Linux; bridge and names need root). Repeatable for bridge networks: the container joins each, in order. name=NAME,ip=IP,ip6=IP,mac-address=MAC,alias=ALIAS gives one its own settings
* `--name <NAME>` — The container's name: on each network made with `shipyard network create` that it joins, other containers find it by this name (Linux). Letters, digits, _ and -
* `--network-alias <NAME>` — Another name the container answers to on each network made with `shipyard network create` that it joins. Repeatable (Linux)
* `--ip <IP>` — The container's address on its network (a bridge network's; with one network only: on several, name=NETWORK,ip=IP gives each its own)
* `--ip6 <IP>` — The container's IPv6 address on its network (one made with --ipv6; with one network only: on several, name=NETWORK,ip6=IP gives each its own)
* `--mac-address <MAC>` — The container's hardware address on its network (a bridge network's; with one network only), locally administered, like 02:42:0a:58:00:02
* `--add-host <NAME:IP>` — Add NAME with address IP to the container's /etc/hosts. Repeatable (Linux; with --image or --rootfs)
* `--dns <IP>` — A DNS server for the container's /etc/resolv.conf, instead of the host's. Repeatable (Linux; with --image or --rootfs)
* `--dns-search <DOMAIN>` — A DNS search domain, instead of the host's. Repeatable (Linux; with --image or --rootfs)
* `--dns-option <OPTION>` — A resolver option (like ndots:2), instead of the host's. Repeatable (Linux; with --image or --rootfs)
* `-p`, `--publish <SPEC>` — Publish a container's port on the host, like 8080:80 (the host's port 8080, on every IPv4 address, to the container's 80), 127.0.0.1:8080:80 (on that address only), 80 (a free host port) or 53:53/udp. Ranges work (8000-8010:80-90). Repeatable (Linux; a bridge network, or pasta)
* `-P`, `--publish-all` — Publish every port the image says it uses (its exposed ports) on free host ports (Linux; a bridge network, or pasta)
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



## `shipyard network`

Manage networks (bridges containers share; Linux, root)

**Usage:** `shipyard network <COMMAND>`

###### **Subcommands:**

* `create` — Create a network: a bridge with a subnet of its own
* `ls` — List networks
* `inspect` — Show networks' details, and the containers on them, as JSON
* `rm` — Remove one or more networks (never one a container uses, nor the default one)
* `prune` — Remove every network no container uses but the default one (asks first)



## `shipyard network create`

Create a network: a bridge with a subnet of its own

**Usage:** `shipyard network create [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — The network's name

###### **Options:**

* `--subnet <CIDR>` — Its subnet, like `10.90.0.0/24` (a free one by default); once more with an IPv6 prefix, like `fd00:1::/64`, for IPv6 too
* `--gateway <IP>` — Its gateway, the bridge's address (the subnet's first by default); once per family
* `--ipv6` — IPv6 too: a prefix of its own (a random ULA /64 unless --subnet gives one), and a way out through the host's IPv6 address
* `--internal` — No way out of it: containers on it reach each other and the host only



## `shipyard network ls`

List networks

**Usage:** `shipyard network ls`



## `shipyard network inspect`

Show networks' details, and the containers on them, as JSON

**Usage:** `shipyard network inspect <NETWORK>...`

###### **Arguments:**

* `<NETWORK>` — The networks



## `shipyard network rm`

Remove one or more networks (never one a container uses, nor the default one)

**Usage:** `shipyard network rm <NETWORK>...`

###### **Arguments:**

* `<NETWORK>` — The networks to remove



## `shipyard network prune`

Remove every network no container uses but the default one (asks first)

**Usage:** `shipyard network prune [OPTIONS]`

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

Check whether this host can run containers, and what to fix

**Usage:** `shipyard doctor`




<!-- Generated from the CLI's flag definitions. Do not edit by hand.
     Regenerate: UPDATE_DOCS=1 cargo test -p shipyard-cli cli_reference -->

# Command reference

This document contains the help content for the `shipyard` command-line program.

**Command Overview:**

* [`shipyard`↴](#shipyard)
* [`shipyard run`↴](#shipyard-run)
* [`shipyard create`↴](#shipyard-create)
* [`shipyard start`↴](#shipyard-start)
* [`shipyard attach`↴](#shipyard-attach)
* [`shipyard stop`↴](#shipyard-stop)
* [`shipyard restart`↴](#shipyard-restart)
* [`shipyard update`↴](#shipyard-update)
* [`shipyard kill`↴](#shipyard-kill)
* [`shipyard rename`↴](#shipyard-rename)
* [`shipyard exec`↴](#shipyard-exec)
* [`shipyard cp`↴](#shipyard-cp)
* [`shipyard logs`↴](#shipyard-logs)
* [`shipyard events`↴](#shipyard-events)
* [`shipyard wait`↴](#shipyard-wait)
* [`shipyard ps`↴](#shipyard-ps)
* [`shipyard port`↴](#shipyard-port)
* [`shipyard inspect`↴](#shipyard-inspect)
* [`shipyard rm`↴](#shipyard-rm)
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
* [`shipyard network connect`↴](#shipyard-network-connect)
* [`shipyard network disconnect`↴](#shipyard-network-disconnect)
* [`shipyard system`↴](#shipyard-system)
* [`shipyard system df`↴](#shipyard-system-df)
* [`shipyard system recover`↴](#shipyard-system-recover)
* [`shipyard system stop`↴](#shipyard-system-stop)
* [`shipyard vm`↴](#shipyard-vm)
* [`shipyard vm start`↴](#shipyard-vm-start)
* [`shipyard vm status`↴](#shipyard-vm-status)
* [`shipyard vm stop`↴](#shipyard-vm-stop)
* [`shipyard doctor`↴](#shipyard-doctor)

## `shipyard`

A lightweight container engine

**Usage:** `shipyard [OPTIONS] <COMMAND>`

###### **Subcommands:**

* `run` — Run an image in a container (its command, or COMMAND): on Linux, isolated in namespaces, with the image's files as its root. --no-image runs COMMAND on the host's files. With -d, in the background, by name
* `create` — Make a container, to start later (`shipyard start`): its image pulled, nothing run (Linux)
* `start` — Start containers, in the background (one running is left as it is), or one attached (-a)
* `attach` — Attach to a running container: its output, and your input (unless --no-stdin), until it ends (its exit code) or the detach keys
* `stop` — Stop running containers: their stop signal, then SIGKILL once their stop timeout is up. Returns once each has ended
* `restart` — Stop containers (as `stop`), then start them again
* `update` — Change containers' settings without making them again: their restart policy
* `kill` — Send a signal to running containers' main process (SIGKILL unless told), without waiting
* `rename` — Rename a container (a running one's name on its networks too, at once)
* `exec` — Run a command in a running container. Exits with its exit code (127 if it isn't found, 126 if it can't be run)
* `cp` — Copy files between a container (running or not) and the host: CONTAINER:PATH to a host path (or - for a tar stream on stdout), or back
* `logs` — Print a container's logs: every start's, oldest first
* `events` — Print what happens (containers made, started, ended; images pulled; volumes and networks made and removed) as it happens, or what did with --since
* `wait` — Wait for containers to end, and print each one's exit code
* `ps` — List containers: the running ones, or all of them with -a
* `port` — Show the host ports a container publishes (all of them, or those to one of its ports)
* `inspect` — Show containers in full, as JSON
* `rm` — Remove containers (their writable layers too). A running one needs -f
* `pull` — Pull an image into the local store (and extract it, on Linux), so a run starts at once and needs no registry
* `images` — List images in the local store
* `rmi` — Remove one or more images from the local store (reference-counted; shared layers survive)
* `image` — Manage images
* `volume` — Manage named volumes
* `network` — Manage networks (bridges containers share; Linux, root)
* `system` — System-wide information and maintenance
* `vm` — The Linux VM containers run in on a Mac: its status, start and stop
* `doctor` — Check whether this host can run containers, and what to fix

###### **Options:**

* `-D`, `--debug` — Show debug logs (same as --log-level debug)
* `--log-level <LEVEL>` — How much to log

  Possible values: `error`, `warn`, `info`, `debug`, `trace`




## `shipyard run`

Run an image in a container (its command, or COMMAND): on Linux, isolated in namespaces, with the image's files as its root. --no-image runs COMMAND on the host's files. With -d, in the background, by name

**Usage:**

```text
shipyard run [OPTIONS] IMAGE [COMMAND]...
shipyard run [OPTIONS] --no-image|--no-isolation|--rootfs DIR COMMAND...
shipyard run [OPTIONS] --bundle DIR
```

###### **Arguments:**

* `<IMAGE>` — The image to run (a reference like `alpine:3.20` or `ghcr.io/owner/app@sha256:...`), then the command to run instead of its own, with its arguments. With --no-image, --rootfs or --no-isolation there's no image: the command comes first (--bundle takes none). Everything after the image is the command's, options too

###### **Options:**

* `-d`, `--detach` — Run in the background: the container is made and started, its ID printed, and it runs on once this command has ended (Linux)
* `--rm` — Remove the container once it ends (a foreground run returns once it's gone)
* `--sig-proxy <BOOL>` — In the foreground: pass on the signals this command gets (Ctrl-C, a kill) to the container. With false, Ctrl-C ends this command and the container runs on

  Default value: `true`

  Possible values: `true`, `false`

* `--detach-keys <KEYS>` — In the foreground with -it: the keys that detach (the container goes on): ctrl-p,ctrl-q, or others separated by commas (empty: none)
* `-i`, `--interactive` — Pass your input on to the command. Without it, an isolated command's stdin is empty (/dev/null), so it can't take keys meant for your shell
* `-t`, `--tty` — Give the command a terminal of the container's own (a pseudo-terminal): full-screen programs, password prompts and job control work. With -i, what you type goes to it
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
* `--restart <POLICY>` — A container's restart policy, when it ends: no, on-failure (an exit code other than 0; on-failure:N at most N times), always, or unless-stopped. Never after you stop it. Not with --rm

  Default value: `no`
* `--health-cmd <CMD>` — A container's health check: CMD, run in the container by the image's shell (its SHELL, else /bin/sh -c) every --health-interval while it runs; its exit code says whether the container is well (0) or not. `ps` and `inspect` show what it says: starting, healthy or unhealthy. Without it, the image's own check (its HEALTHCHECK), which the other --health-* flags tune
* `--no-healthcheck` — No health check, not even the image's own
* `--health-interval <DURATION>` — Between two health checks (the image's check's, else 30s): a duration, such as 500ms, 30s, 1m or 2h
* `--health-timeout <DURATION>` — The longest a health check may take (the image's check's, else 30s): then it's a failure
* `--health-start-period <DURATION>` — After a start, how long failed health checks don't count (the image's check's, else none)
* `--health-start-interval <DURATION>` — Between two health checks during the start period (the image's check's, else 5s)
* `--health-retries <N>` — How many failed health checks in a row make the container unhealthy (the image's check's, else 3)
* `--no-isolation` — Run without isolation: the command sees the host's processes, mounts, hostname, IPC and network, as you. On Linux, commands are isolated by default (rootless, in a user namespace, without root); on macOS they always run like this for now
* `--preserve-fds <N>` — Pass N more descriptors on to the command, from 3 up (after socket activation's, if `LISTEN_FDS` and `LISTEN_PID` give it any), as Podman does

  Default value: `0`
* `--hostname <NAME>` — Hostname inside the container (Linux)
* `--domainname <NAME>` — NIS domain name inside the container (Linux)
* `--sysctl <KEY=VALUE>` — Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824 (Linux): IPC settings, or net.* with a network of its own (any --network but host)
* `--network <MODE>` — The container's network: bridge (the default network: root's default), pasta (a network of its own through pasta: the default without root), host (the host's), none (one of its own with only a loopback interface), or a network's name (Linux; bridge and names need root). Repeatable for bridge networks: the container joins each, in order. name=NAME,ip=IP,ip6=IP,mac-address=MAC,alias=ALIAS gives one its own settings
* `--name <NAME>` — The container's name: on each network made with `shipyard network create` that it joins, other containers find it by this name (Linux). Letters, digits, _ and -
* `--network-alias <NAME>` — Another name the container answers to on each network made with `shipyard network create` that it joins. Repeatable (Linux)
* `--ip <IP>` — The container's address on its network (a bridge network's; with one network only: on several, name=NETWORK,ip=IP gives each its own)
* `--ip6 <IP>` — The container's IPv6 address on its network (one made with --ipv6; with one network only: on several, name=NETWORK,ip6=IP gives each its own)
* `--mac-address <MAC>` — The container's hardware address on its network (a bridge network's; with one network only), locally administered, like 02:42:0a:58:00:02
* `--add-host <NAME:IP>` — Add NAME with address IP to the container's /etc/hosts. Repeatable (Linux; with an image or --rootfs)
* `--dns <IP>` — A DNS server for the container's /etc/resolv.conf, instead of the host's. Repeatable (Linux; with an image or --rootfs)
* `--dns-search <DOMAIN>` — A DNS search domain, instead of the host's. Repeatable (Linux; with an image or --rootfs)
* `--dns-option <OPTION>` — A resolver option (like ndots:2), instead of the host's. Repeatable (Linux; with an image or --rootfs)
* `-p`, `--publish <SPEC>` — Publish a container's port on the host, like 8080:80 (the host's port 8080, on every IPv4 address, to the container's 80), 127.0.0.1:8080:80 (on that address only), 80 (a free host port) or 53:53/udp. Ranges work (8000-8010:80-90). Repeatable (Linux; a bridge network, or pasta)
* `-P`, `--publish-all` — Publish every port the image says it uses (its exposed ports) on free host ports (Linux; a bridge network, or pasta)
* `--rootfs <DIR>` — Use DIR as the container's root filesystem: it becomes `/` inside, with its own /proc, /sys and /dev (Linux)
* `--no-image` — Run without an image: the command sees the host's files, isolated in its own namespaces otherwise (Linux), and COMMAND comes first
* `--entrypoint <COMMAND>` — Run COMMAND instead of the image's entrypoint (and without its default arguments); `""` runs the command alone
* `--pull <WHEN>` — When to pull the image: `missing` (if the store doesn't have it: the default), `always`, or `never` (the store's, or an error)

  Possible values:
  - `missing`:
    Only if the store doesn't have it whole
  - `always`:
    Every time (a tag may have moved)
  - `never`:
    Never: the store's, or nothing

* `--read-only` — Make the container's root filesystem read-only (with an image or --rootfs)
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



## `shipyard create`

Make a container, to start later (`shipyard start`): its image pulled, nothing run (Linux)

**Usage:**

```text
shipyard create [OPTIONS] IMAGE [COMMAND]...
shipyard create [OPTIONS] --rootfs DIR COMMAND...
```

###### **Arguments:**

* `<IMAGE>` — The image to run (a reference like `alpine:3.20` or `ghcr.io/owner/app@sha256:...`), then the command to run instead of its own, with its arguments. With --no-image, --rootfs or --no-isolation there's no image: the command comes first (--bundle takes none). Everything after the image is the command's, options too

###### **Options:**

* `--rm` — Remove the container once it ends, after its first start
* `-i`, `--interactive` — Pass your input on to the command. Without it, an isolated command's stdin is empty (/dev/null), so it can't take keys meant for your shell
* `-t`, `--tty` — Give the command a terminal of the container's own (a pseudo-terminal): full-screen programs, password prompts and job control work. With -i, what you type goes to it
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
* `--restart <POLICY>` — A container's restart policy, when it ends: no, on-failure (an exit code other than 0; on-failure:N at most N times), always, or unless-stopped. Never after you stop it. Not with --rm

  Default value: `no`
* `--health-cmd <CMD>` — A container's health check: CMD, run in the container by the image's shell (its SHELL, else /bin/sh -c) every --health-interval while it runs; its exit code says whether the container is well (0) or not. `ps` and `inspect` show what it says: starting, healthy or unhealthy. Without it, the image's own check (its HEALTHCHECK), which the other --health-* flags tune
* `--no-healthcheck` — No health check, not even the image's own
* `--health-interval <DURATION>` — Between two health checks (the image's check's, else 30s): a duration, such as 500ms, 30s, 1m or 2h
* `--health-timeout <DURATION>` — The longest a health check may take (the image's check's, else 30s): then it's a failure
* `--health-start-period <DURATION>` — After a start, how long failed health checks don't count (the image's check's, else none)
* `--health-start-interval <DURATION>` — Between two health checks during the start period (the image's check's, else 5s)
* `--health-retries <N>` — How many failed health checks in a row make the container unhealthy (the image's check's, else 3)
* `--no-isolation` — Run without isolation: the command sees the host's processes, mounts, hostname, IPC and network, as you. On Linux, commands are isolated by default (rootless, in a user namespace, without root); on macOS they always run like this for now
* `--preserve-fds <N>` — Pass N more descriptors on to the command, from 3 up (after socket activation's, if `LISTEN_FDS` and `LISTEN_PID` give it any), as Podman does

  Default value: `0`
* `--hostname <NAME>` — Hostname inside the container (Linux)
* `--domainname <NAME>` — NIS domain name inside the container (Linux)
* `--sysctl <KEY=VALUE>` — Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824 (Linux): IPC settings, or net.* with a network of its own (any --network but host)
* `--network <MODE>` — The container's network: bridge (the default network: root's default), pasta (a network of its own through pasta: the default without root), host (the host's), none (one of its own with only a loopback interface), or a network's name (Linux; bridge and names need root). Repeatable for bridge networks: the container joins each, in order. name=NAME,ip=IP,ip6=IP,mac-address=MAC,alias=ALIAS gives one its own settings
* `--name <NAME>` — The container's name: on each network made with `shipyard network create` that it joins, other containers find it by this name (Linux). Letters, digits, _ and -
* `--network-alias <NAME>` — Another name the container answers to on each network made with `shipyard network create` that it joins. Repeatable (Linux)
* `--ip <IP>` — The container's address on its network (a bridge network's; with one network only: on several, name=NETWORK,ip=IP gives each its own)
* `--ip6 <IP>` — The container's IPv6 address on its network (one made with --ipv6; with one network only: on several, name=NETWORK,ip6=IP gives each its own)
* `--mac-address <MAC>` — The container's hardware address on its network (a bridge network's; with one network only), locally administered, like 02:42:0a:58:00:02
* `--add-host <NAME:IP>` — Add NAME with address IP to the container's /etc/hosts. Repeatable (Linux; with an image or --rootfs)
* `--dns <IP>` — A DNS server for the container's /etc/resolv.conf, instead of the host's. Repeatable (Linux; with an image or --rootfs)
* `--dns-search <DOMAIN>` — A DNS search domain, instead of the host's. Repeatable (Linux; with an image or --rootfs)
* `--dns-option <OPTION>` — A resolver option (like ndots:2), instead of the host's. Repeatable (Linux; with an image or --rootfs)
* `-p`, `--publish <SPEC>` — Publish a container's port on the host, like 8080:80 (the host's port 8080, on every IPv4 address, to the container's 80), 127.0.0.1:8080:80 (on that address only), 80 (a free host port) or 53:53/udp. Ranges work (8000-8010:80-90). Repeatable (Linux; a bridge network, or pasta)
* `-P`, `--publish-all` — Publish every port the image says it uses (its exposed ports) on free host ports (Linux; a bridge network, or pasta)
* `--rootfs <DIR>` — Use DIR as the container's root filesystem: it becomes `/` inside, with its own /proc, /sys and /dev (Linux)
* `--no-image` — Run without an image: the command sees the host's files, isolated in its own namespaces otherwise (Linux), and COMMAND comes first
* `--entrypoint <COMMAND>` — Run COMMAND instead of the image's entrypoint (and without its default arguments); `""` runs the command alone
* `--pull <WHEN>` — When to pull the image: `missing` (if the store doesn't have it: the default), `always`, or `never` (the store's, or an error)

  Possible values:
  - `missing`:
    Only if the store doesn't have it whole
  - `always`:
    Every time (a tag may have moved)
  - `never`:
    Never: the store's, or nothing

* `--read-only` — Make the container's root filesystem read-only (with an image or --rootfs)
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



## `shipyard start`

Start containers, in the background (one running is left as it is), or one attached (-a)

**Usage:** `shipyard start [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-a`, `--attach` — Attach to it as it starts (one container): its output from its first byte, until it ends (its exit code) or Ctrl-P Ctrl-Q
* `-i`, `--interactive` — With -a: pass your input on to it
* `--detach-keys <KEYS>` — With -a: the keys that detach (the container goes on), as attach's

  Default value: `ctrl-p,ctrl-q`



## `shipyard attach`

Attach to a running container: its output, and your input (unless --no-stdin), until it ends (its exit code) or the detach keys

**Usage:** `shipyard attach [OPTIONS] <CONTAINER>`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix

###### **Options:**

* `--detach-keys <KEYS>` — The keys that detach (the container goes on): ctrl-p,ctrl-q, or others separated by commas (empty: none)

  Default value: `ctrl-p,ctrl-q`
* `--no-stdin` — Don't pass your input on



## `shipyard stop`

Stop running containers: their stop signal, then SIGKILL once their stop timeout is up. Returns once each has ended

**Usage:** `shipyard stop [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-t`, `--time <SECONDS>` — Seconds to wait after the stop signal before SIGKILL (the container's own, its --stop-timeout, unless given)
* `-s`, `--signal <SIGNAL>` — The signal sent first (the container's own, its --stop-signal, unless given)



## `shipyard restart`

Stop containers (as `stop`), then start them again

**Usage:** `shipyard restart [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-t`, `--time <SECONDS>` — Seconds to wait after the stop signal before SIGKILL (the container's own, its --stop-timeout, unless given)
* `-s`, `--signal <SIGNAL>` — The signal sent first (the container's own, its --stop-signal, unless given)



## `shipyard update`

Change containers' settings without making them again: their restart policy

**Usage:** `shipyard update --restart <POLICY> <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `--restart <POLICY>` — The restart policy: no, on-failure (or on-failure:N), always or unless-stopped



## `shipyard kill`

Send a signal to running containers' main process (SIGKILL unless told), without waiting

**Usage:** `shipyard kill [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-s`, `--signal <SIGNAL>` — The signal: a name (KILL, SIGTERM) or a number

  Default value: `SIGKILL`



## `shipyard rename`

Rename a container (a running one's name on its networks too, at once)

**Usage:** `shipyard rename <CONTAINER> <NEW_NAME>`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix
* `<NEW_NAME>` — Its new name



## `shipyard exec`

Run a command in a running container. Exits with its exit code (127 if it isn't found, 126 if it can't be run)

**Usage:** `shipyard exec [OPTIONS] <CONTAINER> <COMMAND>...`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix
* `<COMMAND>` — The command and its arguments

###### **Options:**

* `-i`, `--interactive` — Pass your input on to it (without: it reads end-of-file)
* `-t`, `--tty` — Give it a terminal
* `-d`, `--detach` — Return once it runs (it goes on)
* `-u`, `--user <USER[:GROUP]>` — Run it as this user, and group if given: names (from the container's own /etc/passwd and /etc/group) or numbers
* `-e`, `--env <KEY=VALUE>` — Set an environment variable for it (KEY=VALUE; repeatable)
* `-w`, `--workdir <DIR>` — Its working directory, in the container



## `shipyard cp`

Copy files between a container (running or not) and the host: CONTAINER:PATH to a host path (or - for a tar stream on stdout), or back

**Usage:** `shipyard cp [OPTIONS] <SOURCE> <DESTINATION>`

###### **Arguments:**

* `<SOURCE>` — From: CONTAINER:PATH, a host path, or - (a tar stream on stdin)
* `<DESTINATION>` — To: CONTAINER:PATH, a host path, or - (a tar stream on stdout)

###### **Options:**

* `-a`, `--archive` — Keep the files' owners (where the container has those IDs)
* `-L`, `--follow-link` — Follow a source that's a symbolic link



## `shipyard logs`

Print a container's logs: every start's, oldest first

**Usage:** `shipyard logs [OPTIONS] <CONTAINER>`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix

###### **Options:**

* `-f`, `--follow` — Then print more as it comes, until the container stops (or is started again)
* `--tail <N>` — Only the last N lines (across its starts)
* `--since <TIME>` — Only lines from TIME on: RFC 3339 (2026-10-10T06:16:19Z), Unix seconds, or a duration ago (10m, 2h, 30s, 3d)
* `-t`, `--timestamps` — Start each line with the time it was written (RFC 3339)



## `shipyard events`

Print what happens (containers made, started, ended; images pulled; volumes and networks made and removed) as it happens, or what did with --since

**Usage:** `shipyard events [OPTIONS]`

###### **Options:**

* `--since <TIME>` — Past events too, from TIME on: RFC 3339 (2026-10-10T06:16:19Z), Unix seconds, or a duration ago (10m, 2h, 3d). Without it, only new ones
* `--until <TIME>` — Stop at TIME (as --since): with a past TIME, print and exit. Without it, wait for more until interrupted
* `-f`, `--filter <KEY=VALUE>` — Only the events that match: type=container|image|volume|network, event=ACTION, container=NAME|ID, image=REF, volume=NAME, network=NAME. Repeat a key for any of its values
* `--format <FORMAT>` — How to print them: Docker's line, or JSON (one object per line, as the journal keeps it)

  Default value: `text`

  Possible values:
  - `text`:
    `TIME TYPE ACTION ID (KEY=VALUE, …)`
  - `json`:
    The journal's line




## `shipyard wait`

Wait for containers to end, and print each one's exit code

**Usage:** `shipyard wait <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes



## `shipyard ps`

List containers: the running ones, or all of them with -a

**Usage:** `shipyard ps [OPTIONS]`

###### **Options:**

* `-a`, `--all` — All containers, not only the ones up
* `-q`, `--quiet` — Only their IDs (short)
* `--format <FORMAT>` — How to print them: a table, or JSON (one object per container)

  Possible values:
  - `table`:
    A table, as Docker's
  - `json`:
    A JSON array, one object per container (or socket)




## `shipyard port`

Show the host ports a container publishes (all of them, or those to one of its ports)

**Usage:** `shipyard port [OPTIONS] <CONTAINER> [PORT[/PROTO]]`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix
* `<PORT[/PROTO]>` — One of its ports: 80, 80/tcp or 53/udp (tcp if not said)

###### **Options:**

* `--sockets` — Show its sockets instead: what it listens on, and its connections, each peer by its container's name where it's one (Linux)
* `--format <FORMAT>` — How to show its sockets: a table, or JSON

  Possible values:
  - `table`:
    A table, as Docker's
  - `json`:
    A JSON array, one object per container (or socket)




## `shipyard inspect`

Show containers in full, as JSON

**Usage:** `shipyard inspect <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes



## `shipyard rm`

Remove containers (their writable layers too). A running one needs -f

**Usage:** `shipyard rm [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-f`, `--force` — Remove a running container too (it's killed first)



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

* `-f`, `--force` — Untag an image even while a container uses it (a container keeps what it needs until it's removed; a foreground run's layers stay until it ends and a prune collects them)



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
* `prune` — Remove every named volume no container uses, running or not (asks first)



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

Remove every named volume no container uses, running or not (asks first)

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
* `connect` — Connect a container to a network: at once if it's running, at its next start otherwise
* `disconnect` — Disconnect a container from a network: at once if it's running, at its next start otherwise



## `shipyard network create`

Create a network: a bridge with a subnet of its own

**Usage:** `shipyard network create [OPTIONS] <NAME>`

###### **Arguments:**

* `<NAME>` — The network's name

###### **Options:**

* `--subnet <CIDR>` — Its subnet, like `10.90.0.0/24` (a free one by default); once more with an IPv6 prefix, like `fd00:1::/64`, for IPv6 too
* `--gateway <IP>` — Its gateway, the bridge's address (the subnet's first by default); once per family
* `--ipv6` — IPv6 too: a prefix of its own (a random ULA /64 unless --subnet gives one), and a way out through the host's IPv6 address
* `--internal` — No way out of it: containers on it reach each other and the host only (each other only, with --isolate-host)
* `--isolate-host` — Keep its containers from the host itself (its services, on any of its addresses) and from the cloud's metadata addresses, but for the network's DNS server. Not for the default network
* `--conntrack-limit` — Limit each container address on it to 8192 open connections (of those it opens: to the world, the host and other networks); more are refused



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



## `shipyard network connect`

Connect a container to a network: at once if it's running, at its next start otherwise

**Usage:** `shipyard network connect [OPTIONS] <NETWORK> <CONTAINER>`

###### **Arguments:**

* `<NETWORK>` — The network
* `<CONTAINER>` — The container: its name, ID, serial or ID prefix

###### **Options:**

* `--ip <IP>` — Its address on the network (the next free one if not given)
* `--ip6 <IP>` — Its IPv6 address on the network (one that has IPv6)
* `--alias <NAME>` — Another name it answers to on the network (repeatable; a network made with `network create`)



## `shipyard network disconnect`

Disconnect a container from a network: at once if it's running, at its next start otherwise

**Usage:** `shipyard network disconnect [OPTIONS] <NETWORK> <CONTAINER>`

###### **Arguments:**

* `<NETWORK>` — The network
* `<CONTAINER>` — The container: its name, ID, serial or ID prefix

###### **Options:**

* `-f`, `--force` — Disconnect it from the network that carries its way out (its default route and published ports) too



## `shipyard system`

System-wide information and maintenance

**Usage:** `shipyard system <COMMAND>`

###### **Subcommands:**

* `df` — Show the store's disk usage
* `recover` — Settle containers that ended unwatched, then start those whose restart policy brings them back when the host starts (what the boot unit runs: see contrib/systemd)
* `stop` — Stop every container that's up, for the host's shutdown, each with its own stop signal and timeout: not as your stop (their restart policy brings them back at boot), and none restarted until `system recover` (what shipyard-stop.service runs at a shutdown)



## `shipyard system df`

Show the store's disk usage

**Usage:** `shipyard system df`



## `shipyard system recover`

Settle containers that ended unwatched, then start those whose restart policy brings them back when the host starts (what the boot unit runs: see contrib/systemd)

**Usage:** `shipyard system recover`



## `shipyard system stop`

Stop every container that's up, for the host's shutdown, each with its own stop signal and timeout: not as your stop (their restart policy brings them back at boot), and none restarted until `system recover` (what shipyard-stop.service runs at a shutdown)

**Usage:** `shipyard system stop`



## `shipyard vm`

The Linux VM containers run in on a Mac: its status, start and stop

**Usage:** `shipyard vm <COMMAND>`

###### **Subcommands:**

* `start` — Start the VM, if it isn't running, and wait for its guest to boot
* `status` — Say whether the VM runs, which guest it boots, and the ports it forwards to this Mac
* `stop` — Stop the VM: its containers stopped in order, then the guest powered off



## `shipyard vm start`

Start the VM, if it isn't running, and wait for its guest to boot

**Usage:** `shipyard vm start`



## `shipyard vm status`

Say whether the VM runs, which guest it boots, and the ports it forwards to this Mac

**Usage:** `shipyard vm status`



## `shipyard vm stop`

Stop the VM: its containers stopped in order, then the guest powered off

**Usage:** `shipyard vm stop`



## `shipyard doctor`

Check whether this host can run containers, and what to fix

**Usage:** `shipyard doctor`




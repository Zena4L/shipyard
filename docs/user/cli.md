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
* [`shipyard pause`↴](#shipyard-pause)
* [`shipyard unpause`↴](#shipyard-unpause)
* [`shipyard top`↴](#shipyard-top)
* [`shipyard stats`↴](#shipyard-stats)
* [`shipyard compose`↴](#shipyard-compose)
* [`shipyard rename`↴](#shipyard-rename)
* [`shipyard exec`↴](#shipyard-exec)
* [`shipyard cp`↴](#shipyard-cp)
* [`shipyard export`↴](#shipyard-export)
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
* [`shipyard tag`↴](#shipyard-tag)
* [`shipyard save`↴](#shipyard-save)
* [`shipyard load`↴](#shipyard-load)
* [`shipyard build`↴](#shipyard-build)
* [`shipyard history`↴](#shipyard-history)
* [`shipyard push`↴](#shipyard-push)
* [`shipyard login`↴](#shipyard-login)
* [`shipyard logout`↴](#shipyard-logout)
* [`shipyard import`↴](#shipyard-import)
* [`shipyard container`↴](#shipyard-container)
* [`shipyard container ls`↴](#shipyard-container-ls)
* [`shipyard container run`↴](#shipyard-container-run)
* [`shipyard container create`↴](#shipyard-container-create)
* [`shipyard container start`↴](#shipyard-container-start)
* [`shipyard container stop`↴](#shipyard-container-stop)
* [`shipyard container restart`↴](#shipyard-container-restart)
* [`shipyard container kill`↴](#shipyard-container-kill)
* [`shipyard container pause`↴](#shipyard-container-pause)
* [`shipyard container unpause`↴](#shipyard-container-unpause)
* [`shipyard container top`↴](#shipyard-container-top)
* [`shipyard container stats`↴](#shipyard-container-stats)
* [`shipyard container rm`↴](#shipyard-container-rm)
* [`shipyard container logs`↴](#shipyard-container-logs)
* [`shipyard container exec`↴](#shipyard-container-exec)
* [`shipyard container inspect`↴](#shipyard-container-inspect)
* [`shipyard container cp`↴](#shipyard-container-cp)
* [`shipyard container export`↴](#shipyard-container-export)
* [`shipyard container port`↴](#shipyard-container-port)
* [`shipyard container rename`↴](#shipyard-container-rename)
* [`shipyard container wait`↴](#shipyard-container-wait)
* [`shipyard container attach`↴](#shipyard-container-attach)
* [`shipyard container update`↴](#shipyard-container-update)
* [`shipyard container prune`↴](#shipyard-container-prune)
* [`shipyard image`↴](#shipyard-image)
* [`shipyard image ls`↴](#shipyard-image-ls)
* [`shipyard image prune`↴](#shipyard-image-prune)
* [`shipyard image rm`↴](#shipyard-image-rm)
* [`shipyard image pull`↴](#shipyard-image-pull)
* [`shipyard image tag`↴](#shipyard-image-tag)
* [`shipyard image save`↴](#shipyard-image-save)
* [`shipyard image load`↴](#shipyard-image-load)
* [`shipyard image build`↴](#shipyard-image-build)
* [`shipyard image history`↴](#shipyard-image-history)
* [`shipyard image import`↴](#shipyard-image-import)
* [`shipyard image push`↴](#shipyard-image-push)
* [`shipyard image inspect`↴](#shipyard-image-inspect)
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
* [`shipyard system prune`↴](#shipyard-system-prune)
* [`shipyard system df`↴](#shipyard-system-df)
* [`shipyard system recover`↴](#shipyard-system-recover)
* [`shipyard system stop`↴](#shipyard-system-stop)
* [`shipyard system service`↴](#shipyard-system-service)
* [`shipyard vm`↴](#shipyard-vm)
* [`shipyard vm start`↴](#shipyard-vm-start)
* [`shipyard vm status`↴](#shipyard-vm-status)
* [`shipyard vm stop`↴](#shipyard-vm-stop)
* [`shipyard vm install`↴](#shipyard-vm-install)
* [`shipyard vm uninstall`↴](#shipyard-vm-uninstall)
* [`shipyard vm set`↴](#shipyard-vm-set)
* [`shipyard builder`↴](#shipyard-builder)
* [`shipyard builder prune`↴](#shipyard-builder-prune)
* [`shipyard doctor`↴](#shipyard-doctor)
* [`shipyard version`↴](#shipyard-version)
* [`shipyard info`↴](#shipyard-info)

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
* `update` — Change containers' settings without making them again: their restart policy, and their resource limits (in their cgroup at once while they run, and for their next starts)
* `kill` — Send a signal to running containers' main process (SIGKILL unless told), without waiting
* `pause` — Freeze running containers' processes where they are (their cgroup's freezer), until `unpause`
* `unpause` — Let paused containers' processes run on
* `top` — Show a running container's processes, in `ps -ef`'s columns
* `stats` — Show what containers use (CPU, memory, network, block I/O, processes), every second until Ctrl-C, or once with --no-stream
* `compose` — Run Docker's `docker compose` against Shipyard (its API's service started if nothing serves it yet): `shipyard compose up -d`. Needs Docker's CLI and its compose plugin
* `rename` — Rename a container (a running one's name on its networks too, at once)
* `exec` — Run a command in a running container. Exits with its exit code (127 if it isn't found, 126 if it can't be run)
* `cp` — Copy files between a container (running or not) and the host: CONTAINER:PATH to a host path (or - for a tar stream on stdout), or back
* `export` — Write a container's own filesystem (never its volumes) as a tar archive, to standard output or -o FILE; running or stopped
* `logs` — Print a container's logs: every start's, oldest first
* `events` — Print what happens (containers made, started, ended; images pulled; volumes and networks made and removed) as it happens, or what did with --since
* `wait` — Wait for containers to end, and print each one's exit code
* `ps` — List containers: the running ones, or all of them with -a
* `port` — Show the host ports a container publishes (all of them, or those to one of its ports)
* `inspect` — Show objects in full, as JSON: a container, an image, a network or a volume, by name, in Docker's shape (`docker inspect`'s)
* `rm` — Remove containers (their writable layers too). A running one needs -f
* `pull` — Pull an image into the local store (and extract it, on Linux), so a run starts at once and needs no registry
* `images` — List images in the local store
* `rmi` — Remove one or more images from the local store (reference-counted; shared layers survive)
* `tag` — Give an image another name (TARGET, a reference) beside its own
* `save` — Write images to a tar archive (to standard output, or -o FILE), as Docker 25 writes one: an OCI image layout with Docker's `manifest.json`
* `load` — Load images from a tar archive (from standard input, or -i FILE): Docker's (`docker save`'s) or an OCI layout (BuildKit's), plain or gzipped
* `build` — Build an image from a Dockerfile, with BuildKit (in a container of its own, gone at the build's end): named with -t
* `history` — Show how an image was built: a row per step, newest first
* `push` — Send an image to its registry (the name `tag` gave it: `NAME[:TAG]`), signed in as `login` (or `docker login`) left you
* `login` — Sign in to a registry (Docker Hub if none), kept where Docker keeps it: its credential helper, else ~/.docker/config.json
* `logout` — Sign out of a registry (Docker Hub if none)
* `import` — Make an image of a root filesystem's tar archive (a file, or - for standard input; plain or gzipped): one layer, named
* `container` — Manage containers (the same commands as at the top: `container ls` is `ps`, `container rm` is `rm`…)
* `image` — Manage images
* `volume` — Manage named volumes
* `network` — Manage networks (bridges containers share; Linux, root)
* `system` — System-wide information and maintenance
* `vm` — The Linux VM containers run in on a Mac: its status, start and stop
* `builder` — The build cache
* `doctor` — Check whether this host can run containers, and what to fix
* `version` — Show Shipyard's version and its host's (`--format` a Go template over `{Client, Server}`, as Docker's)
* `info` — Show what Shipyard has and what its host is (`--format` a Go template over Docker's `SystemInfo`)

###### **Options:**

* `-v`, `--version` — Print version (`-v` as Docker's, `-V` too)

  Possible values: `true`, `false`

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
* `-a`, `--attach <STREAM>` — In the foreground: attach only these streams (stdin, stdout, stderr; repeatable). stdin takes your input, as -i. Without it, all of them
* `--help` — Print help (`-h` is the hostname's, as Docker's)

  Possible values: `true`, `false`

* `-i`, `--interactive` — Pass your input on to the command. Without it, an isolated command's stdin is empty (/dev/null), so it can't take keys meant for your shell
* `-t`, `--tty` — Give the command a terminal of the container's own (a pseudo-terminal): full-screen programs, password prompts and job control work. With -i, what you type goes to it
* `-e`, `--env <KEY[=VALUE]>` — Set an environment variable, or copy KEY from your shell if no value is given
* `--env-file <FILE>` — Read variables from a file, as Docker's: a KEY=VALUE per line (a bare KEY copies yours), # lines and blank ones skipped, values as they are. Before --env, which wins. Repeatable
* `-l`, `--label <KEY[=VALUE]>` — Set a label on the container: KEY=VALUE, or KEY (an empty value). Repeatable
* `--label-file <FILE>` — Read labels from a file: a KEY=VALUE (or KEY) per line, # lines and blank ones skipped. Before --label, which wins. Repeatable
* `--cidfile <FILE>` — Write the container's ID to FILE once it's made (refused if FILE is there already)
* `-q`, `--quiet` — Print no pull progress
* `--privileged` — Not supported: give the container the capabilities (--cap-add) and devices (--device) it needs instead
* `--init` — Not supported yet: run the command without it (an image's own init, if it has one)
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
* `-h`, `--hostname <NAME>` — Hostname inside the container (Linux)
* `--domainname <NAME>` — NIS domain name inside the container (Linux)
* `--sysctl <KEY=VALUE>` — Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824 (Linux): IPC settings, or net.* with a network of its own (any --network but host and container:)
* `--network <MODE>` — The container's network: bridge (the default network: root's default), pasta (a network of its own through pasta: the default without root), host (the host's), none (one of its own with only a loopback interface), container:NAME (another running container's, as it is), or a network's name (Linux; bridge and names need root). Repeatable for bridge networks: the container joins each, in order. name=NAME,ip=IP,ip6=IP,mac-address=MAC,alias=ALIAS gives one its own settings
* `--ipc <MODE>` — The container's IPC namespace (Linux): private or shareable (its own: any running container's can be joined), or container:NAME (another running container's, with its /dev/shm)
* `--pid <MODE>` — The container's PID namespace (Linux): its own unless given; host (the host's: it sees and can signal every process there; root only), or container:NAME (another running container's: each sees the other's processes)
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
* `--expose <PORT[/PROTO]>` — Expose a port, or a range (8000-8010), TCP unless /udp: published with -P beside the image's own, and shown by inspect. Repeatable
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

* `--platform <PLATFORM>` — The image's platform (`linux/amd64`: on a Mac, run through Rosetta); by default the image's own among what runs here, this host's first
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
* `-v`, `--volume <[SOURCE:]DEST[:OPTIONS]>` — Mount a named volume or host path into the container: `[SOURCE:]DEST[:MODES]`, MODES `ro` and `nocopy`, comma-joined. A bare SOURCE (no slash) is a named volume, created on first use; DEST alone, a new anonymous one; either, if empty, is filled with the image's files at DEST when the container is made (unless `nocopy`). A path is bind-mounted (it must exist); DEST alone is a new anonymous volume. Repeatable (Linux, with a root filesystem)
* `--tmpfs <PATH[:OPTIONS]>` — Mount a tmpfs (in memory, empty at each start) at PATH, with `mount(8)`'s OPTIONS: `size=64m`, `mode=1777`, `uid`, `gid`, `nr_inodes`, `ro`, and `exec`, `suid`, `dev` (it's `noexec`, `nosuid`, `nodev` without them, as Docker's). Repeatable
* `--mount <KEY=VALUE,...>` — Mount as Docker's `--mount` does: `type=bind,source=/path,target=/d`, `type=volume,source=NAME,target=/d` (the default type), or `type=tmpfs,target=/d,tmpfs-size=64m,tmpfs-mode=1777`; `readonly` for any, `volume-nocopy` for a volume (not filled from the image). Repeatable



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
* `--help` — Print help (`-h` is the hostname's, as Docker's)

  Possible values: `true`, `false`

* `-i`, `--interactive` — Pass your input on to the command. Without it, an isolated command's stdin is empty (/dev/null), so it can't take keys meant for your shell
* `-t`, `--tty` — Give the command a terminal of the container's own (a pseudo-terminal): full-screen programs, password prompts and job control work. With -i, what you type goes to it
* `-e`, `--env <KEY[=VALUE]>` — Set an environment variable, or copy KEY from your shell if no value is given
* `--env-file <FILE>` — Read variables from a file, as Docker's: a KEY=VALUE per line (a bare KEY copies yours), # lines and blank ones skipped, values as they are. Before --env, which wins. Repeatable
* `-l`, `--label <KEY[=VALUE]>` — Set a label on the container: KEY=VALUE, or KEY (an empty value). Repeatable
* `--label-file <FILE>` — Read labels from a file: a KEY=VALUE (or KEY) per line, # lines and blank ones skipped. Before --label, which wins. Repeatable
* `--cidfile <FILE>` — Write the container's ID to FILE once it's made (refused if FILE is there already)
* `-q`, `--quiet` — Print no pull progress
* `--privileged` — Not supported: give the container the capabilities (--cap-add) and devices (--device) it needs instead
* `--init` — Not supported yet: run the command without it (an image's own init, if it has one)
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
* `-h`, `--hostname <NAME>` — Hostname inside the container (Linux)
* `--domainname <NAME>` — NIS domain name inside the container (Linux)
* `--sysctl <KEY=VALUE>` — Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824 (Linux): IPC settings, or net.* with a network of its own (any --network but host and container:)
* `--network <MODE>` — The container's network: bridge (the default network: root's default), pasta (a network of its own through pasta: the default without root), host (the host's), none (one of its own with only a loopback interface), container:NAME (another running container's, as it is), or a network's name (Linux; bridge and names need root). Repeatable for bridge networks: the container joins each, in order. name=NAME,ip=IP,ip6=IP,mac-address=MAC,alias=ALIAS gives one its own settings
* `--ipc <MODE>` — The container's IPC namespace (Linux): private or shareable (its own: any running container's can be joined), or container:NAME (another running container's, with its /dev/shm)
* `--pid <MODE>` — The container's PID namespace (Linux): its own unless given; host (the host's: it sees and can signal every process there; root only), or container:NAME (another running container's: each sees the other's processes)
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
* `--expose <PORT[/PROTO]>` — Expose a port, or a range (8000-8010), TCP unless /udp: published with -P beside the image's own, and shown by inspect. Repeatable
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

* `--platform <PLATFORM>` — The image's platform (`linux/amd64`: on a Mac, run through Rosetta); by default the image's own among what runs here, this host's first
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
* `-v`, `--volume <[SOURCE:]DEST[:OPTIONS]>` — Mount a named volume or host path into the container: `[SOURCE:]DEST[:MODES]`, MODES `ro` and `nocopy`, comma-joined. A bare SOURCE (no slash) is a named volume, created on first use; DEST alone, a new anonymous one; either, if empty, is filled with the image's files at DEST when the container is made (unless `nocopy`). A path is bind-mounted (it must exist); DEST alone is a new anonymous volume. Repeatable (Linux, with a root filesystem)
* `--tmpfs <PATH[:OPTIONS]>` — Mount a tmpfs (in memory, empty at each start) at PATH, with `mount(8)`'s OPTIONS: `size=64m`, `mode=1777`, `uid`, `gid`, `nr_inodes`, `ro`, and `exec`, `suid`, `dev` (it's `noexec`, `nosuid`, `nodev` without them, as Docker's). Repeatable
* `--mount <KEY=VALUE,...>` — Mount as Docker's `--mount` does: `type=bind,source=/path,target=/d`, `type=volume,source=NAME,target=/d` (the default type), or `type=tmpfs,target=/d,tmpfs-size=64m,tmpfs-mode=1777`; `readonly` for any, `volume-nocopy` for a volume (not filled from the image). Repeatable



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

* `-t`, `--timeout <SECONDS>` [alias: `time`] — Seconds to wait after the stop signal before SIGKILL (the container's own, its --stop-timeout, unless given); -1 waits for as long as it takes, as Docker's
* `-s`, `--signal <SIGNAL>` — The signal sent first (the container's own, its --stop-signal, unless given)



## `shipyard restart`

Stop containers (as `stop`), then start them again

**Usage:** `shipyard restart [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-t`, `--timeout <SECONDS>` [alias: `time`] — Seconds to wait after the stop signal before SIGKILL (the container's own, its --stop-timeout, unless given); -1 waits for as long as it takes, as Docker's
* `-s`, `--signal <SIGNAL>` — The signal sent first (the container's own, its --stop-signal, unless given)



## `shipyard update`

Change containers' settings without making them again: their restart policy, and their resource limits (in their cgroup at once while they run, and for their next starts)

**Usage:** `shipyard update [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `--restart <POLICY>` — The restart policy: no, on-failure (or on-failure:N), always or unless-stopped
* `-m`, `--memory <SIZE>` — The most memory the container may use (such as 512m or 2g). Its swap, if it's the default (as much as its memory), follows
* `--memory-reservation <SIZE>` — Memory kept for the container when the host runs short
* `--memory-swap <SIZE>` — Memory plus swap: counted from --memory, or the container's own memory limit without it; -1 for unlimited swap
* `--cpus <NUMBER>` — How many CPUs' worth of time the container may use (such as 1.5)
* `--cpu-period <MICROSECONDS>` — The CPU period, in microseconds (1000 to 1000000); its quota stays
* `--cpu-quota <MICROSECONDS>` — CPU time per period, in microseconds; its period stays
* `-c`, `--cpu-shares <SHARES>` — The container's CPU share against others, in Docker's units (1024 is the default share)
* `--cpuset-cpus <LIST>` — The CPUs the container may run on (such as 0-3 or 0,2)
* `--cpuset-mems <LIST>` — The memory nodes the container may use (such as 0 or 0-1)
* `--pids-limit <NUMBER>` — The most processes (and threads) in the container, its init included (a number: a limit isn't lifted yet)
* `--blkio-weight <WEIGHT>` — The container's disk-time share, in Docker's units (10 to 1000)



## `shipyard kill`

Send a signal to running containers' main process (SIGKILL unless told), without waiting

**Usage:** `shipyard kill [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-s`, `--signal <SIGNAL>` — The signal: a name (KILL, SIGTERM) or a number

  Default value: `SIGKILL`



## `shipyard pause`

Freeze running containers' processes where they are (their cgroup's freezer), until `unpause`

**Usage:** `shipyard pause <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes



## `shipyard unpause`

Let paused containers' processes run on

**Usage:** `shipyard unpause <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes



## `shipyard top`

Show a running container's processes, in `ps -ef`'s columns

**Usage:** `shipyard top <CONTAINER> [PS OPTIONS]...`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix
* `<PS OPTIONS>` — Not supported: `ps`'s options (Shipyard reads /proc itself, in `ps -ef`'s columns; any but `-ef` refused)



## `shipyard stats`

Show what containers use (CPU, memory, network, block I/O, processes), every second until Ctrl-C, or once with --no-stream

**Usage:** `shipyard stats [OPTIONS] [CONTAINER]...`

###### **Arguments:**

* `<CONTAINER>` — The containers (without: those running, listed again each time)

###### **Options:**

* `-a`, `--all` — All containers, not only the ones up (without CONTAINER)
* `--format <FORMAT>` — How to print them: `table` (the default), `json` (an object per line), `table TEMPLATE`, or a Go TEMPLATE (`{{.Name}} {{.CPUPerc}}`)
* `--no-stream` — Print once and end (without: every second, until Ctrl-C)
* `--no-trunc` — Don't cut IDs short



## `shipyard compose`

Run Docker's `docker compose` against Shipyard (its API's service started if nothing serves it yet): `shipyard compose up -d`. Needs Docker's CLI and its compose plugin

**Usage:** `shipyard compose [COMPOSE ARGS]...`

###### **Arguments:**

* `<COMPOSE ARGS>` — `docker compose`'s arguments: its command and flags (`up -d`, `ps`, `down -v`…), as Docker's compose takes them



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
* `-e`, `--env <KEY[=VALUE]>` — Set an environment variable for it (KEY=VALUE, or a bare KEY to copy your shell's value; repeatable)
* `--env-file <FILE>` — Read variables from a file, as `run --env-file` does (before --env, which wins). Repeatable
* `--detach-keys <KEYS>` — Not supported yet: an exec's command ends with this session, so there's nothing to detach from (refused)
* `--privileged` — Not supported: give the container the capabilities it needs (refused)
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
* `-q`, `--quiet` — Taken, as Docker's (no progress): Shipyard prints none



## `shipyard export`

Write a container's own filesystem (never its volumes) as a tar archive, to standard output or -o FILE; running or stopped

**Usage:** `shipyard export [OPTIONS] <CONTAINER>`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix

###### **Options:**

* `-o`, `--output <FILE>` — Write the archive to FILE instead of standard output



## `shipyard logs`

Print a container's logs: every start's, oldest first

**Usage:** `shipyard logs [OPTIONS] <CONTAINER>`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix

###### **Options:**

* `-f`, `--follow` — Then print more as it comes, until the container stops (or is started again)
* `-n`, `--tail <N>` — Only the last N lines (across its starts); `all` is every one, as Docker's
* `--since <TIME>` — Only lines from TIME on: RFC 3339 (2026-10-10T06:16:19Z), Unix seconds, or a duration ago (10m, 2h, 30s, 3d)
* `--until <TIME>` — Only lines up to TIME (as --since takes it); a follow ends there
* `-t`, `--timestamps` — Start each line with the time it was written (RFC 3339)



## `shipyard events`

Print what happens (containers made, started, ended; images pulled; volumes and networks made and removed) as it happens, or what did with --since

**Usage:** `shipyard events [OPTIONS]`

###### **Options:**

* `--since <TIME>` — Past events too, from TIME on: RFC 3339 (2026-10-10T06:16:19Z), Unix seconds, or a duration ago (10m, 2h, 3d). Without it, only new ones
* `--until <TIME>` — Stop at TIME (as --since): with a past TIME, print and exit. Without it, wait for more until interrupted
* `-f`, `--filter <KEY=VALUE>` — Only the events that match: type=container|image|volume|network, event=ACTION, container=NAME|ID, image=REF, volume=NAME, network=NAME. Repeat a key for any of its values
* `--format <FORMAT>` — How to print them: Docker's line without it; `json` (Docker's event message, an object per line), or a Go TEMPLATE over it (`{{.Type}} {{.Action}} {{.Actor.Attributes.name}}`)



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
* `-q`, `--quiet` — Only their IDs (the format is then ignored, as Docker's)
* `--format <FORMAT>` — How to print them: `table` (the default), `json` (an object per line), `table TEMPLATE` (aligned, with a header), or a Go TEMPLATE (a line per object; `{{.Names}}`, `{{json .}}`, Docker's fields and functions)
* `-f`, `--filter <KEY=VALUE>` — Only those that match: KEY=VALUE, as Docker's (repeat a key for any of its values; `label` for all of them)
* `--no-trunc` — Don't cut IDs and commands short
* `-n`, `--last <N>` — Only the N made last (of all of them, up or not)
* `-l`, `--latest` — Only the one made last (of all of them, up or not)
* `-s`, `--size` — Taken, as Docker's: Shipyard doesn't measure a container's size (`Size` is `0B`)



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

Show objects in full, as JSON: a container, an image, a network or a volume, by name, in Docker's shape (`docker inspect`'s)

**Usage:** `shipyard inspect [OPTIONS] <NAME>...`

###### **Arguments:**

* `<NAME>` — The objects: names, IDs or ID prefixes

###### **Options:**

* `--shipyard` — Shipyard's own view of containers instead (its serial, its health history, its network's loss, the config as its record keeps it)
* `--type <TYPE>` — Only this kind of object (without it: the first of a container, an image, a network, a volume that has the name, as Docker's)

  Possible values:
  - `container`:
    A container
  - `image`:
    An image
  - `network`:
    A network
  - `volume`:
    A volume

* `-f`, `--format <FORMAT>` — How to show it: as Docker's command does without it (`inspect`: a JSON array; `version`, `info`: Docker's text); `json`, as JSON; or a Go TEMPLATE over it, a line each (`inspect`'s `{{.Name}}`, `{{json .Config}}`)
* `-s`, `--size` — Taken, as Docker's: Shipyard doesn't measure a container's size



## `shipyard rm`

Remove containers (their writable layers too). A running one needs -f

**Usage:** `shipyard rm [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-f`, `--force` — Remove a running container too (it's killed first)
* `-v`, `--volumes` — Remove its anonymous volumes too (`-v /path`, the image's own), each that no other container names; a named volume is kept



## `shipyard pull`

Pull an image into the local store (and extract it, on Linux), so a run starts at once and needs no registry

**Usage:** `shipyard pull [OPTIONS] <IMAGE>`

###### **Arguments:**

* `<IMAGE>` — The image, like `alpine:3.20` or `ghcr.io/owner/app@sha256:...`

###### **Options:**

* `-q`, `--quiet` — Print nothing but the image's digest
* `--platform <PLATFORM>` — The platform to pull (`linux/amd64`: on a Mac, run through Rosetta); by default the image's own among what runs here, this host's first



## `shipyard images`

List images in the local store

**Usage:** `shipyard images [OPTIONS] [REPOSITORY[:TAG]]`

###### **Arguments:**

* `<REPOSITORY[:TAG]>` — Only images of this name (`alpine`, `alpine:3.*`, as the `reference` filter)

###### **Options:**

* `-a`, `--all` — Taken, as Docker's: Shipyard keeps no intermediate images
* `-q`, `--quiet` — Only their IDs (the format is then ignored, as Docker's)
* `--format <FORMAT>` — How to print them: `table` (the default), `json` (an object per line), `table TEMPLATE` (aligned, with a header), or a Go TEMPLATE (a line per object; `{{.Names}}`, `{{json .}}`, Docker's fields and functions)
* `-f`, `--filter <KEY=VALUE>` — Only those that match: KEY=VALUE, as Docker's (repeat a key for any of its values; `label` for all of them)
* `--no-trunc` — Don't cut IDs short
* `--digests` — Show each image's digest too



## `shipyard rmi`

Remove one or more images from the local store (reference-counted; shared layers survive)

**Usage:** `shipyard rmi [OPTIONS] <IMAGE>...`

###### **Arguments:**

* `<IMAGE>` — The images to remove: references or IDs

###### **Options:**

* `-f`, `--force` — Untag an image even while a container uses it (a container keeps what it needs until it's removed; a foreground run's layers stay until it ends and a prune collects them)



## `shipyard tag`

Give an image another name (TARGET, a reference) beside its own

**Usage:** `shipyard tag <SOURCE_IMAGE> <TARGET_IMAGE>`

###### **Arguments:**

* `<SOURCE_IMAGE>` — The image: a reference or an ID
* `<TARGET_IMAGE>` — Its new name, like `registry.example/app:1.0`



## `shipyard save`

Write images to a tar archive (to standard output, or -o FILE), as Docker 25 writes one: an OCI image layout with Docker's `manifest.json`

**Usage:** `shipyard save [OPTIONS] <IMAGE>...`

###### **Arguments:**

* `<IMAGE>` — The images: references (a repository alone is every tag of it) or IDs (an image saved by ID has no name in the archive)

###### **Options:**

* `-o`, `--output <FILE>` — Write the archive to FILE instead of standard output



## `shipyard load`

Load images from a tar archive (from standard input, or -i FILE): Docker's (`docker save`'s) or an OCI layout (BuildKit's), plain or gzipped

**Usage:** `shipyard load [OPTIONS]`

###### **Options:**

* `-i`, `--input <FILE>` — Read the archive from FILE instead of standard input
* `-q`, `--quiet` — Don't say which tags moved to the loaded images (the names loaded, and what was left out, are still said)



## `shipyard build`

Build an image from a Dockerfile, with BuildKit (in a container of its own, gone at the build's end): named with -t

**Usage:** `shipyard build [OPTIONS] <CONTEXT>`

###### **Arguments:**

* `<CONTEXT>` — The context: a directory

###### **Options:**

* `-t`, `--tag <NAME[:TAG]>` — Name the image `NAME[:TAG]` (repeat it for more names): required unless -o writes its files (Shipyard keeps no untagged images)
* `-f`, `--file <FILE>` — The Dockerfile (default: CONTEXT/Dockerfile; - reads it from standard input)
* `--build-arg <KEY[=VALUE]>` — A build arg: KEY=VALUE, or KEY for this shell's value (repeat it)
* `--target <STAGE>` — The stage to build
* `--no-cache` — Use no cache
* `--pull` — Fetch the base images again
* `-q`, `--quiet` — Print only the image's ID (no progress)
* `--label <KEY=VALUE>` — A label of the image: KEY=VALUE (repeat it)
* `--progress <TYPE>` — How progress is shown: auto, plain (BuildKit's lines, as auto) or quiet

  Default value: `auto`
* `--platform <PLATFORM>` — The platform to build for: one that runs here (on a Mac, `linux/amd64` too, through Rosetta)
* `--secret <id=ID,src=FILE>` — A secret for `RUN --mount=type=secret`: id=ID,src=FILE (repeat it)
* `-o`, `--output <OUTPUT>` — Write the build's files instead of an image: type=local,dest=DIR (or DIR), or type=tar,dest=FILE (- for standard output)



## `shipyard history`

Show how an image was built: a row per step, newest first

**Usage:** `shipyard history [OPTIONS] <IMAGE>`

###### **Arguments:**

* `<IMAGE>` — The image: a reference or an ID

###### **Options:**

* `-H`, `--human <BOOL>` — Sizes and times for a person (`7.8MB`, `3 weeks ago`); with `-H=false`, bytes and RFC 3339 times

  Default value: `true`

  Possible values: `true`, `false`

* `--no-trunc` — Don't cut the IDs and instructions short
* `-q`, `--quiet` — Only the IDs
* `--format <FORMAT>` — How to print it: `table` (the default), `json` (an object per line), `table TEMPLATE`, or a Go TEMPLATE (`{{.CreatedBy}}`)



## `shipyard push`

Send an image to its registry (the name `tag` gave it: `NAME[:TAG]`), signed in as `login` (or `docker login`) left you

**Usage:** `shipyard push [OPTIONS] <NAME[:TAG]>`

###### **Arguments:**

* `<NAME[:TAG]>` — The image: `NAME[:TAG]` (`:latest` if none), as `tag` named it for the registry (`ghcr.io/owner/app:1`)

###### **Options:**

* `-a`, `--all-tags` — Push every tag of the repository the store has
* `-q`, `--quiet` — Print only each name pushed (no progress)



## `shipyard login`

Sign in to a registry (Docker Hub if none), kept where Docker keeps it: its credential helper, else ~/.docker/config.json

**Usage:** `shipyard login [OPTIONS] [SERVER]`

###### **Arguments:**

* `<SERVER>` — The registry (Docker Hub if none): `ghcr.io`, `registry.example:5000`

###### **Options:**

* `-u`, `--username <USERNAME>` — The username (asked on the terminal if not given)
* `-p`, `--password <PASSWORD>` — The password: seen by anyone who can list processes, and kept in the shell's history (use --password-stdin)
* `--password-stdin` — Read the password (or token) from standard input



## `shipyard logout`

Sign out of a registry (Docker Hub if none)

**Usage:** `shipyard logout [SERVER]`

###### **Arguments:**

* `<SERVER>` — The registry



## `shipyard import`

Make an image of a root filesystem's tar archive (a file, or - for standard input; plain or gzipped): one layer, named

**Usage:** `shipyard import [OPTIONS] <FILE|-> [REPOSITORY[:TAG]]`

###### **Arguments:**

* `<FILE|->` — The archive: a file, or - for standard input (a URL isn't taken)
* `<REPOSITORY[:TAG]>` — The image's name: `REPOSITORY[:TAG]` (required: Shipyard keeps no untagged images)

###### **Options:**

* `-c`, `--change <INSTRUCTION>` — Set a config instruction of the image's: CMD, ENTRYPOINT, ENV, EXPOSE, LABEL, USER, VOLUME, WORKDIR or STOPSIGNAL, as a Dockerfile's (repeat it for more)
* `-m`, `--message <MESSAGE>` — Its history's comment
* `--platform <PLATFORM>` — The image's platform: one that runs here (on a Mac, `linux/amd64` too)



## `shipyard container`

Manage containers (the same commands as at the top: `container ls` is `ps`, `container rm` is `rm`…)

**Usage:** `shipyard container <COMMAND>`

###### **Subcommands:**

* `ls` — List containers (`ps`)
* `run` — Run an image in a container (`run`)
* `create` — Make a container, to start later (`create`)
* `start` — Start containers (`start`)
* `stop` — Stop running containers (`stop`)
* `restart` — Stop containers, then start them again (`restart`)
* `kill` — Send a signal to running containers (`kill`)
* `pause` — Freeze running containers (`pause`)
* `unpause` — Let paused containers run on (`unpause`)
* `top` — Show a container's processes (`top`)
* `stats` — Show what containers use (`stats`)
* `rm` — Remove containers (`rm`)
* `logs` — Print a container's logs (`logs`)
* `exec` — Run a command in a running container (`exec`)
* `inspect` — Show containers in full, as JSON: Docker's shape (`inspect --type container`)
* `cp` — Copy files between a container and the host (`cp`)
* `export` — Write a container's own filesystem as a tar archive (`export`)
* `port` — Show the host ports a container publishes (`port`)
* `rename` — Rename a container (`rename`)
* `wait` — Wait for containers to end (`wait`)
* `attach` — Attach to a running container (`attach`)
* `update` — Change containers' settings (`update`)
* `prune` — Remove every stopped container



## `shipyard container ls`

List containers (`ps`)

**Usage:** `shipyard container ls [OPTIONS]`

**Command Aliases:** `ps`, `list`

###### **Options:**

* `-a`, `--all` — All containers, not only the ones up
* `-q`, `--quiet` — Only their IDs (the format is then ignored, as Docker's)
* `--format <FORMAT>` — How to print them: `table` (the default), `json` (an object per line), `table TEMPLATE` (aligned, with a header), or a Go TEMPLATE (a line per object; `{{.Names}}`, `{{json .}}`, Docker's fields and functions)
* `-f`, `--filter <KEY=VALUE>` — Only those that match: KEY=VALUE, as Docker's (repeat a key for any of its values; `label` for all of them)
* `--no-trunc` — Don't cut IDs and commands short
* `-n`, `--last <N>` — Only the N made last (of all of them, up or not)
* `-l`, `--latest` — Only the one made last (of all of them, up or not)
* `-s`, `--size` — Taken, as Docker's: Shipyard doesn't measure a container's size (`Size` is `0B`)



## `shipyard container run`

Run an image in a container (`run`)

**Usage:** `shipyard container run [OPTIONS] [IMAGE] [COMMAND]...`

###### **Arguments:**

* `<IMAGE>` — The image to run (a reference like `alpine:3.20` or `ghcr.io/owner/app@sha256:...`), then the command to run instead of its own, with its arguments. With --no-image, --rootfs or --no-isolation there's no image: the command comes first (--bundle takes none). Everything after the image is the command's, options too

###### **Options:**

* `-d`, `--detach` — Run in the background: the container is made and started, its ID printed, and it runs on once this command has ended (Linux)
* `--rm` — Remove the container once it ends (a foreground run returns once it's gone)
* `--sig-proxy <BOOL>` — In the foreground: pass on the signals this command gets (Ctrl-C, a kill) to the container. With false, Ctrl-C ends this command and the container runs on

  Default value: `true`

  Possible values: `true`, `false`

* `--detach-keys <KEYS>` — In the foreground with -it: the keys that detach (the container goes on): ctrl-p,ctrl-q, or others separated by commas (empty: none)
* `-a`, `--attach <STREAM>` — In the foreground: attach only these streams (stdin, stdout, stderr; repeatable). stdin takes your input, as -i. Without it, all of them
* `--help` — Print help (`-h` is the hostname's, as Docker's)

  Possible values: `true`, `false`

* `-i`, `--interactive` — Pass your input on to the command. Without it, an isolated command's stdin is empty (/dev/null), so it can't take keys meant for your shell
* `-t`, `--tty` — Give the command a terminal of the container's own (a pseudo-terminal): full-screen programs, password prompts and job control work. With -i, what you type goes to it
* `-e`, `--env <KEY[=VALUE]>` — Set an environment variable, or copy KEY from your shell if no value is given
* `--env-file <FILE>` — Read variables from a file, as Docker's: a KEY=VALUE per line (a bare KEY copies yours), # lines and blank ones skipped, values as they are. Before --env, which wins. Repeatable
* `-l`, `--label <KEY[=VALUE]>` — Set a label on the container: KEY=VALUE, or KEY (an empty value). Repeatable
* `--label-file <FILE>` — Read labels from a file: a KEY=VALUE (or KEY) per line, # lines and blank ones skipped. Before --label, which wins. Repeatable
* `--cidfile <FILE>` — Write the container's ID to FILE once it's made (refused if FILE is there already)
* `-q`, `--quiet` — Print no pull progress
* `--privileged` — Not supported: give the container the capabilities (--cap-add) and devices (--device) it needs instead
* `--init` — Not supported yet: run the command without it (an image's own init, if it has one)
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
* `-h`, `--hostname <NAME>` — Hostname inside the container (Linux)
* `--domainname <NAME>` — NIS domain name inside the container (Linux)
* `--sysctl <KEY=VALUE>` — Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824 (Linux): IPC settings, or net.* with a network of its own (any --network but host and container:)
* `--network <MODE>` — The container's network: bridge (the default network: root's default), pasta (a network of its own through pasta: the default without root), host (the host's), none (one of its own with only a loopback interface), container:NAME (another running container's, as it is), or a network's name (Linux; bridge and names need root). Repeatable for bridge networks: the container joins each, in order. name=NAME,ip=IP,ip6=IP,mac-address=MAC,alias=ALIAS gives one its own settings
* `--ipc <MODE>` — The container's IPC namespace (Linux): private or shareable (its own: any running container's can be joined), or container:NAME (another running container's, with its /dev/shm)
* `--pid <MODE>` — The container's PID namespace (Linux): its own unless given; host (the host's: it sees and can signal every process there; root only), or container:NAME (another running container's: each sees the other's processes)
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
* `--expose <PORT[/PROTO]>` — Expose a port, or a range (8000-8010), TCP unless /udp: published with -P beside the image's own, and shown by inspect. Repeatable
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

* `--platform <PLATFORM>` — The image's platform (`linux/amd64`: on a Mac, run through Rosetta); by default the image's own among what runs here, this host's first
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
* `-v`, `--volume <[SOURCE:]DEST[:OPTIONS]>` — Mount a named volume or host path into the container: `[SOURCE:]DEST[:MODES]`, MODES `ro` and `nocopy`, comma-joined. A bare SOURCE (no slash) is a named volume, created on first use; DEST alone, a new anonymous one; either, if empty, is filled with the image's files at DEST when the container is made (unless `nocopy`). A path is bind-mounted (it must exist); DEST alone is a new anonymous volume. Repeatable (Linux, with a root filesystem)
* `--tmpfs <PATH[:OPTIONS]>` — Mount a tmpfs (in memory, empty at each start) at PATH, with `mount(8)`'s OPTIONS: `size=64m`, `mode=1777`, `uid`, `gid`, `nr_inodes`, `ro`, and `exec`, `suid`, `dev` (it's `noexec`, `nosuid`, `nodev` without them, as Docker's). Repeatable
* `--mount <KEY=VALUE,...>` — Mount as Docker's `--mount` does: `type=bind,source=/path,target=/d`, `type=volume,source=NAME,target=/d` (the default type), or `type=tmpfs,target=/d,tmpfs-size=64m,tmpfs-mode=1777`; `readonly` for any, `volume-nocopy` for a volume (not filled from the image). Repeatable



## `shipyard container create`

Make a container, to start later (`create`)

**Usage:** `shipyard container create [OPTIONS] [IMAGE] [COMMAND]...`

###### **Arguments:**

* `<IMAGE>` — The image to run (a reference like `alpine:3.20` or `ghcr.io/owner/app@sha256:...`), then the command to run instead of its own, with its arguments. With --no-image, --rootfs or --no-isolation there's no image: the command comes first (--bundle takes none). Everything after the image is the command's, options too

###### **Options:**

* `--rm` — Remove the container once it ends, after its first start
* `--help` — Print help (`-h` is the hostname's, as Docker's)

  Possible values: `true`, `false`

* `-i`, `--interactive` — Pass your input on to the command. Without it, an isolated command's stdin is empty (/dev/null), so it can't take keys meant for your shell
* `-t`, `--tty` — Give the command a terminal of the container's own (a pseudo-terminal): full-screen programs, password prompts and job control work. With -i, what you type goes to it
* `-e`, `--env <KEY[=VALUE]>` — Set an environment variable, or copy KEY from your shell if no value is given
* `--env-file <FILE>` — Read variables from a file, as Docker's: a KEY=VALUE per line (a bare KEY copies yours), # lines and blank ones skipped, values as they are. Before --env, which wins. Repeatable
* `-l`, `--label <KEY[=VALUE]>` — Set a label on the container: KEY=VALUE, or KEY (an empty value). Repeatable
* `--label-file <FILE>` — Read labels from a file: a KEY=VALUE (or KEY) per line, # lines and blank ones skipped. Before --label, which wins. Repeatable
* `--cidfile <FILE>` — Write the container's ID to FILE once it's made (refused if FILE is there already)
* `-q`, `--quiet` — Print no pull progress
* `--privileged` — Not supported: give the container the capabilities (--cap-add) and devices (--device) it needs instead
* `--init` — Not supported yet: run the command without it (an image's own init, if it has one)
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
* `-h`, `--hostname <NAME>` — Hostname inside the container (Linux)
* `--domainname <NAME>` — NIS domain name inside the container (Linux)
* `--sysctl <KEY=VALUE>` — Set a kernel setting the container owns, e.g. kernel.shmmax=1073741824 (Linux): IPC settings, or net.* with a network of its own (any --network but host and container:)
* `--network <MODE>` — The container's network: bridge (the default network: root's default), pasta (a network of its own through pasta: the default without root), host (the host's), none (one of its own with only a loopback interface), container:NAME (another running container's, as it is), or a network's name (Linux; bridge and names need root). Repeatable for bridge networks: the container joins each, in order. name=NAME,ip=IP,ip6=IP,mac-address=MAC,alias=ALIAS gives one its own settings
* `--ipc <MODE>` — The container's IPC namespace (Linux): private or shareable (its own: any running container's can be joined), or container:NAME (another running container's, with its /dev/shm)
* `--pid <MODE>` — The container's PID namespace (Linux): its own unless given; host (the host's: it sees and can signal every process there; root only), or container:NAME (another running container's: each sees the other's processes)
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
* `--expose <PORT[/PROTO]>` — Expose a port, or a range (8000-8010), TCP unless /udp: published with -P beside the image's own, and shown by inspect. Repeatable
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

* `--platform <PLATFORM>` — The image's platform (`linux/amd64`: on a Mac, run through Rosetta); by default the image's own among what runs here, this host's first
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
* `-v`, `--volume <[SOURCE:]DEST[:OPTIONS]>` — Mount a named volume or host path into the container: `[SOURCE:]DEST[:MODES]`, MODES `ro` and `nocopy`, comma-joined. A bare SOURCE (no slash) is a named volume, created on first use; DEST alone, a new anonymous one; either, if empty, is filled with the image's files at DEST when the container is made (unless `nocopy`). A path is bind-mounted (it must exist); DEST alone is a new anonymous volume. Repeatable (Linux, with a root filesystem)
* `--tmpfs <PATH[:OPTIONS]>` — Mount a tmpfs (in memory, empty at each start) at PATH, with `mount(8)`'s OPTIONS: `size=64m`, `mode=1777`, `uid`, `gid`, `nr_inodes`, `ro`, and `exec`, `suid`, `dev` (it's `noexec`, `nosuid`, `nodev` without them, as Docker's). Repeatable
* `--mount <KEY=VALUE,...>` — Mount as Docker's `--mount` does: `type=bind,source=/path,target=/d`, `type=volume,source=NAME,target=/d` (the default type), or `type=tmpfs,target=/d,tmpfs-size=64m,tmpfs-mode=1777`; `readonly` for any, `volume-nocopy` for a volume (not filled from the image). Repeatable



## `shipyard container start`

Start containers (`start`)

**Usage:** `shipyard container start [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-a`, `--attach` — Attach to it as it starts (one container): its output from its first byte, until it ends (its exit code) or Ctrl-P Ctrl-Q
* `-i`, `--interactive` — With -a: pass your input on to it
* `--detach-keys <KEYS>` — With -a: the keys that detach (the container goes on), as attach's

  Default value: `ctrl-p,ctrl-q`



## `shipyard container stop`

Stop running containers (`stop`)

**Usage:** `shipyard container stop [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-t`, `--timeout <SECONDS>` [alias: `time`] — Seconds to wait after the stop signal before SIGKILL (the container's own, its --stop-timeout, unless given); -1 waits for as long as it takes, as Docker's
* `-s`, `--signal <SIGNAL>` — The signal sent first (the container's own, its --stop-signal, unless given)



## `shipyard container restart`

Stop containers, then start them again (`restart`)

**Usage:** `shipyard container restart [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-t`, `--timeout <SECONDS>` [alias: `time`] — Seconds to wait after the stop signal before SIGKILL (the container's own, its --stop-timeout, unless given); -1 waits for as long as it takes, as Docker's
* `-s`, `--signal <SIGNAL>` — The signal sent first (the container's own, its --stop-signal, unless given)



## `shipyard container kill`

Send a signal to running containers (`kill`)

**Usage:** `shipyard container kill [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-s`, `--signal <SIGNAL>` — The signal: a name (KILL, SIGTERM) or a number

  Default value: `SIGKILL`



## `shipyard container pause`

Freeze running containers (`pause`)

**Usage:** `shipyard container pause <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes



## `shipyard container unpause`

Let paused containers run on (`unpause`)

**Usage:** `shipyard container unpause <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes



## `shipyard container top`

Show a container's processes (`top`)

**Usage:** `shipyard container top <CONTAINER> [PS OPTIONS]...`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix
* `<PS OPTIONS>` — Not supported: `ps`'s options (Shipyard reads /proc itself, in `ps -ef`'s columns; any but `-ef` refused)



## `shipyard container stats`

Show what containers use (`stats`)

**Usage:** `shipyard container stats [OPTIONS] [CONTAINER]...`

###### **Arguments:**

* `<CONTAINER>` — The containers (without: those running, listed again each time)

###### **Options:**

* `-a`, `--all` — All containers, not only the ones up (without CONTAINER)
* `--format <FORMAT>` — How to print them: `table` (the default), `json` (an object per line), `table TEMPLATE`, or a Go TEMPLATE (`{{.Name}} {{.CPUPerc}}`)
* `--no-stream` — Print once and end (without: every second, until Ctrl-C)
* `--no-trunc` — Don't cut IDs short



## `shipyard container rm`

Remove containers (`rm`)

**Usage:** `shipyard container rm [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `-f`, `--force` — Remove a running container too (it's killed first)
* `-v`, `--volumes` — Remove its anonymous volumes too (`-v /path`, the image's own), each that no other container names; a named volume is kept



## `shipyard container logs`

Print a container's logs (`logs`)

**Usage:** `shipyard container logs [OPTIONS] <CONTAINER>`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix

###### **Options:**

* `-f`, `--follow` — Then print more as it comes, until the container stops (or is started again)
* `-n`, `--tail <N>` — Only the last N lines (across its starts); `all` is every one, as Docker's
* `--since <TIME>` — Only lines from TIME on: RFC 3339 (2026-10-10T06:16:19Z), Unix seconds, or a duration ago (10m, 2h, 30s, 3d)
* `--until <TIME>` — Only lines up to TIME (as --since takes it); a follow ends there
* `-t`, `--timestamps` — Start each line with the time it was written (RFC 3339)



## `shipyard container exec`

Run a command in a running container (`exec`)

**Usage:** `shipyard container exec [OPTIONS] <CONTAINER> <COMMAND>...`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix
* `<COMMAND>` — The command and its arguments

###### **Options:**

* `-i`, `--interactive` — Pass your input on to it (without: it reads end-of-file)
* `-t`, `--tty` — Give it a terminal
* `-d`, `--detach` — Return once it runs (it goes on)
* `-u`, `--user <USER[:GROUP]>` — Run it as this user, and group if given: names (from the container's own /etc/passwd and /etc/group) or numbers
* `-e`, `--env <KEY[=VALUE]>` — Set an environment variable for it (KEY=VALUE, or a bare KEY to copy your shell's value; repeatable)
* `--env-file <FILE>` — Read variables from a file, as `run --env-file` does (before --env, which wins). Repeatable
* `--detach-keys <KEYS>` — Not supported yet: an exec's command ends with this session, so there's nothing to detach from (refused)
* `--privileged` — Not supported: give the container the capabilities it needs (refused)
* `-w`, `--workdir <DIR>` — Its working directory, in the container



## `shipyard container inspect`

Show containers in full, as JSON: Docker's shape (`inspect --type container`)

**Usage:** `shipyard container inspect [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `--shipyard` — Shipyard's own view instead (its serial, its health history, its network's loss, the config as its record keeps it)
* `-f`, `--format <FORMAT>` — How to show it: as Docker's command does without it (`inspect`: a JSON array; `version`, `info`: Docker's text); `json`, as JSON; or a Go TEMPLATE over it, a line each (`inspect`'s `{{.Name}}`, `{{json .Config}}`)
* `-s`, `--size` — Taken, as Docker's: Shipyard doesn't measure a container's size



## `shipyard container cp`

Copy files between a container and the host (`cp`)

**Usage:** `shipyard container cp [OPTIONS] <SOURCE> <DESTINATION>`

###### **Arguments:**

* `<SOURCE>` — From: CONTAINER:PATH, a host path, or - (a tar stream on stdin)
* `<DESTINATION>` — To: CONTAINER:PATH, a host path, or - (a tar stream on stdout)

###### **Options:**

* `-a`, `--archive` — Keep the files' owners (where the container has those IDs)
* `-L`, `--follow-link` — Follow a source that's a symbolic link
* `-q`, `--quiet` — Taken, as Docker's (no progress): Shipyard prints none



## `shipyard container export`

Write a container's own filesystem as a tar archive (`export`)

**Usage:** `shipyard container export [OPTIONS] <CONTAINER>`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix

###### **Options:**

* `-o`, `--output <FILE>` — Write the archive to FILE instead of standard output



## `shipyard container port`

Show the host ports a container publishes (`port`)

**Usage:** `shipyard container port [OPTIONS] <CONTAINER> [PORT[/PROTO]]`

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




## `shipyard container rename`

Rename a container (`rename`)

**Usage:** `shipyard container rename <CONTAINER> <NEW_NAME>`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix
* `<NEW_NAME>` — Its new name



## `shipyard container wait`

Wait for containers to end (`wait`)

**Usage:** `shipyard container wait <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes



## `shipyard container attach`

Attach to a running container (`attach`)

**Usage:** `shipyard container attach [OPTIONS] <CONTAINER>`

###### **Arguments:**

* `<CONTAINER>` — The container: its name, ID, serial or ID prefix

###### **Options:**

* `--detach-keys <KEYS>` — The keys that detach (the container goes on): ctrl-p,ctrl-q, or others separated by commas (empty: none)

  Default value: `ctrl-p,ctrl-q`
* `--no-stdin` — Don't pass your input on



## `shipyard container update`

Change containers' settings (`update`)

**Usage:** `shipyard container update [OPTIONS] <CONTAINER>...`

###### **Arguments:**

* `<CONTAINER>` — The containers: names, IDs, serials or ID prefixes

###### **Options:**

* `--restart <POLICY>` — The restart policy: no, on-failure (or on-failure:N), always or unless-stopped
* `-m`, `--memory <SIZE>` — The most memory the container may use (such as 512m or 2g). Its swap, if it's the default (as much as its memory), follows
* `--memory-reservation <SIZE>` — Memory kept for the container when the host runs short
* `--memory-swap <SIZE>` — Memory plus swap: counted from --memory, or the container's own memory limit without it; -1 for unlimited swap
* `--cpus <NUMBER>` — How many CPUs' worth of time the container may use (such as 1.5)
* `--cpu-period <MICROSECONDS>` — The CPU period, in microseconds (1000 to 1000000); its quota stays
* `--cpu-quota <MICROSECONDS>` — CPU time per period, in microseconds; its period stays
* `-c`, `--cpu-shares <SHARES>` — The container's CPU share against others, in Docker's units (1024 is the default share)
* `--cpuset-cpus <LIST>` — The CPUs the container may run on (such as 0-3 or 0,2)
* `--cpuset-mems <LIST>` — The memory nodes the container may use (such as 0 or 0-1)
* `--pids-limit <NUMBER>` — The most processes (and threads) in the container, its init included (a number: a limit isn't lifted yet)
* `--blkio-weight <WEIGHT>` — The container's disk-time share, in Docker's units (10 to 1000)



## `shipyard container prune`

Remove every stopped container

**Usage:** `shipyard container prune [OPTIONS]`

###### **Options:**

* `-f`, `--force` — Don't ask (needed where there's no terminal to ask on)
* `--filter <KEY=VALUE>` — Only what matches: `until=TIME` (a time, or a duration such as `24h`: made before), `label=KEY[=VALUE]`, `label!=KEY[=VALUE]`; `image prune` takes `dangling=false` too (as -a)



## `shipyard image`

Manage images

**Usage:** `shipyard image <COMMAND>`

###### **Subcommands:**

* `ls` — List images in the local store (`images`)
* `prune` — Remove dangling images (every image no container was made from, with -a), and what nothing in the store needs
* `rm` — Remove images (`rmi`)
* `pull` — Pull an image (`pull`)
* `tag` — Give an image another name (`tag`)
* `save` — Write images to a tar archive (`save`)
* `load` — Load images from a tar archive (`load`)
* `build` — Build an image from a Dockerfile (`build`)
* `history` — Show how an image was built (`history`)
* `import` — Make an image of a root filesystem's tar archive (`import`)
* `push` — Send an image to its registry (`push`)
* `inspect` — Show images in full, as JSON: Docker's shape (`docker image inspect`'s)



## `shipyard image ls`

List images in the local store (`images`)

**Usage:** `shipyard image ls [OPTIONS] [REPOSITORY[:TAG]]`

**Command Alias:** `list`

###### **Arguments:**

* `<REPOSITORY[:TAG]>` — Only images of this name (`alpine`, `alpine:3.*`, as the `reference` filter)

###### **Options:**

* `-a`, `--all` — Taken, as Docker's: Shipyard keeps no intermediate images
* `-q`, `--quiet` — Only their IDs (the format is then ignored, as Docker's)
* `--format <FORMAT>` — How to print them: `table` (the default), `json` (an object per line), `table TEMPLATE` (aligned, with a header), or a Go TEMPLATE (a line per object; `{{.Names}}`, `{{json .}}`, Docker's fields and functions)
* `-f`, `--filter <KEY=VALUE>` — Only those that match: KEY=VALUE, as Docker's (repeat a key for any of its values; `label` for all of them)
* `--no-trunc` — Don't cut IDs short
* `--digests` — Show each image's digest too



## `shipyard image prune`

Remove dangling images (every image no container was made from, with -a), and what nothing in the store needs

**Usage:** `shipyard image prune [OPTIONS]`

###### **Options:**

* `-a`, `--all` — Every image no container was made from, not only dangling ones
* `-f`, `--force` — Don't ask (needed where there's no terminal to ask on)
* `--filter <KEY=VALUE>` — Only what matches: `until=TIME` (a time, or a duration such as `24h`: made before), `label=KEY[=VALUE]`, `label!=KEY[=VALUE]`; `image prune` takes `dangling=false` too (as -a)



## `shipyard image rm`

Remove images (`rmi`)

**Usage:** `shipyard image rm [OPTIONS] <IMAGE>...`

###### **Arguments:**

* `<IMAGE>` — The images to remove: references or IDs

###### **Options:**

* `-f`, `--force` — Untag an image even while a container uses it (a container keeps what it needs until it's removed; a foreground run's layers stay until it ends and a prune collects them)



## `shipyard image pull`

Pull an image (`pull`)

**Usage:** `shipyard image pull [OPTIONS] <IMAGE>`

###### **Arguments:**

* `<IMAGE>` — The image, like `alpine:3.20` or `ghcr.io/owner/app@sha256:...`

###### **Options:**

* `-q`, `--quiet` — Print nothing but the image's digest
* `--platform <PLATFORM>` — The platform to pull (`linux/amd64`: on a Mac, run through Rosetta); by default the image's own among what runs here, this host's first



## `shipyard image tag`

Give an image another name (`tag`)

**Usage:** `shipyard image tag <SOURCE_IMAGE> <TARGET_IMAGE>`

###### **Arguments:**

* `<SOURCE_IMAGE>` — The image: a reference or an ID
* `<TARGET_IMAGE>` — Its new name, like `registry.example/app:1.0`



## `shipyard image save`

Write images to a tar archive (`save`)

**Usage:** `shipyard image save [OPTIONS] <IMAGE>...`

###### **Arguments:**

* `<IMAGE>` — The images: references (a repository alone is every tag of it) or IDs (an image saved by ID has no name in the archive)

###### **Options:**

* `-o`, `--output <FILE>` — Write the archive to FILE instead of standard output



## `shipyard image load`

Load images from a tar archive (`load`)

**Usage:** `shipyard image load [OPTIONS]`

###### **Options:**

* `-i`, `--input <FILE>` — Read the archive from FILE instead of standard input
* `-q`, `--quiet` — Don't say which tags moved to the loaded images (the names loaded, and what was left out, are still said)



## `shipyard image build`

Build an image from a Dockerfile (`build`)

**Usage:** `shipyard image build [OPTIONS] <CONTEXT>`

###### **Arguments:**

* `<CONTEXT>` — The context: a directory

###### **Options:**

* `-t`, `--tag <NAME[:TAG]>` — Name the image `NAME[:TAG]` (repeat it for more names): required unless -o writes its files (Shipyard keeps no untagged images)
* `-f`, `--file <FILE>` — The Dockerfile (default: CONTEXT/Dockerfile; - reads it from standard input)
* `--build-arg <KEY[=VALUE]>` — A build arg: KEY=VALUE, or KEY for this shell's value (repeat it)
* `--target <STAGE>` — The stage to build
* `--no-cache` — Use no cache
* `--pull` — Fetch the base images again
* `-q`, `--quiet` — Print only the image's ID (no progress)
* `--label <KEY=VALUE>` — A label of the image: KEY=VALUE (repeat it)
* `--progress <TYPE>` — How progress is shown: auto, plain (BuildKit's lines, as auto) or quiet

  Default value: `auto`
* `--platform <PLATFORM>` — The platform to build for: one that runs here (on a Mac, `linux/amd64` too, through Rosetta)
* `--secret <id=ID,src=FILE>` — A secret for `RUN --mount=type=secret`: id=ID,src=FILE (repeat it)
* `-o`, `--output <OUTPUT>` — Write the build's files instead of an image: type=local,dest=DIR (or DIR), or type=tar,dest=FILE (- for standard output)



## `shipyard image history`

Show how an image was built (`history`)

**Usage:** `shipyard image history [OPTIONS] <IMAGE>`

###### **Arguments:**

* `<IMAGE>` — The image: a reference or an ID

###### **Options:**

* `-H`, `--human <BOOL>` — Sizes and times for a person (`7.8MB`, `3 weeks ago`); with `-H=false`, bytes and RFC 3339 times

  Default value: `true`

  Possible values: `true`, `false`

* `--no-trunc` — Don't cut the IDs and instructions short
* `-q`, `--quiet` — Only the IDs
* `--format <FORMAT>` — How to print it: `table` (the default), `json` (an object per line), `table TEMPLATE`, or a Go TEMPLATE (`{{.CreatedBy}}`)



## `shipyard image import`

Make an image of a root filesystem's tar archive (`import`)

**Usage:** `shipyard image import [OPTIONS] <FILE|-> [REPOSITORY[:TAG]]`

###### **Arguments:**

* `<FILE|->` — The archive: a file, or - for standard input (a URL isn't taken)
* `<REPOSITORY[:TAG]>` — The image's name: `REPOSITORY[:TAG]` (required: Shipyard keeps no untagged images)

###### **Options:**

* `-c`, `--change <INSTRUCTION>` — Set a config instruction of the image's: CMD, ENTRYPOINT, ENV, EXPOSE, LABEL, USER, VOLUME, WORKDIR or STOPSIGNAL, as a Dockerfile's (repeat it for more)
* `-m`, `--message <MESSAGE>` — Its history's comment
* `--platform <PLATFORM>` — The image's platform: one that runs here (on a Mac, `linux/amd64` too)



## `shipyard image push`

Send an image to its registry (`push`)

**Usage:** `shipyard image push [OPTIONS] <NAME[:TAG]>`

###### **Arguments:**

* `<NAME[:TAG]>` — The image: `NAME[:TAG]` (`:latest` if none), as `tag` named it for the registry (`ghcr.io/owner/app:1`)

###### **Options:**

* `-a`, `--all-tags` — Push every tag of the repository the store has
* `-q`, `--quiet` — Print only each name pushed (no progress)



## `shipyard image inspect`

Show images in full, as JSON: Docker's shape (`docker image inspect`'s)

**Usage:** `shipyard image inspect [OPTIONS] <IMAGE>...`

###### **Arguments:**

* `<IMAGE>` — The images: references or IDs

###### **Options:**

* `-f`, `--format <FORMAT>` — How to show it: as Docker's command does without it (`inspect`: a JSON array; `version`, `info`: Docker's text); `json`, as JSON; or a Go TEMPLATE over it, a line each (`inspect`'s `{{.Name}}`, `{{json .Config}}`)



## `shipyard volume`

Manage named volumes

**Usage:** `shipyard volume <COMMAND>`

###### **Subcommands:**

* `create` — Create a named volume (one with a new name of its own without NAME; one that's there already is left as it is)
* `ls` — List named volumes
* `inspect` — Show a volume's details
* `rm` — Remove one or more named volumes (never one a container names, running or not)
* `prune` — Remove every named volume no container uses, running or not (asks first)



## `shipyard volume create`

Create a named volume (one with a new name of its own without NAME; one that's there already is left as it is)

**Usage:** `shipyard volume create [OPTIONS] [NAME]`

###### **Arguments:**

* `<NAME>` — The volume's name

###### **Options:**

* `-l`, `--label <KEY[=VALUE]>` — Set a label on it: KEY=VALUE, or KEY (an empty value). Repeatable
* `-d`, `--driver <DRIVER>` — Its driver: local, the only one (a directory of the store's)

  Default value: `local`
* `-o`, `--opt <KEY=VALUE>` — A driver option: refused (a local volume is a directory, with no options; `type=nfs` and the like aren't mounted)



## `shipyard volume ls`

List named volumes

**Usage:** `shipyard volume ls [OPTIONS]`

**Command Alias:** `list`

###### **Options:**

* `-q`, `--quiet` — Only their IDs (the format is then ignored, as Docker's)
* `--format <FORMAT>` — How to print them: `table` (the default), `json` (an object per line), `table TEMPLATE` (aligned, with a header), or a Go TEMPLATE (a line per object; `{{.Names}}`, `{{json .}}`, Docker's fields and functions)
* `-f`, `--filter <KEY=VALUE>` — Only those that match: KEY=VALUE, as Docker's (repeat a key for any of its values; `label` for all of them)



## `shipyard volume inspect`

Show a volume's details

**Usage:** `shipyard volume inspect [OPTIONS] <VOLUME>...`

###### **Arguments:**

* `<VOLUME>` — The volumes

###### **Options:**

* `-f`, `--format <FORMAT>` — How to show it: as Docker's command does without it (`inspect`: a JSON array; `version`, `info`: Docker's text); `json`, as JSON; or a Go TEMPLATE over it, a line each (`inspect`'s `{{.Name}}`, `{{json .Config}}`)



## `shipyard volume rm`

Remove one or more named volumes (never one a container names, running or not)

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
* `rm` — Remove one or more networks (never one a container that's up is on, nor the default one; a stopped container doesn't keep its networks, as Docker's don't)
* `prune` — Remove every network no container that's up is on, but the default one (asks first)
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
* `-l`, `--label <KEY[=VALUE]>` — Set a label on it: KEY=VALUE, or KEY (an empty value). Repeatable
* `-d`, `--driver <DRIVER>` — Its driver: bridge, the only one

  Default value: `bridge`
* `-o`, `--opt <KEY=VALUE>` — A driver option, KEY=VALUE: Docker's bridge options at their defaults, and Shipyard's own (`com.shipyard.network.isolate-host`, `com.shipyard.network.conntrack-limit`); others are refused. Repeatable



## `shipyard network ls`

List networks

**Usage:** `shipyard network ls [OPTIONS]`

**Command Alias:** `list`

###### **Options:**

* `-q`, `--quiet` — Only their IDs (the format is then ignored, as Docker's)
* `--format <FORMAT>` — How to print them: `table` (the default), `json` (an object per line), `table TEMPLATE` (aligned, with a header), or a Go TEMPLATE (a line per object; `{{.Names}}`, `{{json .}}`, Docker's fields and functions)
* `-f`, `--filter <KEY=VALUE>` — Only those that match: KEY=VALUE, as Docker's (repeat a key for any of its values; `label` for all of them)
* `--no-trunc` — Don't cut IDs short



## `shipyard network inspect`

Show networks' details, and the containers on them, as JSON

**Usage:** `shipyard network inspect [OPTIONS] <NETWORK>...`

###### **Arguments:**

* `<NETWORK>` — The networks

###### **Options:**

* `--shipyard` — Shipyard's own view instead (the containers on it by their leases, its bridge, its DNS server)
* `-f`, `--format <FORMAT>` — How to show it: as Docker's command does without it (`inspect`: a JSON array; `version`, `info`: Docker's text); `json`, as JSON; or a Go TEMPLATE over it, a line each (`inspect`'s `{{.Name}}`, `{{json .Config}}`)



## `shipyard network rm`

Remove one or more networks (never one a container that's up is on, nor the default one; a stopped container doesn't keep its networks, as Docker's don't)

**Usage:** `shipyard network rm <NETWORK>...`

###### **Arguments:**

* `<NETWORK>` — The networks to remove



## `shipyard network prune`

Remove every network no container that's up is on, but the default one (asks first)

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

* `prune` — Remove what nothing uses: stopped containers, unused networks, dangling images (every unused one with -a), unused anonymous volumes with --volumes
* `df` — Show the store's disk usage
* `recover` — Settle containers that ended unwatched, then start those whose restart policy brings them back when the host starts (what the boot unit runs: see contrib/systemd)
* `stop` — Stop every container that's up, for the host's shutdown, each with its own stop signal and timeout: not as your stop (their restart policy brings them back at boot), and none restarted until `system recover` (what shipyard-stop.service runs at a shutdown)
* `service` — Serve Docker's Engine API (a subset, version 1.43) on a UNIX socket, for Docker's own clients (`DOCKER_HOST=unix://SOCKET docker ps`); systemd's shipyard.socket starts it at the first connection (see contrib/systemd). It exits once nothing has been asked for 60 s (`SHIPYARD_API_IDLE`: other whole seconds)



## `shipyard system prune`

Remove what nothing uses: stopped containers, unused networks, dangling images (every unused one with -a), unused anonymous volumes with --volumes

**Usage:** `shipyard system prune [OPTIONS]`

###### **Options:**

* `-a`, `--all` — Every image no container was made from, not only dangling ones
* `--volumes` — The anonymous volumes no container uses too
* `-f`, `--force` — Don't ask (needed where there's no terminal to ask on)
* `--filter <KEY=VALUE>` — Only what matches: `until=TIME` (a time, or a duration such as `24h`: made before), `label=KEY[=VALUE]`, `label!=KEY[=VALUE]`; `image prune` takes `dangling=false` too (as -a)



## `shipyard system df`

Show the store's disk usage

**Usage:** `shipyard system df [OPTIONS]`

###### **Options:**

* `--format <FORMAT>` — How to show it: Docker's table without it; `json`, an object per kind; or a Go TEMPLATE (`{{.Type}} {{.Size}}`)



## `shipyard system recover`

Settle containers that ended unwatched, then start those whose restart policy brings them back when the host starts (what the boot unit runs: see contrib/systemd)

**Usage:** `shipyard system recover`



## `shipyard system stop`

Stop every container that's up, for the host's shutdown, each with its own stop signal and timeout: not as your stop (their restart policy brings them back at boot), and none restarted until `system recover` (what shipyard-stop.service runs at a shutdown)

**Usage:** `shipyard system stop`



## `shipyard system service`

Serve Docker's Engine API (a subset, version 1.43) on a UNIX socket, for Docker's own clients (`DOCKER_HOST=unix://SOCKET docker ps`); systemd's shipyard.socket starts it at the first connection (see contrib/systemd). It exits once nothing has been asked for 60 s (`SHIPYARD_API_IDLE`: other whole seconds)

**Usage:** `shipyard system service [OPTIONS]`

###### **Options:**

* `--socket <PATH>` — The socket to listen on, when systemd passes none (by default api.sock in the containers' state directory: /run/shipyard for root, shipyard in the user's runtime directory for anyone else)
* `--ensure` — Start the service in the background if nothing answers on its socket (as `shipyard compose` does), and exit once it answers: it runs on until it's idle. Refused for a socket in a directory that isn't yours alone



## `shipyard vm`

The Linux VM containers run in on a Mac: its status, start and stop

**Usage:** `shipyard vm <COMMAND>`

###### **Subcommands:**

* `start` — Start the VM, if it isn't running, and wait for its guest to boot
* `status` — Say whether the VM runs, which guest it boots, and the ports it forwards to this Mac
* `stop` — Stop the VM: its containers stopped in order, then the guest powered off
* `install` — Let launchd hold Docker's socket while the VM is stopped and start the VM at its first connection (`docker` works any time; nothing runs until then). Run again after moving shipyard
* `uninstall` — Undo `vm install`: launchd lets go of Docker's socket (a VM it started is stopped first)
* `set` — Set the VM's memory and CPUs (taken at its next start)



## `shipyard vm start`

Start the VM, if it isn't running, and wait for its guest to boot

**Usage:** `shipyard vm start`



## `shipyard vm status`

Say whether the VM runs, which guest it boots, and the ports it forwards to this Mac

**Usage:** `shipyard vm status`



## `shipyard vm stop`

Stop the VM: its containers stopped in order, then the guest powered off

**Usage:** `shipyard vm stop`



## `shipyard vm install`

Let launchd hold Docker's socket while the VM is stopped and start the VM at its first connection (`docker` works any time; nothing runs until then). Run again after moving shipyard

**Usage:** `shipyard vm install [OPTIONS]`

###### **Options:**

* `--at-login` — Also start the VM at login, when containers with a restart policy would come back (they're brought back in it, and the VM stops by itself once none runs). `vm install` without it turns that off



## `shipyard vm uninstall`

Undo `vm install`: launchd lets go of Docker's socket (a VM it started is stopped first)

**Usage:** `shipyard vm uninstall`



## `shipyard vm set`

Set the VM's memory and CPUs (taken at its next start)

**Usage:** `shipyard vm set <--memory <SIZE>|--cpus <N>>`

###### **Options:**

* `--memory <SIZE>` — The VM's memory (`8g`, `6144m`), or `default` (a quarter of this Mac's, from 4 GiB to 16 GiB)
* `--cpus <N>` — The VM's CPUs, or `default` (half this Mac's cores, at most 8)



## `shipyard builder`

The build cache

**Usage:** `shipyard builder <COMMAND>`

###### **Subcommands:**

* `prune` — Remove the build cache (every context's: one a build is using is kept, said)



## `shipyard builder prune`

Remove the build cache (every context's: one a build is using is kept, said)

**Usage:** `shipyard builder prune [OPTIONS]`

###### **Options:**

* `-a`, `--all` — Taken, as Docker's: every build cache goes (each is a context's whole)
* `-f`, `--force` — Don't ask (needed where there's no terminal to ask on)



## `shipyard doctor`

Check whether this host can run containers, and what to fix

**Usage:** `shipyard doctor`



## `shipyard version`

Show Shipyard's version and its host's (`--format` a Go template over `{Client, Server}`, as Docker's)

**Usage:** `shipyard version [OPTIONS]`

###### **Options:**

* `-f`, `--format <FORMAT>` — How to show it: as Docker's command does without it (`inspect`: a JSON array; `version`, `info`: Docker's text); `json`, as JSON; or a Go TEMPLATE over it, a line each (`inspect`'s `{{.Name}}`, `{{json .Config}}`)



## `shipyard info`

Show what Shipyard has and what its host is (`--format` a Go template over Docker's `SystemInfo`)

**Usage:** `shipyard info [OPTIONS]`

###### **Options:**

* `-f`, `--format <FORMAT>` — How to show it: as Docker's command does without it (`inspect`: a JSON array; `version`, `info`: Docker's text); `json`, as JSON; or a Go TEMPLATE over it, a line each (`inspect`'s `{{.Name}}`, `{{json .Config}}`)




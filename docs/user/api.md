# The Docker API

Shipyard serves a subset of Docker's Engine API (version 1.43, Docker 24's) on a
UNIX socket, so Docker's own clients can drive it: the `docker` CLI, and the tools
built on Docker's API. On Linux it's the service's own socket; on a Mac, `docker.sock`
relays to the same API in Shipyard's VM ([macOS](macos.md)).

Shipyard's own CLI doesn't use it: on Linux, `shipyard` calls the engine directly, with
nothing in between. The API is for Docker's clients.

Checked against: Docker's CLI 24, `docker compose` 5.6 (see [compose](compose.md)),
and [Testcontainers for Rust](https://github.com/testcontainers/testcontainers-rs) 0.28.
With `DOCKER_HOST` set to the socket, Testcontainers' everyday calls work as they do
with Docker's daemon: images pulled when missing; waits on a container's output, its
health check and its exit; files copied in and out; `exec` and its exit code; mounts;
published ports read back; pause, stop and removal; its networks (root).

## Starting it

The API is served by `shipyard system service`. Nothing runs while nothing asks: with
systemd's socket units, systemd listens on the socket and starts the service at the
first connection, and the service exits after 60 s with no connection open and no
request running.

Root's API, for root's containers:

```sh
sudo install -m 0644 contrib/systemd/shipyard.socket contrib/systemd/shipyard.service \
  /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now shipyard.socket
export DOCKER_HOST=unix:///run/shipyard/api.sock
sudo -E docker ps
```

Yours (rootless), for your containers:

```sh
mkdir -p ~/.config/systemd/user
install -m 0644 contrib/systemd/user/shipyard.socket contrib/systemd/user/shipyard.service \
  ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now shipyard.socket
export DOCKER_HOST=unix://$XDG_RUNTIME_DIR/shipyard/api.sock
docker ps
```

Without systemd, run it yourself: `shipyard system service` listens on the same
socket (`--socket PATH` for another) until it's idle. A stop (SIGTERM, as systemd
sends, or Ctrl-C) ends it once the requests it's running have finished: a create or a
start is never cut, and none starts once it's stopping (`503`). `SHIPYARD_API_IDLE=SECONDS`
sets the idle time (whole seconds, at least 1). A second service on a socket that's
served already is refused, and so is a socket path where something else is.

`shipyard doctor` says where the socket is, and whether systemd's socket unit is
enabled.

On a Mac, the VM serves the API on `docker.sock` in its directory while it runs, and
the guest starts its service on the first connection: [Docker's CLI on your
Mac](macos.md#dockers-cli-on-your-mac).

The socket is Shipyard's own, never Docker's `/var/run/docker.sock`, so Docker and
Shipyard can both be installed: `DOCKER_HOST` picks one. `docker context create
shipyard --docker host=unix:///run/shipyard/api.sock` keeps the choice in Docker's
own configuration.

## Who can use it

**Anyone who can use root's socket can run containers as root.** So it's root's alone
(`0600`, in a directory only root can enter), and the service checks each
connection's user as the kernel gives it: a connection from another user is closed
before anything is read. There's no `docker` group, as there is with Docker: that
group is root for its members. Use `sudo`, or your own rootless API.

A rootless socket is yours alone, in your runtime directory.

## What's served

| Request | Docker's command |
|---|---|
| `GET`, `HEAD /_ping` | every command, first: the API version |
| `GET /version` | `docker version` |
| `GET /info` | `docker info` (the part Shipyard can fill: counts, kernel, CPUs, memory) |
| `GET /containers/json` | `docker ps` (`all`, `limit`; filters `id`, `name`, `label`, `status`, `exited`, `health`, `ancestor`, `network`, `volume`, `before`, `since`) |
| `GET /containers/{id}/json` | `docker inspect` |
| `POST /containers/create` | `docker create`, the first half of `docker run -d` (`platform`, as `docker run --platform` sends it: one that runs here, `400` otherwise; an image stored for another platform is `404 No such image`, so the client pulls the one asked for) |
| `POST /containers/{id}/start` | `docker start` |
| `POST /containers/{id}/stop` | `docker stop` (`t`, `signal`) |
| `POST /containers/{id}/restart` | `docker restart` |
| `POST /containers/{id}/kill` | `docker kill` (`signal`) |
| `GET /containers/{id}/top` | `docker top`: Docker's `Titles` and `Processes`, `ps -ef`'s columns read from `/proc`; `ps_args` other than `-ef` (the daemon's default) `400`; one not running `409` |
| `GET /containers/{id}/stats` | `docker stats`: Docker's `ContainerStats` (`cpu_stats` with `precpu_stats` the sample before, `memory_stats` with `memory.stat`'s lines, `pids_stats`, `blkio_stats`, `networks` without `lo`); `stream` (the default) one object a second, zeros while the container isn't running (as Docker's), until it's removed, the client goes or the service stops; `stream=0` one object from two samples a second apart (`one-shot=1`, or one not running: one sample); `networks` left out for one on the host's network; one that can't be read (rootless without a cgroup of its own) refused before the stream, with its status |
| `POST /containers/{id}/pause`, `/unpause` | `docker pause`, `docker unpause`: `204`; one not running, or already paused (or not paused), `409` |
| `POST /containers/{id}/rename` | `docker rename` (`name`), and `docker compose up`'s recreate (it renames the new container to the old's): `204`; no such container `404`; a name taken `409` |
| `POST /containers/{id}/update` | `docker update`: Docker's resource fields (`Memory`, `MemorySwap`, `MemoryReservation`, `NanoCpus`, `CpuPeriod`, `CpuQuota`, `CpuShares`, `CpusetCpus`, `CpusetMems`, `PidsLimit`, `BlkioWeight`) and `RestartPolicy`, as `shipyard update` takes them; `200` with Docker's `Warnings` (a limit the host can't apply) |
| `POST /containers/{id}/wait` | `docker wait` (`condition`: `not-running`, `next-exit`, `removed`); its headers are answered at once and the exit code when it comes, as Docker's; an end before a restart counts, as Docker's; a `--rm` container's code is read from the [events](events.md) journal |
| `DELETE /containers/{id}` | `docker rm` (`force`; `v`: its anonymous volumes too, each no other container names) |
| `GET /containers/{id}/logs` | `docker logs` (`stdout`, `stderr`, `follow`, `since`, `until`, `timestamps`, `tail`: a number, anything else all of it, as Docker reads it; with `until`, the tail is of the lines up to it): Docker's framed streams, or a terminal container's output as it is; a follow ends with its container, when the client goes, or when the service stops |
| `POST /containers/{id}/attach` | `docker attach`, `docker run` without `-d` (with `-i`, `-t`), `docker start -a`: the connection taken over (`Upgrade: tcp`, answered `101`), the container's output framed as Docker's (raw with a terminal), your input passed on (`stdin=1`), detached by `detachKeys` (`ctrl-p,ctrl-q` by default); one to a container not running (made, or ended, as `docker start -a` asks) waits for its next start, and is let in before it, so nothing it writes first is missed; a paused or restarting one is refused (`409`), and so is an attach without `Upgrade` (`400`); with `-i`, the container's input ends at yours (`docker run -i … < file`). `logs=1` isn't supported yet (`400`) |
| `POST /containers/{id}/resize` | the container's terminal resized (`h`, `w`), as `docker run -it` and `docker attach` do when your window changes |
| `POST /containers/{id}/exec`, `POST /exec/{id}/start`, `GET /exec/{id}/json`, `POST /exec/{id}/resize` | `docker exec` (`-i`, `-t`, `-d`, `-u`, `-w`, `-e`): run as `shipyard exec`, a process of the service's own, so a variable's value is never on a command line; its exit code in the inspect; `Privileged` is refused (`400`), and so is an exec into a container that isn't running (`409`). Execs are kept by the service while it runs, as Docker's are by its daemon (one never started, or ended, for 5 minutes). An exec's variables may hold 120 KiB at most (`400`). A client that hangs up hangs its exec up (SIGHUP, then SIGKILL after 10 s); a process an exec leaves in the background doesn't hold its end up (its output is read for 2 s more). With a terminal, its detach keys (`DetachKeys`: Docker's ctrl-p,ctrl-q unless given; a bad one `400`) end the session and leave the exec running, as Docker's. A detached exec (`-d`) keeps the service up 10 s at most: a long one runs on after the service's idle exit, its instance gone with the service (`404` then), as Docker's are at its daemon's restart. One detached from with its keys keeps the service up until it ends (its terminal is the service's: the service's exit would hang it up). `Pid` in the inspect is the command's, on the host, once it runs. `resize` of an exec without a terminal is `409` |
| `POST /images/create` | `docker pull` (`fromImage`, `tag`; a `tag` replaces one `fromImage` has): the image pulled, its progress as Docker's JSON lines, one line a layer as it's stored (`Pull complete`, or `Already exists`), never byte by byte; a failure once the pull is under way comes in the stream, as Docker's, without the control characters a registry's words may hold. `Digest:` is the multi-platform index's the reference named, as Docker's, else the image's manifest's. With `X-Registry-Auth` (a username and password, an identity token, or a registry's own bearer token, `registrytoken`, read first, as Docker reads them) (the credentials `docker login` keeps, which Docker's CLI hands in) it signs in as you; without, with the service's own Docker config. Your credentials are used for that pull alone: never logged, kept, or shown. `platform` asks for one, which must run here (on a Mac's VM, `linux/amd64` too, through Rosetta: `400` otherwise); without it, the image's own among what runs here, the host's first. With `fromSrc`, it's an import (below). A client that goes doesn't cut a pull short |
| `GET /images/json` | `docker images` (filters `reference`, a pattern of `*` and `?`, `[…]` refused; `label`; `dangling`: an image with a name never is). An image that can't be read is left out of the list, not the list refused. `Size` is the layers' as pulled (compressed): Shipyard keeps no total of their unpacked bytes. `Containers` and `SharedSize` are `-1`, as Docker's list says them (its daemon counts them for `system df` alone) |
| `POST /images/{name}/push` | `docker push` (`tag`; none, and a name without one: every tag of the repository, as `docker push -a`): the image pushed as it's stored (see [push](images.md#pushing-push)), signed in with `X-Registry-Auth` (what `docker login` keeps, which Docker's CLI hands in), else the service's own Docker config; its progress as Docker's lines (`The push refers to repository […]`, a line a blob, `Pushed` or `Layer already exists`), then for each tag `TAG: digest: … size: …` and Docker's `aux` (`Tag`, `Digest`, `Size`). A failure once under way comes in the stream. A client that goes doesn't cut a push short |
| `POST /auth` | `docker login`: the credentials (a username and password, or a token) checked against the registry `serveraddress` names (Docker Hub if none) at its `/v2/`, as Docker's daemon checks them: `200 {"Status": "Login Succeeded"}`, or `401` with Docker's `unauthorized: …`. Nothing is kept: your CLI keeps them in your Docker config, as it does with Docker, and hands them in with each pull. Never logged or shown |
| `GET /images/{name}/json` | `docker image inspect`: Docker's keys at 1.43. An image's `Id` is its manifest's digest (Docker 29's own, for an image of one platform); `RepoDigests` name it by the multi-platform index it was pulled through, if it was, as its registry knows it (one pulled by an older Shipyard: its manifest); `Metadata.LastTagTime` is when its reference was last pulled to it or tagged; a name is a reference, a repository's digest (`alpine@sha256:…`, as `RepoDigests` names it), or an ID: the manifest's digest, whole or its first 4 hex digits or more (`400` if they start more than one image's), or the config's digest, whole |
| `POST /images/{name}/tag` | `docker tag` (`repo`, `tag`) |
| `GET /images/{name}/history` | `docker history`: a step of the image's build a row, newest first, from its config's `history` (one with none: a row a layer). `Id` is the image's on the newest row, `<missing>` on the rest, as Docker says for an image it didn't build; `Size` the step's layer as stored (compressed); `Tags` the image's names on the newest row |
| `GET /images/{name}/get`, `GET /images/get?names=…` | `docker save`: the images (`names` repeated for more than one; a repository alone is every tag of it) as one tar archive, Docker 25's: an OCI layout (`oci-layout`, `index.json`, `blobs/sha256/…`) with Docker's `manifest.json`. Each image's own blobs as stored, so it keeps its ID; the same images give the same bytes. One that isn't there is `404` before anything is sent; a failure once the archive is under way cuts it short |
| `POST /images/create?fromSrc=-` | `docker import` (`repo`, a whole reference as Docker's CLI sends it, `tag` added to one without; `changes`, repeated; `message`; `platform`, one that runs here): the body, a root filesystem's tar (plain or gzip), made a one-layer image (see [import](images.md#a-filesystem-made-an-image-import)); Docker's `{"status": ID}` line. No name, a URL `fromSrc`, a change import doesn't take: 400, read by the client even mid-body (the rest of the body is read and dropped, up to 1 GiB) |
| `POST /images/load` | `docker load` (`quiet`): the body an archive, plain or gzipped (see [Saving and loading](images.md#saving-and-loading-save-load)), read whole before anything is loaded; Docker's JSON lines (`{"stream": "Loaded image: NAME\n"}`), a tag the load moved and what it left out said first. A cut, malformed or unsound archive is `400`, nothing loaded |
| `DELETE /images/{name}` | `docker rmi` (`force`; `noprune` taken and ignored): Docker's `Untagged` and `Deleted` list. An image a container was made from is refused (`409`), and so is an ID whose references are in more than one repository, without `force` (which removes them all), as Docker's rule is |
| `GET /volumes` | `docker volume ls` (filters `name`: what it contains; `label`; `dangling`: no container mounts it; `driver`: `local`). A volume's labels or time that can't be read are said in `Warnings`, not the list refused |
| `POST /volumes/create` | `docker volume create` (`Name`, none for a new 64-hex one, anonymous as Docker marks it: `com.docker.volume.anonymous`; `Labels`, 64 KiB at most): an existing volume is answered as it is (its labels unchanged), as Docker's `local` driver does. Only the `local` driver, and no driver options (`400`): a volume is a directory of Shipyard's |
| `GET /volumes/{name}` | `docker volume inspect`: Docker's keys (`Labels` `null` when none; `CreatedAt` to the second: its making, kept beside it; a volume made before 0.4 says its directory's last change, which its first use may have moved) |
| `DELETE /volumes/{name}` | `docker volume rm` (`force`: one that isn't there is no error): one a container mounts, running or not, is refused (`409`) |
| `POST /containers/prune` | `docker container prune` (`filters`: `until`, `label`, `label!`): every stopped container (made, or ended; never one running, paused or about to restart); `ContainersDeleted` (`null` for none) and `SpaceReclaimed` (their writable layers); another key `400` |
| `POST /images/prune` | `docker image prune` (`filters`: `dangling`, `true` by default, `false` for `-a`; `until`, `label`, `label!`): the images no container was made from, then what nothing in the store needs; `ImagesDeleted` (`{"Untagged"}` then `{"Deleted"}`, `null` for none) and `SpaceReclaimed` |
| `POST /build/prune` | `docker builder prune`, and `docker system prune`'s last ask: every context's build cache no build is using, removed (one in use is kept); `CachesDeleted` (`null` for none) and `SpaceReclaimed`. `POST /build` (`docker build`) isn't served: use `shipyard build` |
| `POST /volumes/prune` | `docker volume prune` (`filters`: `all`, `label`, `label!`): as Docker's API since 1.42, only anonymous volumes (labelled `com.docker.volume.anonymous`) unless `all` (a client of an older API: every unused one, as Docker's daemon reads it); a volume whose labels can't be read is never taken; never one a container mounts; `SpaceReclaimed` is what they held |
| `GET /networks` | `docker network ls` (filters `name`: what it contains; `id`: its start; `label`; `driver`; `type`: `builtin`, the default `bridge` among them, or `custom`; `dangling`: no container up on it, and never a built-in, as Docker's; refused while a container's record can't be read, which may name one). Root's: Shipyard's networks with `host` and `none`, and the default `bridge` (made first if it isn't there, where runs get it). A network whose labels can't be read is listed without them. Rootless (bridges are root's): `host`, `none` and `pasta` |
| `GET /networks/{id}` | `docker network inspect`: Docker's keys, its containers by ID. A network is named by its name, its ID (64 hex, made at its making; one made before Shipyard kept IDs has one derived from its name and the second it was made), or the start of its ID |
| `POST /networks/create` | `docker network create` (`Internal`, `EnableIPv6`, one `IPAM.Config` a family: `Subnet`, `Gateway`; `Labels`; `Options`). Docker's bridge options pass at their defaults (`enable_icc`, `enable_ip_masquerade` true, `host_binding_ipv4` 0.0.0.0, `driver.mtu` 1500); `com.shipyard.network.isolate-host` and `com.shipyard.network.conntrack-limit` turn on Shipyard's own; any other option or value is refused (`400`), as are another driver, an IP range, auxiliary addresses, an attachable, ingress or config-only network. A name taken, the default network's (`bridge`) among them, is `409`; `default`, the name Docker keeps for the default network, is `403`. Rootless: `403` |
| `DELETE /networks/{id}` | `docker network rm`: a pre-defined network (`bridge`, `host`, `none`) is refused (`403`), and one a container uses (`409`). Rootless: `403` |
| `POST /networks/{id}/connect`, `…/disconnect` | `docker network connect` (`--alias`, `--ip`, `--ip6`; links, a MAC address and endpoint options refused) and `disconnect` (`Container`, `Force`: as `shipyard network disconnect --force`, it lets a container leave the network its way out or its published ports are on, so it can cut a running container off), as `shipyard network connect` does: at once for a running container, at its next start otherwise. A built-in is refused (`403`) |
| `POST /networks/prune` | `docker network prune` (`filters`: `until`, `label`, `label!`): every network no container uses, never the default one; `NetworksDeleted` `null` for none. Rootless: none to prune (`null`) |
| `GET`, `HEAD`, `PUT /containers/{id}/archive` | `docker cp`, both ways, the container running or not, as `shipyard cp` copies (`path` required). `GET` sends a tar of the path (a symlink as one) as it's made; `HEAD` only what the path is, in `X-Docker-Container-Path-Stat` (Docker's: its name, size, Go's mode bits, time, and a link's target). `PUT` extracts a tar into the directory `path`, plain or gzip (bzip2 and xz are refused, `400`, once its container is found: a missing one's `404`); `copyUIDGID` keeps the archive's owners; a directory never replaces a file or the reverse (`noOverwriteDirNonDir`, always so). A path that isn't there is Docker's `404 Could not find the file …`; `path` not a directory, `400`; a read-only root or volume, `403`. A copy holds its container (a start or stop of it waits): a client that stops reading or sending for 5 minutes is let go, and a copy that fails half way breaks its stream off, never ends it as a whole one. A link's target is its chain of links followed in the container, as Docker's (a link to a link names the file at its end; at most 255 links) |
| `GET /containers/{id}/export` | `docker export`: the container's own filesystem as one tar, running or stopped, as `shipyard export` writes it (see [export](containers.md#its-whole-filesystem-export)): never its volumes, binds or tmpfs; sent as it's made. One that isn't there, `404` (`No such container`); a failure half way breaks the stream off. It holds its container while it streams, as a copy does |
| `GET /shipyard/containers/{id}` | Shipyard's own view of a container, as `shipyard inspect --shipyard` prints it (`404` if there's none) |
| `GET /shipyard/networks/{name}` | Shipyard's own view of a network, as `shipyard network inspect --shipyard` prints it. Rootless: `403` |
| `GET /events` | `docker events` (`since`, `until`, `filters`: `type`, `event`, `container`, `image`, `volume`, `network`, `label`), Docker's JSON lines; followed until `until`, or for good without one |

Paths with a version prefix (`/v1.43/containers/json`) are taken for any version from
1.24 to 1.43; a newer one is refused with Docker's "client version … is too new", and
Docker's clients then use 1.43. Answers are Docker's: its JSON, its statuses (`304`
for a start or stop with nothing to do, `404` for what isn't there, `409` for a
conflict), and its `{"message": "…"}` for a refusal.

At most 128 streams (followed logs, events) are served at once; one more is refused
(`503`) until one ends, so the other requests always have room.

What isn't served yet answers `404 page not found`. An events filter Shipyard doesn't read (`daemon`, say) is refused
(`400`), never ignored. A create never pulls: an image that isn't stored is Docker's
`No such image`, and the `docker` CLI then pulls it and creates again, as it does
with Docker.

## Creating containers

A create takes what `shipyard run` takes, as Docker's client sends it, and labels:
the command and entrypoint, environment, user, working directory, hostname, ports,
`-v` binds and named volumes, `--tmpfs` and `--mount` (`HostConfig.Tmpfs` and
`Mounts`: a bind, a named or anonymous volume or a tmpfs, its `SizeBytes` and
`Mode`; a volume's `NoCopy`; a bind's `CreateMountpoint`), `Volumes` (anonymous
volumes), restart policy, `--rm`, network and its aliases and
addresses, capabilities, DNS settings, extra hosts, groups, `--read-only`, security
options, sysctls, ulimits, devices, resource limits, the health check, the stop
signal and timeout. Each value is checked as the same `shipyard run` flag checks it.

A bind's host path that doesn't exist (a `Binds` entry, or a `Mounts` bind asking
`CreateMountpoint`, as compose's short syntax does) is made as Docker's daemon makes
it: a directory, with its parents, as the service's user, 0755 (less the umask), at
the create and again at each start (a `/tmp` directory a reboot cleared comes back).
`shipyard run -v` still refuses a missing host path. What Shipyard made is made, and
bound, without following a symlink: if something puts one where it made a directory
(a container that shares a directory above it, say), the start is refused with the
path rather than binding what the symlink names. A host path that was there already
is bound as Docker binds it, symlinks followed.

On a Mac, a bind's host path must be in a place the Mac shares with the VM (the home,
`/Volumes`, `/tmp`, `/var/folders`: [Your files](macos.md#your-files)), its links
followed in the VM, and never the VM's own files: else the create is refused (400,
`mounts denied: the path … isn't shared from the Mac`), as Docker Desktop refuses it,
rather than an empty directory made in the VM. A start checks it again.

Refused, with what to use instead (400):

| Asked for | Instead |
|---|---|
| `--privileged` | the capabilities (`--cap-add`) and devices (`--device`) it needs |
| a mount's propagation other than `rprivate`, a non-recursive bind, a volume's driver, labels or subpath, a tmpfs's own `TmpfsOptions.Options` | a plain bind, a named volume, a tmpfs |
| `--init` | run the command without it |
| `--volumes-from`, `--link` | name the volumes; put the containers on one network |
| `--pid`, `--ipc host` and `none`, `--uts`, `--userns`, `--cgroupns host` | a container has namespaces of its own (another container's network and IPC namespace are taken: `container:NAME`, see [Another container's network](networking.md#another-containers-network) and [Sharing another's IPC](containers.md#sharing-anothers-ipc---ipc-containername)) |
| a log driver other than `json-file` or `local`, a volume driver, a runtime | Shipyard keeps the logs, the volumes and runs containers itself |
| `--shm-size`, `--gpus`, `--storage-opt`, `--cgroup-parent`, `--oom-kill-disable`, `--memory-swappiness`, `--security-opt systempaths=unconfined`, `--blkio-weight-device`, the realtime CPU settings | create the container without them |
| `--security-opt seccomp=FILE` from Docker's CLI (it sends the profile's contents) | `seccomp=unconfined`, or Shipyard's own profile; a client that sends an absolute path has it read where the service runs |
| more than one network at create | `docker network connect` after |
| an address, alias or hardware address on the default network (`--ip`, `--network-alias`, `--mac-address` without `--network`) | make a network of your own and use `--network` |

Each of these is refused only when it's asked for: at Docker's defaults (what its
CLI sends when the flag isn't used) they pass. A terminal's size, which doesn't
change the container, is ignored. `ExposedPorts` are kept with the container
(`-P` publishes them, with its image's).

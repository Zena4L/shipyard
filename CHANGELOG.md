# Changelog

All notable changes to Shipyard. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (before 1.0, a minor version may change
behaviour; each change is listed here).

## 0.5.0 - unreleased

### Added

- **The Mac's VM takes its size from the Mac** (step 13.2): a quarter of its memory (4 to
  16 GiB) and half its cores (at most 8), instead of a fixed 4 GiB and 4 CPUs;
  `shipyard vm set --memory SIZE --cpus N` (or `default`) changes them for every start
  after, and `vm status` says what a start takes.
- **The VM gives freed disk space back to the Mac at each stop** (F103): its sparse
  `data.img` shrinks after a prune; `doctor` says what the disk takes and warns when the
  Mac runs short of free space.
- **A VM whose Linux hangs or reboots in place is stopped** (F90): the host asks for its
  boot ID every 10 seconds; the console log is capped at 4 MiB while it runs.

- **One signing identity across rebuilds on a Mac** (step 13.7, D13-3, F140):
  `scripts/macos/make-signing-identity.sh` makes a local code-signing identity (only
  its own key's access changed in your keychain), and
  `sign.sh` signs every build with it (without the hardened runtime, so a debugger
  attaches; a release's Developer ID signing has it), so macOS asks once,
  not after each rebuild, before a VM launchd started reads your guarded folders;
  `doctor` says how `shipyard` is signed. `scripts/macos/notarize.sh` does a release's
  Developer ID signing and notarization (not run until a release). Builds target
  macOS 13, and the VM refuses an older macOS by name.
- **The VM can start at login for containers that come back** (step 13.6b2, F106):
  `shipyard vm install --at-login` boots it as you log in only when containers with a
  restart policy would come back (the guest says how many as it runs; `vm status` says
  it too), and it stops by itself once none runs.
- **amd64 images on a Mac, through Rosetta** (step 13.3, D13-2): the VM shares the
  Mac's Rosetta for Linux and registers it at boot, so `linux/amd64` images run; one with
  no arm64 build is pulled as amd64, with a warning; `--platform` on `pull`, `run`,
  `create` and `build` (and the API's `platform`) asks for a platform, refused where it
  doesn't run. `doctor` says Rosetta's state, on the Mac and in the VM.
- **The Mac's VM shares Docker Desktop's places** (step 13.4, D13.4-1): `/Volumes`,
  `/tmp` and `/var/folders` beside the home, each at its own path, so a project in
  `/tmp` or on an external drive binds as it does in the home. `doctor` lists them and
  says which of the folders macOS guards (Desktop, Documents, Downloads) the VM may
  read; a refusal for one names the permission (F105, F140).

- **UDP ports reach the Mac** (step 13.5a, F96): a container's `-p …/udp` port answers on
  the Mac's `127.0.0.1` and `::1`, every program's datagrams told apart and answered
  back to it; `vm status` and `doctor` list them. The VM reaches each container at its
  own address for a forwarded port, TCP's too, not through its port rules (F97).

- **The Mac says when another program shadows a forwarded port** (step 13.5b, F98):
  `vm status` and `doctor` name a port another Mac program holds on every address,
  which `localhost` doesn't reach (the container does).

### Security

- **The VM's agent port can't be taken** (F108): the VM's init holds it from boot to
  stop and hands it to each agent it starts, so no container that can open vsock sockets
  takes it while the agent restarts; a command that waited past a minute for it is
  refused, not run late. `doctor` checks no command holds it.
- **Published ports don't answer on the VM's own network address** (F108): the guest's
  firewall (ruleset 9, an `outside` set) refuses what reaches them from its link to the
  Mac's virtual network, so other VMs there don't.
- **A bind in a Mac's share is bound from the share itself** (step 13.8c, F173a): its
  source is judged once more as the container starts, then reached from the share without
  following any link and bound by descriptor, so a container that writes in the share can
  no longer swap in a link, after the check, that binds the VM's own files into another
  container. A link put there makes the start fail instead.

### Changed

- **A bind source Shipyard made is refused, not followed, once its directory has become a
  symbolic link** (step 13.8c): each start reaches the directory it was made in without
  following a link at its end (one higher up is followed, as before); a container whose
  directory was swapped for a link since its create fails to start, saying so. A source
  Shipyard made, or one bound by descriptor, is a directory or a regular file, never a
  device, a FIFO or a socket put in its place.
- **A bind outside what the Mac shares is refused, through Docker's API too** (F143):
  `mounts denied: the path … isn't shared from the Mac`, as Docker Desktop says, where it
  used to bind an empty directory made in the VM. So is a bind of the VM's own files or
  of a socket. A container made before with such a bind keeps it; one whose source is
  in a share is checked again at each start (a link in it may lead out since).
- **The VM's `/tmp` is the Mac's**: what the guest itself writes there moved to its
  `/run/tmp`.

### Fixed

- **A Mac's signals reach a command that doesn't read its input** (step 13.6a, F94):
  a Ctrl-C, a `kill`, a hang-up or a resize used to wait behind input the command hadn't
  read (`yes | shipyard run -i … sleep 1000`, a paste into a busy terminal); they go on a
  connection of their own now (attach protocol 6).
- **A VM started by another build is said** (F167, F173b): `vm status`, `doctor` and
  each command say when the running VM booted another build's Linux than the one there
  now, with what to do, and a request it can't read is refused with those words, not
  "a request of version 3, newer than this guest's 2".
- **A Docker client's hang-up on the Mac reaches the right connection** (F155): it's
  named by a token, not by a port another connection may have taken.
- **A Mac command whose command ends just as the 10 s after a SIGTERM run out** exits
  with the command's code, not 143 (F109); and a connection whose other side still sends isn't cut a minute after one
  side has finished (the wait counts from the last byte, 10 minutes at most).
- **`vm stop` asks the VM's host, never a PID** (step 13.6b1, F93): on the host's own
  socket (`host.sock`), there from its start; a stop sent while the VM starts succeeds
  (taken once it's up: a disk check is never cut), where it used to report a failure.
  The stop signals can no longer end the host before its stop runs.
- **A slow start is waited for by the host's own word** (F100, F144): a first command
  that gives up never stops a VM that's still booting in time or already up, and never
  signals a PID; a launchd start whose run count couldn't be read sees its host's
  failure at once.
- **The VM's host keeps its own log short** (F106): a second command no longer moves
  a running host's log aside, so the reason a start failed is where it's looked for;
  `vm status` and `doctor` say when the running VM shares another home or guest
  directory. launchd now gives the host about 15 minutes to stop at a logout (a disk check
  under way, then its containers).
- **A foreground `run -i` whose container ended with input still queued** could lose
  the container's exit code ("the container's monitor is gone", 125): what the monitor
  said before it closed is now read.
- **The VM's host serves every connection blocking** (F147): one accepted from launchd's
  socket after a start that failed could have been non-blocking, its copy ending early.
- **The host's sockets share one budget** (F152): 1,024 connections in all, the
  forwarded ports at most 768 of them, so a load on a published port never shuts out
  `shipyard` or `docker`; `vm status` and `doctor` say when launchd's agent is installed
  but not loaded (nothing serves `docker.sock` then); `system service` on a Mac points
  at the VM's `docker.sock`.
- **The VM asks its guest for ports less often while nothing runs** (F106): from every
  2 s to every 10 s, at once again after any command.
- **The VM's guest is built from exactly the packages it was pinned to** (F92): every
  Alpine package it installs, dependencies too, is checked against
  `scripts/guest/alpine.installed`, so two builds of one lock can't differ.
- **A `shipyard` built from the source tree finds its guest** (`target/guest`) without
  `SHIPYARD_GUEST_DIR`; `vm install` checks the guest it records is there.
- **A copy of the VM's directory run as a second VM** shared the first's network address,
  and the two clashed on the Mac's network (F138): each directory's card has its own
  (`network.json` v2; an older `shipyard` refuses it: remove it, and that one makes a new
  address).
- **A UDP port that streams to a Mac client no longer holds `vm stop`** (step 13.8c): the
  relay took the guest's answers for as long as any was waiting, so a container that never
  paused kept the stop (and the idle stop) waiting for good.
- **Heavy UDP traffic both ways no longer resets a forwarded port's stream** (step 13.8c):
  each side of it waited to write while the other did the same, then broke the stream off
  after a second, and every client's source port changed. Neither side waits now; a
  datagram with no room is dropped, as UDP drops it.
- **A Mac killed during the VM's first boot can't leave a half-made disk** (step 13.8c):
  the new disk was kept as `data.img` once the guest's agent answered, while the guest was
  still formatting it; it's kept once the guest is ready, so a first boot cut short starts
  over with a new disk instead of one every later boot refuses.
- **A build's `FROM --platform=` stage no longer gets the store's image of another
  platform** (step 13.8c): the store's images are given to BuildKit for their own platform
  alone, so a stage of another platform pulls its base, as Docker's builder does.
- **`save`, and `cp` from a stopped container, read an image of any platform** (step
  13.8c): an amd64 image could no longer be saved, or copied out of, once the VM had lost
  Rosetta.

### Known limitations

- **The VM keeps the memory it was given** (F168): a balloon was tried, and macOS took
  nothing back; `vm set --memory` sizes it instead.
- **Small files on a share are slower than OrbStack's** (F172): about 3.6 times when
  writing many, on par for reads and large files; nothing in Virtualization.framework's
  virtiofs to tune.
- **No installer yet** (F178): the guest is built from the source tree, which a `shipyard`
  built there finds by itself; an installed binary needs `SHIPYARD_GUEST_DIR`.
- **A bind of Docker's socket is refused on a Mac** (F171): Testcontainers' Ryuk needs
  `TESTCONTAINERS_RYUK_DISABLED=true` there for now.
- **A build's context must be in your home on a Mac** (F181): one in `/tmp`,
  `$TMPDIR` or `/Volumes` is refused as one the builder can't enter.
- **Upgrading from 0.4 on a Mac** takes a `vm stop`, a guest built again and, if it was
  installed, `vm install` again ([macOS](docs/user/macos.md#upgrading-from-04)): `vm
  status` and `doctor` say when the running VM or launchd's agent is the older build's,
  and an older guest refuses a command naming `/tmp` or `/Volumes` rather than bind a
  directory of its own.

## 0.4.0 - unreleased

### Added

- **`save`, `load` and `history`** (step 12.2a), in the CLI (`image save|load|history`
  too) and the API (`GET /images/{name}/get`, `GET /images/get?names=…`, `POST
  /images/load`, `GET /images/{name}/history`), so `docker save`, `docker load` and
  `docker history` work through it. `save` writes Docker 25's archive (an OCI layout
  with `manifest.json`), each image's blobs as stored: its ID kept, the same bytes
  each time. `load` reads that, Docker's older archive and BuildKit's OCI layout,
  plain or gzipped, as hostile: members matched by name and never extracted, every
  blob checked against its digest, staged and moved into the store only once the
  whole archive is read and sound, nothing fetched from a registry; a cut archive
  loads nothing, a tag it moves is said, an untagged image is refused (said).
- **`export` and `import`** (step 12.2b), in the CLI (`container export`, `image
  import` too) and the API (`GET /containers/{id}/export`, `POST
  /images/create?fromSrc=-`), so `docker export` and `docker import` work through it.
  `export` writes a container's own filesystem, running or stopped, never its volumes:
  a running one's root mount is read alone (a copy of it with nothing mounted on it,
  made in its namespaces), so what a volume covers is exported as it is underneath, as
  Docker's. `import` makes a root filesystem's archive a one-layer image, named (a name
  is required), gzipped as it's stored, its config set by `--change` (`CMD`,
  `ENTRYPOINT`, `ENV`, `EXPOSE`, `LABEL`, `USER`, `VOLUME`, `WORKDIR`, `STOPSIGNAL`);
  the archive read whole and checked before anything is kept.

- **`build`** (step 12.3, D28), also `image build`: a Dockerfile built by BuildKit
  (`moby/buildkit:v0.34.1`, pinned by digest) in a container Shipyard runs for the build
  and removes at its end, in a user namespace with `CAP_SYS_ADMIN` only there and seccomp
  unconfined (said in the docs). `-t`, `-f` (`-` too), `--build-arg`, `--target`,
  `--no-cache`, `--pull`, `--label`, `-q`, `--progress`, `--secret`, `-o type=local|tar`.
  The image loaded under the build's names alone; a context the builder couldn't read
  whole refused by name (BuildKit would copy an unreadable file empty). A cache per
  context directory, kept between builds; `builder prune`, `system prune`'s and the
  API's `/build/prune` empty it; `system df` has its `Build Cache` row.

- **`push`, `login` and `logout`** (step 12.4), also `image push`; and the API's `POST
  /images/{name}/push`, so `docker push` works through it. A push sends an image's
  stored blobs and manifest as they are (its ID kept), skips blobs the registry has,
  signs in for push access, never follows an upload to another host, and starts an
  upload over when a step fails or a token expires. `login` checks a sign-in, then keeps
  it where `docker login` does (Docker's config: its credential helper, else the file,
  warned as unencrypted; every other key, the file's mode, owner and link kept);
  `logout` removes it. On a Mac, `login`/`logout` run on the Mac, and a forwarded
  `push` or `pull` is handed the Mac's credential for its registry.

- **A build uses the store's own images** (12.5's readiness review): a `FROM`, `COPY
  --from` or `RUN --mount=from=` naming an image Shipyard has (built, pulled, loaded or
  imported) is built from it, as Docker's builder does, never pulled (`--pull` asks the
  registry again); a stage of the Dockerfile's own name is always the stage. Each image the Dockerfile or a build arg mentions is given to
  BuildKit as a named context over an OCI layout of its blobs, hard-linked from the
  store, read-only (where they can't be linked, said, and pulled).
- **A build cache no build has used for a week is removed** by the next build of
  another context (said): a host that builds in a new directory each time (CI) no longer
  keeps each one's cache, about 10 GB, for good.

### Changed

- **Pull and push progress show sizes as Docker's pull does** (`2.048kB`, `4.089MB`,
  not `2.0 KiB`), with the rest of the CLI's formatter (F145).
- **A program is never looked for through a relative `PATH` entry** (`.`, an empty
  one): `newuidmap`/`newgidmap` were found through one before, so a program of that
  name in the directory a rootless command was run from could have been run in their
  place (the hint a `run` gives for a host command looked there too) (F145).
- **A registry that takes a credential but not the account's access** (a token the
  registry then refuses) is said as `denied: …` for the repository, no longer as
  credentials to sign in again with; messages name `shipyard login` beside `docker
  login`.

### Fixed

- **`rm` right after a container ended for good** (a `start -a` then `rm`, a `stop`
  then `rm` as compose's `down` does, whatever its restart policy) could be refused as
  "restarting": its clean-up took the restart lock for a moment at every end. It takes
  it only for an end its policy starts again (found by 12.5c3's full run and review).
- **`docker exec -i` without a terminal** passed the client's input on only once it
  ended (a line typed into an interactive `sh` waited for the client to close): it
  goes on as it comes (found in 12.5a).
- **A volume's `CreatedAt`** (`volume inspect`, the API) moved when its first fill made
  and removed its marker beside the data: the making's time is kept in its own
  `created.json` (a volume made before keeps the old answer) (F146).
- **An exec whose start failed** (no process could be made) was left "started", and a
  retry refused: it can be started again; and a finished exec's session now ends both
  ways, so the service's thread passing its input on goes at once (F148).
- **A service `shipyard compose` (or the macOS guest) started** removes its socket's name
  before its idle end, so a client never connects to a service about to go (it starts a
  new one); its output goes to `service.log` beside the socket (it went nowhere); and
  the systemd units set no start limit, so a client retrying a request that crashed
  the service can't leave its socket off (F149).
- **`save` of an image pulled through a multi-platform index** now carries that index
  (as Docker 25's), so a name with the index's digest loads back, and the image keeps
  it (12.2a wrote the platform's manifest only, and `load` refused such a name).
- **A load or import refused before its body's end** (no name, an archive found
  unsound half way) no longer leaves Docker's CLI with a broken pipe instead of the
  refusal: the API reads and drops the rest of the body (up to 1 GiB, a minute).
- **`build` on a Mac** refused every context in the home (the guest checked it as root
  for other users' access, which a Mac's home never gives): the home the VM shares is
  read by the builder as you read it, and is checked so (the stage-12 close).
- **A killed build's builder** ran on until the next build of its context, and a cache
  prune removed the cache under it: `builder prune` (and `system prune`'s build cache)
  removes the builder first, and `container prune` leaves a build's builder to it.
- **`build -f -` on a Mac** read a Dockerfile typed at the terminal (until Ctrl-D): it's
  refused there as on Linux; the Mac's terminal refusals give Linux's hints.
- **`--platform linux`** was taken by `pull` and refused by `import` and `build`: one
  rule for the three (the host's OS alone, its architecture alone, `os/arch`,
  `os/arch/variant`).
- **`save` of an image the store lost a blob of** was said as a bad request: it's the
  store's (as `push` says it), and a damaged record is refused, never saved without
  its index.
- **Every rootless run with a named volume failed on Fedora** (and wherever `newuidmap`
  has file capabilities instead of being setuid root): the volume's fill helper hid
  itself before its ID maps were written, and such a `newuidmap` couldn't open them. It
  lets them be written first (found by the stage-12 close's Fedora milestone; Ubuntu's
  setuid `newuidmap` never showed it). The refusal now names both ways a `newuidmap`
  may be allowed.
- **An error whose reason was part of its own words said it twice** (`load` of an
  archive with an unsafe entry: "…has an unsafe path: …has an unsafe path"): each reason
  is said once (found by the stage-12 book's "Try it").
- **A build said "waiting for another build"** when `system df` counted its cache at
  the same moment: said only after a second's wait.

### Security

- **A repository's `Dockerfile` (or its `Dockerfile.dockerignore`) that's a link** was
  followed anywhere: a hostile checkout could hand BuildKit any file you (or, as root,
  root) can read, such as a private key, for a `# syntax=` frontend or a parse error to
  show. Each is read only from inside the context or `-f`'s own directory; a link
  leading out is refused.
- **A build's secrets** were copied beside its Dockerfile, where a frontend reads every
  file: they're bound apart.
- **An archive `save -o`, `export -o` or `build -o type=tar` wrote** was readable to
  every user (0644 by the umask, and an existing 0600 file replaced by one): it's 0600,
  as Docker's (12.5's readiness review).
- **As root, `build -o type=local,dest=DIR` followed a symbolic link** at DIR or on its
  way: another user's `/tmp/out` linked to `/etc` would have had the build's files
  written there as root. DIR is reached without following one, and one on its way is
  refused (12.5's readiness review).

## 0.3.0 - unreleased

### Added

- **`docker rename` through the API** (step 11.6b): `POST /containers/{id}/rename`.
  `docker compose up` renames the container it recreates (a changed file,
  `--force-recreate`) once the old one is gone; without it, a recreate stopped half
  way, leaving the project's container under a temporary name.

- **`docker login` through the API** (step 11.6a2, F118): `POST /auth` checks the
  credentials against the registry's `/v2/`, as Docker's daemon does, and keeps
  nothing (the CLI keeps them in its config). A registry's own bearer token
  (`registrytoken`) is read, first, from `X-Registry-Auth` and from Docker's config,
  and sent to the registry as it is.
- **Images say more, as Docker's do** (step 11.6a2, F118): an image pulled through a
  multi-platform index is named by the index in `RepoDigests` and in a pull's
  `Digest:` (kept in the store's index; its ID is still its manifest's);
  `Metadata.LastTagTime` is its last pull or tag.
- **`network ls -f dangling=true`** (step 11.6a2, F120): the networks no container
  that's up is on, never a built-in, as Docker's `dangling` filter has them. It, and
  `network rm`, refuse while a container's record can't be read (it may name the
  network): `network rm` used to leave such a record out.
- **`inspect`'s `ResolvConfPath`, `HostnamePath` and `HostsPath`** (step 11.6a2,
  F121): a container's `/etc` files on the host while it's up.

- **`shipyard vm install` / `uninstall`** (step 11.5b2a): launchd holds Docker's socket
  on the Mac while the VM is stopped and starts the VM at its first connection, so
  `docker` works any time and nothing runs until it's used. Once installed, `shipyard`
  starts the VM through launchd too. `vm status` and `doctor` say whether it's
  installed, and `doctor` says when the agent runs another binary than this one.
- **`shipyard compose` on the Mac** (step 11.5b2a): the VM started, then Docker's own
  `docker compose` against its socket.

- **Docker's API on the Mac** (step 11.5b1): while the VM runs, it serves Docker's API
  on `docker.sock` in its directory (`~/Library/Application Support/Shipyard/vm/`), and
  the guest starts its service on the first connection. `docker`, `docker compose` and
  Testcontainers use Shipyard as a Docker daemon there (`DOCKER_HOST`, or a Docker
  context of your own). The socket is yours alone; a connection open keeps the VM up.

- **Testcontainers for Rust against the API** (step 11.4b1): an acceptance test
  (`crates/shipyard-acceptance`, dev-only) runs Testcontainers' everyday calls against
  `shipyard system service`, rootless and as root.

- **A new volume gets its image's files** (step 11.4a2b, F129): at a container's
  create, a volume (named or anonymous) still empty is filled with what the image
  has at its place, owners, modes and links kept, its directory taking the place's
  owner and mode, as Docker's; one fill of a volume at a time; a failed fill fails
  the create and is finished by the next. `-v NAME:/path:nocopy`, `--mount
  …,volume-nocopy` and the API's `NoCopy` ask for none. The API makes a bind's
  missing host path, as Docker's daemon does (a `Binds` entry, or
  `BindOptions.CreateMountpoint`: compose's short-syntax binds), at the create and
  each start (kept in `created.json` v3); `shipyard run -v` still refuses one.

- **Anonymous volumes** (step 11.4a2a): `-v /path`, a `--mount` volume without a
  source and the image's own `VOLUME`s (each no mount covers) get a new volume at
  the create, named by 64 hex digits and labelled `com.docker.volume.anonymous`;
  the API takes `Volumes`, a `Binds` entry `/path` and a `Mounts` volume without a
  `Source` (they were refused). `rm -v` and the API's `DELETE ?v=1` remove those
  the container's create made (kept in `created.json`, v3), never one it was given
  by name nor one another container names, and so does `--rm`; a plain `rm` keeps
  them for `volume prune`. An image's `VOLUME`s under `/dev` or on the run's
  `/etc` files are left in the container; more than 128 are refused.

- **`shipyard compose`** (step 11.4a1, D27): Docker's own `docker compose`
  against Shipyard's API, its `DOCKER_HOST` set; the API's service started when
  nothing serves the socket (it ends once idle). `docker events` (and `shipyard
  events`) take `--filter label=KEY[=VALUE]`: a container's events carry its labels,
  as Docker's do (compose's attached `up` follows them).

- **`--mount` and `--tmpfs`** (step 11.3e2): Docker's `--mount` (`type=bind`,
  `volume` or `tmpfs`, `readonly`, `tmpfs-size`, `tmpfs-mode`) and `--tmpfs
  PATH[:OPTIONS]` (`noexec,nosuid,nodev` unless asked, as Docker's), for `run` and
  `create`; the API takes `HostConfig.Tmpfs` and `Mounts` (they were refused).
  A tmpfs is fresh at each start, kept beside the record in `created.json` v2, and
  shown in `inspect`'s `HostConfig.Tmpfs` and `Mounts`.

- **`container prune`, `system prune`, `image prune -a`** (step 11.3e1):
  Docker's prunes, with its warnings, sections and `Total reclaimed space`, and
  `--filter until=… label=… label!=…`. `system prune` takes `-a` (every unused
  image) and `--volumes` (unused anonymous volumes). The API serves `POST
  /containers/prune`, `/images/prune` and `/build/prune` (no build cache yet).

- **`top` and `stats`** (step 11.3d2): `top` lists a container's processes in
  `ps -ef`'s columns, read from `/proc`; `stats` shows what containers use (CPU,
  memory, network and block I/O, processes) as Docker's CLI computes it, every
  second or once with `--no-stream`, with `--format`, `-a` and `--no-trunc`. The API
  serves `GET /containers/{id}/top` and `/stats` (streamed, or once).

- **`pause`, `unpause` and `update` of limits** (step 11.3d1): `pause` freezes a
  running container's processes (its cgroup's freezer) until `unpause`, said as
  Docker's events, `ps` showing `(Paused)`; `update` takes Docker's limit flags
  (`-m`, `--cpus`, `--pids-limit`, …) beside `--restart`, changing a running
  container's cgroup at once and its record for its next starts. The API serves
  `POST /containers/{id}/pause`, `/unpause` and `/update`.

- **`inspect` of any object, `version`, `info`** (step 11.3c2): `inspect` finds a
  container, an image, a network or a volume by name (Docker's order; `--type` one
  kind), with `-f` (a Go template, or `json`) and `-s`; `image`, `volume` and
  `network inspect` take `-f`; `version` and `info` print Docker's layout, with
  `--format` over Docker's objects (the API's `/version` and `/info` are the same,
  now the engine's); `system df --format`.

- **Docker's `--format`, filters and columns for the lists** (step 11.3c1): `ps`,
  `images`, `volume ls` and `network ls` print Docker's default tables and take Go
  templates (`table …`, `json`, `{{.Names}}`, Docker's functions and `.Label "KEY"`,
  through the `gotmpl` crate), `-q`, `-f KEY=VALUE` and `--no-trunc`; `ps` also
  `-n`, `-l` and `-s`, and Docker's `ancestor`, `exited`, `health`, `network`,
  `volume`, `before` and `since` filters (in the API's container list too: one
  parser, the engine's; a `name` filter's regular expression refused where Shipyard
  can't read it, never matched as text); `images` a REPOSITORY, `--digests` and `-a`;
  `events --format` a template over Docker's event message, by Go's field names.
  What a template writes from an object can't act on a terminal or forge a row.

- **More of Docker's everyday flags** (step 11.3b2): `exec --env-file`; `logs --until`
  and `-n all`; `stop -t -1` (no SIGKILL); `volume create -l`, `-d local` and no name
  (a new one); `network create -l`, `-d bridge` and `-o` (Docker's options, checked as
  the API checks them). `exec --detach-keys` and `--privileged` are refused, saying
  why.

- **`run` and `create` take Docker's other everyday flags** (step 11.3b1): `--env-file`,
  `-l`/`--label` and `--label-file`, `--expose` (published by `-P` beside the image's
  ports), `--cidfile`, `-q` (no pull progress) and `-a` (only the streams asked);
  `--privileged` and `--init` are refused, saying what to use instead. The API's
  create keeps a body's `ExposedPorts`.

- **A default process limit** (F62, step 11.3b1): a container made without
  `--pids-limit` (or with `0`, as Docker's CLI sends it) gets 2,048 processes
  (Podman's) at each start where the `pids` controller reaches its cgroup;
  `--pids-limit -1` asks for none. Elsewhere that start goes without one, and `doctor`
  says so. Containers made through the API (`docker run`) get it too.

- **Docker's everyday names and short forms** (step 11.3a): `run`/`create` `-v`
  (`--volume`) and `-h` (`--hostname`; their help is `--help`), `logs -n`, `stop`/`restart`
  `--timeout`, `rm -v`, `cp -q` and `image prune -f` taken, `shipyard -v` for the
  version; `tag`, `image inspect` (Docker's shape), and the `container` and `image`
  groups (`container ls`, `image rm`, …) as the same commands.

- **Shipyard's own views through the API** (step 11.2c3b): `GET
  /shipyard/containers/{id}` and `/shipyard/networks/{name}`, what `inspect --shipyard`
  and `network inspect` print; `inspect` gains Docker's `GraphDriver` (the overlay's
  directories), `HostConfig.LogConfig`, the image's own `ExposedPorts` and each
  network's `IPAMConfig`. A fuzz target for what a Docker client sends the API
  (`api_request`).

- **`docker cp` through Docker's API** (step 11.2c3a): the archive endpoints, both
  ways, the container running or not; a path's stat in Docker's header (`docker cp`'s
  checks and `-L`); a gzip archive taken as a plain one; a read-only root refused as
  Docker refuses it.

- **Docker's network endpoints** (step 11.2c2b): `docker network ls` (with `host` and
  `none`, and `pasta` rootless; filters `name`, `id`, `label`, `driver`, `type`),
  `inspect` (by name, ID or its start), `create` (labels and options kept in the
  network's own `labels.json`, versioned; Docker's bridge options at their defaults
  pass, what Shipyard can't honour is refused), `rm`, `connect`, `disconnect` and
  `prune`. Each network made from now has an ID of its own, kept with it (one made
  before has one derived from its name and when it was made); a container's
  inspect shows it (`NetworkID`, `EndpointID`).
- **A network can't be made with the default network's name (`bridge`)**, by the
  CLI or the API: made with other settings, it would have changed every default run.

- **Docker's volume endpoints** (step 11.2c2a): `docker volume create` (labels kept in
  the volume's own `labels.json`, versioned; a volume without a name gets a 64-hex
  one, marked anonymous), `ls` (filters `name`, `label`, `dangling`, `driver`),
  `inspect`, `rm`, and `prune` (only anonymous volumes unless `--all`, as Docker's API
  since 1.42; every unused one for an older client; the space it frees).
- **Docker's `label` filters match every label asked for**, as Docker's do (step
  11.2c2a): `docker ps --filter label=a --filter label=b` listed a container with
  either.

- **Docker's image endpoints** (step 11.2c1): `docker pull` (Docker's progress lines;
  your `docker login` credentials handed in and never kept), `docker images` (filters
  `reference`, `label`, `dangling`), `docker image inspect`, `docker tag`, `docker rmi`
  (by reference or ID). An image's ID is its manifest's digest.
- **`shipyard rmi` takes an image's ID** (its manifest's digest or its first 4 hex
  digits or more, or its config's whole), removing every reference of it, all or none
  (`--force` when they're in several repositories).

- **A Linux VM on the Mac** (step 10.3a, D24, D29): on Apple Silicon (macOS 13 or
  later), Shipyard boots one small Linux VM of its own with Apple's
  Virtualization.framework, and runs containers there. `shipyard vm start`, `status`
  and `stop`; the VM starts on first use, stops in order (its containers stopped, then
  the guest powered off), keeps its images and containers on a disk of its own across
  boots, and shares your home at the same path. Its guest is built from Alpine by
  `scripts/guest/build.sh`; the binary must be signed with the virtualization
  entitlement (`scripts/macos/sign.sh`).
- **Every command runs in the VM on a Mac** (step 10.3b1): `shipyard run`, `ps`, `logs`
  and the rest run there, as on Linux, from the same directory; `vm` and `doctor` stay
  on the Mac. Paths you name must be under your home (shared at the same path); a
  Ctrl-C or a kill is passed on; your input is read only by commands that take it; a
  bare `-e KEY` takes the Mac's value. See [macOS](docs/user/macos.md).
  `SHIPYARD_VM=off` runs commands on the Mac itself, as before.
- **A Mac's `shipyard doctor` shows the VM's own Linux checks** (stage 10's close): with
  the VM running, the engine's checks run in the guest (cgroups, overlayfs, nf_tables,
  the network, the runtime's state, as on a Linux host) and are listed under the VM's;
  a failed one fails `doctor`. A stopped VM isn't started to be checked.
- **The Mac's VM stops by itself when idle** (step 10.3c2, D20, D10.3-11): after 5
  minutes with no command and no container up (a detached container keeps it up), its
  containers stopped in order; the next command starts it again in about a second.
  `SHIPYARD_VM_IDLE` sets the wait. A command that lands as it stops runs, once, after.
- **`shipyard doctor` on a Mac checks the VM** (step 10.3c2): the macOS version and
  Apple Silicon, the binary's virtualization entitlement, the guest (and whether it's
  this build's), the VM's state, its ports, the shared home, its files.
- **Published ports on a Mac's `localhost`** (step 10.3c1, D10.3-10): `-p 8080:80`
  answers on `127.0.0.1:8080` and `[::1]:8080` on the Mac within a moment of the
  container's start, through the VM; only the Mac's loopback, never its network. A
  port another Mac program has is left to it and said (`shipyard vm status`), and
  taken once it's free. TCP only.
- **Terminals through the VM on a Mac** (step 10.3b2, D10.3-7): with your terminal as
  both its input and output, a command runs on a terminal in the VM: `run -it`'s
  shell, raw, at your window's size and following it, Ctrl-C reaching its job; a
  prune asks first; closing the window hangs the command up.
- **Docker's API, for Docker's own clients** (step 11.2a, D25): `shipyard system
  service` serves a subset of Docker's Engine API (1.43) on a UNIX socket, so
  `DOCKER_HOST=unix:///run/shipyard/api.sock docker ps` works: `version`, `info`,
  `ps` (with `id`, `name`, `label` and `status` filters), `inspect`, `create`,
  `start`, `stop`, `restart`, `kill`, `wait` and `rm`, and `run -d` of an image
  already pulled. systemd's `shipyard.socket` (root's, and a user's in
  `contrib/systemd/user`) starts it at the first connection, and it exits after 60 s
  with nothing asked (`SHIPYARD_API_IDLE`). The socket is its owner's alone, and each
  connection's user is checked; systemd's stop waits for the requests under way. What
  Shipyard can't do yet (`--privileged`, `--init`, `--gpus`,
  another container's namespaces, …) is refused, never ignored.
  Images, volumes, networks and `cp` are served too: below.
  See [the Docker API](docs/user/api.md).
- **Logs and events through Docker's API** (step 11.2b1): `docker logs` (`-f`,
  `--tail`, `--since`, `--until`, `-t`), each stream on its own as Docker frames them, a
  terminal container's output as it is; `docker events` with Docker's JSON, `--since`,
  `--until` and its filters (`type`, `event`, `container`, `image`, `volume`,
  `network`), followed as things happen. A follow ends when its client goes, and when
  the service stops.
- **`docker run` attached, `docker attach` and `docker start -a` through Docker's API**
  (step 11.2b2a): the connection taken over as Docker's clients ask, the output framed
  as Docker frames it (or raw with `-t`), input with `-i`, `-it` with a terminal of its
  own and resized as your window changes, detach keys; an attach made before the
  container starts (as `docker run` makes it) is let in before its start, so its first
  line is never missed.
- **`docker exec` through Docker's API** (step 11.2b2b): `-i`, `-t` (resized as your
  window changes), `-d`, `-u`, `-w`, `-e`, the command's exit code; each exec runs as a
  `shipyard exec` of the service's own, its variables' values never on a command line.
- **`shipyard exec -e KEY`** (a bare key) copies your shell's value, as `run` does and
  as Docker's `exec` does (step 11.2b2b); before, it was refused.
- **`--since` and `--until` take Unix seconds with a fraction** (`1760000000.5`), as
  Docker's clients send them (step 11.2b1).
- **`shipyard doctor` says where the Docker API is** (step 11.2a): whether systemd's
  socket unit is enabled, and the `DOCKER_HOST` to use.

- **`--ipc container:NAME`** (step 11.4b2b2, F34): a container starts in another running
  container's IPC namespace, with its `/dev/shm` shared both ways (Docker's; compose's
  `ipc: "service:NAME"`; the API's `IpcMode`). A mount can't be bound from another mount
  namespace (the kernel's rule, checked by `scripts/probes/foreign-shm.c`), so the setup's
  helper enters the other's, clones its `/dev/shm` there and hands the detached mount back.
  `private` and `shareable` are taken; `host` and `none` are refused; IPC settings are the
  other's; rootless, the network and IPC namespace must be one container's.
- **`--network container:NAME`** (step 11.4b2b1, F34): a container starts in another
  running container's network, as Docker's: its interfaces, ports and hostname, its
  `/etc/hosts` and `/etc/resolv.conf` made as the other's. The CLI, the API
  (`HostConfig.NetworkMode`) and compose's `network_mode: "service:NAME"`. Kept by the
  other's ID (`created.json` v4); refused at each start while the other isn't running
  (Docker's words); `stop`, `restart` and `rm` of the other say who loses their network;
  `system recover` starts the joiners last. Rootless, the other's user namespace is
  joined with it.
- **The runtime joins another container's network and IPC namespaces** (step 11.4b2a,
  F34's first half): a new container can be started in a running one's network and IPC
  namespaces, named by its init's PID and start time (a reused PID is refused), with the rest
  its own; rootless, the other's user namespace is joined too, with its own ID maps, by a
  short-lived process of its own, so the monitor never leaves its own namespaces. `--network
  container:NAME` and `--ipc container:NAME` use it (above).
- **OCI bundles join an `ipc` or `uts` namespace by its `path`** as they already joined a
  `network` one (`linux.namespaces[].path`). Nothing is written into a joined namespace: IPC
  settings with a joined `ipc` and `hostname`/`domainname` with a joined `uts` are refused, as
  are `net.*` settings and network devices for a container that joins another container's
  network. A `pid` path stays refused
  (F134), as do `mount`, `user` and `cgroup` ones. A container with a user namespace of its
  own that joins another's IPC namespace has no `/dev/mqueue` (the kernel refuses the mount
  there); its queues still work through `mq_open`.

### Changed

- **The macOS guest's kernel is `linux-virt` 6.18.58** (Alpine 3.24's, as the rest of
  the guest; `scripts/guest/alpine.lock`'s `KERNEL_BRANCH` can name another branch):
  6.18.57, pinned first, was dropped upstream.

- **A stopped container no longer keeps its networks** (step 11.4a1, D11.4a-2):
  `network rm` and `network prune` refuse only a network a container that's up is
  on, as Docker's do; the stopped container's next start fails, its network gone.
  (Stage 9 kept them, so it could always start again.)

- **A start refuses a container whose `created.json` can't be read** (step
  11.3e2) instead of a warning: it may hold tmpfs mounts, without which the
  container would write to disk what it meant for memory.

- **`image prune` asks first** (step 11.3e1), with Docker's warning, unless
  given `-f`; without a terminal to ask on, it's refused. It now removes
  dangling images too (with `-a`, every image no container was made from), and
  prints Docker's `Deleted Images:` and `Total reclaimed space:` instead of
  `Deleted:` lines and a count.

- **The API's network prune, rootless, prunes nothing** (step 11.3e1) instead of
  refusing (`403`): there are no networks of Shipyard's to prune there, and
  Docker's `system prune` asks for them. Nothing deleted is `null`, as Docker's.
  It takes `until` too (made before), as Docker's.

- **`volume inspect`, `network inspect` and `system df` are Docker's** (step
  11.3c2): `volume inspect` prints Docker's volume as JSON (it printed text lines);
  `network inspect` Docker's network (Shipyard's own view is `network inspect
  --shipyard`); `system df` `TYPE TOTAL ACTIVE SIZE RECLAIMABLE`, volumes as `Local
  Volumes`, sizes in Docker's units. `inspect`'s JSON is indented 4 spaces, as
  Docker's.

- **The lists' output is Docker's** (step 11.3c1): `ps --format json` (and every
  list's) is Docker's row, one object per line, not Shipyard's own array (`inspect
  --shipyard` keeps Shipyard's fields); `images` shows `REPOSITORY TAG IMAGE ID
  CREATED SIZE`, `volume ls` `DRIVER VOLUME NAME`, `network ls` `NETWORK ID NAME
  DRIVER SCOPE` (subnets are `network inspect`'s); `events --format json` writes
  Docker's event message, not the journal's line. A rootless `network ls` lists the
  built-in `host`, `none` and `pasta` instead of failing. `ps` lists newest first.

- **Exit codes are Docker's** (step 11.3a): a failure of any command but `run` exits
  `1` (it was `125`): `rm`, `stop`, `inspect`, `logs`, `exec`, `start -a`, `attach`,
  `create` and the rest, as Docker's CLI. `run`'s own failures and a command line
  that can't be taken still exit `125`.
  `system recover` and `system stop` exit `125` when some containers (not all)
  couldn't be started or stopped, what the boot units count as done, and `1` when
  nothing could be done (the unit then fails). The API's exec start of a container
  that stopped since the exec was made is Docker's `409`.

- **`inspect` prints Docker's shape** (step 11.1): `docker inspect`'s
  (`Id`, `Name`, `State`, `Config`, `HostConfig`, `NetworkSettings`, …), so a script or
  tool written for Docker reads it. Shipyard's own view, with its serial, health
  history and the config as its record keeps it, is `inspect --shipyard`. A container on the
  host's network, or none, lists it under `NetworkSettings.Networks` (`host`, `none`),
  as Docker's does (step 11.2a).
- **A relative store variable is refused** (F61): `SHIPYARD_EVENTS_DIR`,
  `SHIPYARD_IMAGE_STORE` and `SHIPYARD_NETWORK_DIR` must be absolute paths, as
  `SHIPYARD_CONTAINER_DIR` had to be, and so must `XDG_DATA_HOME` (or `HOME`) where
  they place a store. And a release build ignores the DNS server's test
  variables (`SHIPYARD_DNS_UPSTREAM`, `SHIPYARD_DNS_IDLE`).
- **`shipyard-runtime create --hold-fd` refuses a pipe or a device** (F73): only a
  file, a directory or a socket is held, as for a hold added while the container
  runs.
- **Internal: the engine's container code split by job** (stage 10, step 10.1a; no
  behaviour change, checked against 0.2.0's binary by a differential run): a container's
  create, start, foreground run, `cp` view, removal and "find, lock, read again" are
  modules of their own, as are the record store's writers, lookups, locks and what starts
  leave; every helper's entry point is called through one engine function, in its one
  order.
- **Internal: one way to take a container, wait for its end, and say it's up** (step
  10.1b; checked against 0.2.0's binary as 10.1a was): "find, lock, read again by ID",
  the wait for a settled end and the meanings of "up" are each written once. The one
  difference: in a store that holds an unreadable record, `rename`, `network connect`,
  `attach` and `wait` now warn about it once instead of twice, and a record that turns
  unreadable while one of them waits for its lock fails with its read error instead of
  "no container".
- **Internal: the terminal plumbing tidied, and long modules split** (step 10.2f; no
  behaviour change, checked against 0.2.0's binary): an attach session watches only
  SIGWINCH and SIGCONT, through the runtime's own signal watch; the descriptor sweeps
  after `fork` are one; a terminal's size is one type; eight long modules (the
  container store's tests, a run's planning, the image index, `shipyard-runtime`'s
  main, events, the monitor's attach, the firewall's ruleset) are split by job.
- **Internal: one safe way to write and read Shipyard's own files** (step 10.1c1; checked
  against 0.2.0's binary as 10.1a was): a new crate, `shipyard-sys`, writes every state
  file whole or not at all (a temporary file made exclusively, synced, renamed, the
  directory synced) and reads each one without following a symbolic link, without
  waiting on a FIFO, and only up to a bound named beside its version. The runtime's own
  files (under `/run`, which a restart empties) are written whole but not synced, as
  before. A state file larger than its bound is refused instead of
  read cut short: an exit record that large reads as one that can't be made sense of
  (ended, no code); a lease, as no lease; an image index, as a corrupt one (moved aside);
  a container image area's list fails `image prune` and `rmi` instead of reading part
  of it. A store Shipyard wrote reads exactly as before.
- **Internal: one way to wait for a lock, and one rule for a lock handed to a monitor**
  (step 10.1c2; checked against 0.2.0's binary as 10.1a was): every lock waited for to a
  deadline goes through one loop in `shipyard-sys`, and the leases, image areas, volumes
  and run directories a container's monitor keeps share one holder that lets its lock go
  when dropped unless it was handed over. A volume or an image area locked by a command
  that then fails is now let go at once, not kept until another thread's child execs.
- **Internal: one way to signal through a pidfd, pass a descriptor, and take one
  inherited** (step 10.1c3; checked against 0.2.0's binary as 10.1a was): pidfds are
  opened through `rustix`, and every signal, a real-time one or the "still there?" probe,
  goes through one call; descriptors passed over a socket (a terminal, a hold) and those
  a helper is started with go through one function each. The storage helper, the DNS
  server and a container's monitor now refuse a descriptor that isn't open, or one passed
  twice, instead of using it.
- **Internal: a container's record is rewritten at the version it was read at** (step
  10.1d; checked against 0.2.0's binary, and with 0.2.0's binary and this one managing
  the same containers in turn): a newer Shipyard changing a container (`stop`, `rename`,
  `update`, `network connect`) keeps its record readable by the older one still running
  its monitor, unless something only the newer one has was set. Records are written as
  before, byte for byte.
- **Internal: `doctor`'s checks are the engine's** (step 10.1e1; its output checked byte
  for byte against 0.2.0's, as root and rootless): the engine returns the host's checks as
  a report, which `shipyard doctor` prints as before. The one difference: its first lines
  (host, backend, user) now come with the rest, once the checks are done, rather than
  before them.
- **Internal: the engine's API narrowed, and names that clashed renamed** (step 10.1e2;
  checked against 0.2.0's binary): what only the engine uses is no longer public (the
  record writers reach its tests through a hidden `containers::testing`); `report::Event`
  is `Message`, `run::Ran` is `Finished`, and the networks', streams' and options' types
  carry their module's name (`NetworkInspected`, `RunStreams`, `AttachOptions`…), and
  (step 10.1e4b) more of what only the engine uses is its own (its network plumbing
  among it). Nothing a user sees changes.
- **Internal: the command line split by job** (step 10.1e3; its reference pages
  regenerate byte for byte, and checked against 0.2.0's binary): `shipyard`'s flags,
  output helpers, reporter and command groups each in a module of their own, `run`'s
  flags apart from turning them into a request, and what both binaries share in one
  library instead of a file compiled into each.
- **Internal: attaching and reading logs by runtime ID are the runtime's own**
  (step 10.1e4a; checked against 0.2.0's binary): `shipyard-runtime attach` and
  `logs` no longer go through the engine, which keeps attaching and reading logs by
  name. Messages and exit codes are as before.
- **On a Mac, `doctor` exits 1 when the VM runs but its checks can't be had** (a
  guest of another build: step 10.4, F112), as when one fails.

### Fixed

- **The API's streams go at the client's pace** (step 11.7): `docker logs`, `docker cp`
  both ways, pull progress and events waited 50 ms whenever 16 chunks were queued, so
  50,000 log lines took 153 s through the API (0.01 s with `shipyard logs`) and a
  256 MiB copy out took 27 s. They now go on as soon as the client takes a chunk.
- **A Docker client killed in a quiet session on a Mac is hung up** (step 11.7): a
  `docker exec -i` or `docker attach` whose client died while nothing was written
  ran on in the VM, and the Mac's host kept its connection. The host now sees the
  client go and tells the guest, whose service hangs the session up as on Linux.
- **`docker ps` reads each network once** (step 11.7): listing containers read every
  network's leases once per container, so its cost grew with their square (116 ms at
  200 running containers).
- **A refused `update` leaves the container as it was** (step 11.7): a limit newly
  set and then followed by one the kernel refused (no swap accounting) stayed in force
  while `inspect` said none; each limit is now put back as the kernel had it.
- **A volume filled from an image is on the disk before it's marked whole** (step
  11.7), and a fill another container's create left unfinished is given up, never
  copied over, when another container mounts the volume too (one made with `nocopy`).
- **Test pauses do nothing in a release build** (step 11.7): `__cleanup` honoured a
  test's variable in any build, and held a container's locks for a minute.
- **Stage 11's review, smaller fixes** (step 11.6b): `docker volume create` of a
  volume a running container uses says it exists at once (it waited 15 s, then
  failed); `shipyard logs --since` takes Go's durations, as Docker's (`1h30m`,
  `1.5h`); `shipyard network create default` is refused, as the API refuses it;
  `shipyard port --sockets --format json` escapes what a terminal would act on.

- **A volume's unfinished fill is never finished over another container's data**
  (step 11.6b): once another container's create finds the volume in use, the fill a
  killed create left is given up; its container's first start used to copy the
  image's files over what was written there.
- **The API's exec never ends the service when a thread is refused** (step 11.6b):
  a task limit made a thread spawn panic, and the whole service abort; such an exec
  now fails alone. An exec's variables are measured as the child gets them (escaped),
  never past what the kernel takes in one.

- **A detached `docker exec` no longer keeps the API service up** (step 11.6a3, F117):
  `docker exec -d` (a `sleep infinity`, say) held the service from its idle exit for
  good; it now lets it go after 10 s, and the command runs on. An exec's inspect says
  its command's `Pid` (it said the service's helper's), and its detach keys
  (`DetachKeys`, Docker's ctrl-p,ctrl-q by default) end a terminal session and leave
  the exec running (holding the service up until it ends: its terminal is the
  service's).

- **`docker cp` follows a link to a link as Docker does** (step 11.6a2, F121): a
  link's `linkTarget` is its chain followed in the container (at most 255 links), so
  `docker cp -L` of a link to a link copies the file at its end, not the second link.
  A PUT to a container that doesn't exist is `404` whatever its body, as Docker's.

- **A failed `start` no longer sets off a restart policy** (step 11.6a1, F81): an
  `always` container that had ended, whose `start` failed, was restarted by its policy as
  if it had run. Docker's policy follows a run; Shipyard's does now too.
- **A container's health after its end is Docker's** (step 11.6a1, F133): `inspect`'s
  `State.Health`, `ps --filter health=` and the API's `Health` say `unhealthy`, its
  failing streak and latest results kept, as Docker 29.8 does once its checks stop;
  `ps`'s status shows health only while it runs, as Docker's does.
- **The API service raises its own descriptor limit** (step 11.6a1, F114): started by
  systemd with a soft limit of 1024, it could run out under many creates and connections.
  Its containers keep the limit it was started with.
- **Waiting attaches are let in together, and `StdinOnce` is kept with the container**
  (step 11.6a1, F116): attaches waiting for a container's start were welcomed one by one,
  holding a `stop` or `rm` back; and a container made with `StdinOnce` by a service that
  then ended started with its input open. Both fixed. A foreground `shipyard run -i`
  (no terminal) now makes its container `StdinOnce` too, as `docker run -i` does: a
  later `start -ai` ends its input at the first end of file.
- **A container whose process ends before its start is said plainly** (step 11.5b2b,
  F119): its start failed with "container … is stopped; connect needs it created", which
  read as a start gone wrong. It now says "its process ended before it could start
  (killed by SIGKILL …): its command never ran", with how it ended when that's known.
- **The macOS VM's refusals are known by a code** (step 11.5b2b, F99): a host stopping, or
  a guest still starting, said why only in words the Mac's `shipyard` compared; a
  rewording would have stopped it retrying. Older hosts and guests are still understood.
- **Docker's CLI from 29 creates containers on the default network** (step 11.5b1): it
  names the default's endpoint `default`, which the API took for a network of that name
  ("there's no network named default"). It's the default network, as Docker keeps the
  name for it.
- **An address, alias or hardware address without a network of your own is refused**
  (step 11.5b1): `docker run --ip …`, `--network-alias` or `--mac-address` on the default
  network was taken and its setting dropped. Docker's API also refuses a network named
  `default` (`403`), the name Docker keeps for the default network.
- **`-p 0:80` publishes on any free port** (step 11.5b1), as Docker does; it was refused
  ("a port is 1 to 65535"). `shipyard run -p` and the API alike.
- **The macOS VM keeps one network address** (step 11.5a): its network card had a new
  random MAC address at every boot, and each took another of the Mac's NAT leases
  (253, an hour each). Booted often enough (the VM tests, or many idle stops and
  starts), the VM used them all up, and then neither it nor any other VM on the Mac got
  an address: every pull failed with "failed to lookup address information". The
  address is now made at the first boot and kept in the VM's directory
  (`network.json`).
- **A container with a health check is `starting` from its start** (step 11.4b1):
  before its first check's result, `inspect` showed no `State.Health` (and `ps` no
  `(health: starting)`), where Docker shows `starting`; Testcontainers' health wait
  read that as "no health check" and failed.

- **`rm` without `-f` no longer removes a container its restart policy is about
  to start again** (step 11.3e1, the Murphy review): in the moment between its
  end being recorded and its restart being scheduled, it's refused as restarting,
  as Docker's restart manager sees it; a prune keeps it too.

- **`ps` and `inspect` show the command a container runs** (F101, step 11.3b2): its
  image's entrypoint, command, variables, working directory and user where it gave
  none, merged by the start's own rules (it showed only what was given, so an image's
  own command was an empty `COMMAND`). The API's container list and inspect too.

- **`shipyard vm start` returns once its guest is ready** (step 11.3a's verify), as
  its help says: it returned once the VM's host answered, while the guest could still
  be checking its data disk, so `doctor` right after said "the VM is still starting".
  `doctor` now waits up to 30 s for a guest that's starting.

- **A container's end is said once, even if its clean-up is killed after recording
  it** (F88): the `die` event was lost then, a reboot in between too; a start now
  leaves an `end-unsaid` marker beside the record, and whoever settles the end (the
  next command on the container, or the recovery pass after a reboot) says it and
  removes the marker.

- **The runtime library puts the child-subreaper setting back** when a supervisor is
  dropped (F29), so a program that embeds it doesn't stay every orphan's parent.

- **On a Mac, closing the terminal of a `shipyard run -it`** always ends it with 129 as
  a hung-up command does; it could die of a second SIGHUP instead (the shell's), about
  one time in six.

- **`volume rm` and `volume prune` keep every volume when the container store can't
  be found** (a relative `SHIPYARD_CONTAINER_DIR`, no home), saying why: they treated
  it as "no containers" and deleted the volumes of stopped containers (stage 10's
  close; F56's safeguard).
- **On a Mac:**
  - a container given the home (`--volume ~:/h`) no longer reaches the VM's own files
    in it (its data disk, the guest's boot files): the guest hides them;
  - the guest's clock follows the Mac's after a sleep even when no command runs;
  - the VM's data disk is left out of Time Machine's backups;
  - the guest writes its data out before it stops its containers (a shutdown that ends
    the VM early loses less);
  - `shipyard vm stop` no longer reports a failure when another command starts the VM
    again while it waits;
  - a home of `/` is refused.

- **Shipyard's note of the host routing it closed, unreadable, no longer opens it**
  (step 10.2a, F65): if the note can't be read (a symbolic link, too large, not text,
  a disk error) while forwarding is on, the rebuilt firewall closes routing between
  the host's interfaces as they are now, and says so, naming the file; before, it
  rebuilt the table with routing open, so the host routed between its own
  interfaces for anyone.
- **Another user can no longer hold up publishing a port** (step 10.2a, F53): holding a TCP port
  that only closed connections wait on takes a lock in the caller's own runtime
  directory; the abstract socket name it was, any local user could hold for the
  full minute. (Without a runtime directory, it's that name, as before.)
- **`logs` fails when its standard error's reader is gone while its standard output's
  isn't** (step 10.2a, F70): those lines were lost and it exited `0`. A reader of standard output
  that stops early (`logs | head`, `2>&1 | head` too) still ends it quietly, asked of
  the kernel, for `shipyard-runtime logs` too.
- **A failed start no longer forgets that its user stopped the container** (step 10.2b1,
  F55): a start that fails after it updated the container's record (its runtime start,
  say) puts back all it changed, not only its start and end times: "stopped by user"
  (so an `unless-stopped` container its user stopped isn't brought back, by its policy
  or at boot), its restart count and backoff, and its ports.
- **A container whose start never ran shows no finish time** (F64): `inspect` says
  `created` with no start, end or exit, whichever way that start was settled.
- **A failed start lets go of its container at once** (F66): another command no longer
  waits for it to settle while another thread's child still holds a copy of its lock.
- **A volume a container may need is never removed** (F56): `volume rm`
  and `prune` decide under the container store's lock, reading every record: a record
  that can't be read keeps every volume (`prune` removes nothing, `rm` refuses, both
  naming it), and a container made meanwhile is waited for, never missed.
- **A `network connect` or `disconnect` cut short is finished by running it again**
  (F57): it used to refuse ("on the network already", "isn't on the network"), the
  container and its record disagreeing until its next start.
- **A full disk no longer turns restart policies off** (F45): an end the store refuses
  is retried with the policy's backoff, up to a minute apart, until the store takes it
  (or `system stop`, `rm` or a reboot comes first), and the policy then applied.
- **`shipyard doctor` says when the boot unit won't see your containers** (F44):
  containers with a restart policy on stores set by `SHIPYARD_*` variables (or a
  user's `XDG_DATA_HOME`), with the `systemctl edit` line that gives the unit the
  same.
- **`network connect` is no longer refused at the descriptor limit its container was
  made with** (F49): a container's monitor raises its own soft limit, as far as the
  hard one allows, once the container is running. The container, its health checks
  and its exit command keep the caller's limit; the exit command gets room for what
  it's handed where that's more, and a container its restart policy starts again has
  that limit too.
- **Descriptors a monitor received at its limit are closed** (F54): a message cut
  short (`MSG_CTRUNC`) no longer leaves the descriptors the kernel did install open in
  the monitor.
- **`network connect` knows a full monitor by a reason, not its words** (F73): the
  monitor's hold protocol (version 4) says why it refused a hold. Monitors started by
  0.2 or before are still understood.
- **A network's DNS server closes a TCP connection 30 seconds after it was opened**
  (F58): a container sending a query every few seconds on each of its 8 connections
  could keep all 64 for good. A resolver opens another.
- **A paused container stops gracefully** (F46): `stop` and `system stop` thaw it
  after its stop signal, which it used to have only at its timeout's SIGKILL.
  `shipyard doctor` warns when a container's stop timeout is longer than the stop
  unit's, which would kill it first at a shutdown.
- **Keys typed right after a resize no longer reach the container before its new size**
  (F68): `run -it` and `attach` send a changed window size before the typing that
  follows it, and the monitor sets it before writing that typing, where under load the
  container could read the keys first.
- **A made-up name no longer collides on a shared network** (F63): a container's
  generated name is drawn against the names its bridge networks already hold, a
  container of another store's among them, where its start used to be refused.
- **A `cp` racing a container's end copies** (F84): one whose container ended before
  its helper joined it is copied through the stopped container's view, where it said
  only "no container with ID …".
- **pasta's help no longer fills the journal** (F50): what a rootless run's pasta can
  do is asked once per pasta binary and kept in the runtime directory
  (`$XDG_RUNTIME_DIR/shipyard/pasta-features.json`), where every rootless run asked
  `pasta --help`, about 60 lines in the system log each time.
- **Small fixes from the 0.2 review** (F61): a health check's output written just
  after it exits is kept; renaming a running container refuses a lease of its that
  isn't a JSON object instead of panicking; a container's monitor that couldn't join
  its own cgroup is said in the run's warnings, instead of on stderr.
- **A shutdown during boot's start stops the containers** (F52): the stop is a unit of
  its own, `shipyard-stop.service`, started at once; a shutdown while
  `shipyard-recover` was still starting containers skipped its stop, and they ended
  without their stop signal. Install both units (`contrib/systemd/README.md`);
  `shipyard doctor` warns while the stop unit isn't enabled.
- **`logs -f` ends once its reader has gone** (step 10.2f, F78): `logs -f web | head
  -n1` ended only at the container's next line; it now asks, between its looks,
  whether anyone reads its output, and ends quietly once no one does
  (`shipyard-runtime logs --follow --log-dir`, which had no end of its own, too).
- **A start that fails no longer says the container died** (step 10.2f, F80): on a
  container that ran before, a start that failed after its container was made sent
  a `die` event with the last start's exit code (a crash loop of failing restarts
  sent one each); only an end of a start that ran is said now.
- **A container with many published ports keeps its restart policy** (stage 10's
  fresh review): handing well over a thousand holds (`-p` ranges) to its exit command,
  its monitor ran out of descriptor numbers (each hold's copy sits above it while
  they're placed), so the end went unrecorded until the next command and the
  restart never came. The monitor raises its limit for both first, and a run asks
  only for what the hard limit leaves room for.
- **A prefix of an ID that a container with an unreadable record shares is refused
  as naming more than one** (stage 10's fresh review): it found the readable
  container alone, so `rm -f abc` could remove the wrong one.
- **On a Mac, a bare `-e KEY` no longer changes the VM's own `shipyard`** (stage 10's
  fresh review): the value went into its environment, so `-e HTTPS_PROXY` sent the
  VM's pulls through a proxy at the VM's own `localhost`. It now reaches only the
  container, as Docker's CLI does.
- **An end recorded late is never written over a later start's** (step 10.4, F102):
  `__cleanup` retrying an end the store had refused (a full disk) now checks the
  runtime's state is still its own start's (its monitor's PID and start time, which
  the monitor now tells it); before, a second start whose own end was refused too
  could have the first's exit code written over it, and its restart policy decide on
  that.
- **A start that never ran, settled twice, keeps the end put back** (step 10.4,
  F110): a container's last run's end, restored when a start failed, was cleared
  again when a later command settled that start, so it read as never started and a
  boot didn't bring it back.
- **A newer Shipyard's `previous-start.json` is never read as none** (step 10.4,
  F110): after going back to an older Shipyard, a start that never ran is settled
  as never started and the newer file is kept for its Shipyard, said in a warning,
  instead of being deleted unread.
- **A change that landed is no longer reported as failed** (step 10.4, F110): a
  record written in place whose directory couldn't be synced is now a warning
  ("may not survive a power cut"), not an error that made a command report a change
  it had made as failed.
- **Signals where pidfds are refused** (step 10.4, F110): a host whose seccomp
  policy refuses `pidfd_open` (`EPERM`) signals by PID after a check, as a kernel
  without pidfds does; running out of descriptors is now an error, never the
  PID-reuse race the pidfd closes.
- **No false "its monitor stays in this process's cgroup" warning** for a
  container that ended at once (step 10.4, F110).
- **On a Mac, connections to a published port no longer wait for one another**
  while the ports are read (step 10.4, F110): callers waiting for one read share it.
- **On a Mac, an unclean stop no longer ends in deleting every image** (step 10.4,
  F104): a data disk the boot's check (`e2fsck -p`) couldn't fix powered the VM off
  for good, and only deleting `data.img` got out. The VM now repairs it once
  (`e2fsck -y`), says so on its console and in `doctor` (a warning for the next ten
  boots), and what couldn't be placed is in `lost+found` on that disk. Commands are
  refused at once while the disk is checked or repaired ("the VM is still starting"),
  so a long repair is never cut short by a start's time limit.
- **On a Mac, the first command after the VM starts no longer fails with "the VM is
  still starting"** (step 11.1's verification, from 10.4): the VM's agent answers
  before the guest is ready, so a command sent at once was refused. The command now
  waits for the guest (saying so if it takes more than a few seconds, Ctrl-C to stop
  waiting), however long its disk's check or repair takes.
- **On a Mac, the VM's stop is bounded** (step 10.4, F109): a process stuck in the
  kernel (a wedged file-sharing call) could keep the VM's init waiting for it, so its
  disk was never let go cleanly. Its agent, if it keeps failing, is now restarted
  ever later (up to every 5 s), not at once in a loop.

### Known limitations

- **The macOS guest's kernel sometimes crashes with several VMs at once** (F132): with
  four VMs booting on a Mac that's already swapping, about one boot in 40 found its
  memory corrupted a few seconds in, and hung; one VM alone (as Shipyard runs it) didn't
  in 157 boots. The cause isn't found yet (stage 13).

- **A container made with `--tmpfs` and started by Shipyard 0.2.0 runs without its
  tmpfs mounts** (step 11.3e2): 0.2.0 doesn't read `created.json`, where they're
  kept beside the record (F67's rule), so what was meant for memory is written to
  the container's layer on disk. Start it with this version.
- **A container with anonymous volumes keeps them under an older 0.3 build**
  (step 11.4a2a): their list is `created.json` v3, which a build from before it
  refuses (its start refused, as for any newer file); 0.2.0 ignores the file, and
  its `rm -v` takes nothing.
- **`inspect` shows every tmpfs in both `HostConfig.Tmpfs` and `Mounts`, and a
  `--mount` bind or volume in `HostConfig.Binds`**: Shipyard keeps one form of each,
  where Docker keeps the flag it came from.

### Security

- **A bind source Shipyard made is never reached through a symlink** (step 11.7): the
  API makes a missing `Binds` path, and made and bound it again by path at each start,
  so a container sharing a directory above it could put a symlink there and have the
  next start bind (or make a directory in) what it named. What Shipyard made is now
  made and bound by descriptor, and a symlink in its place refuses the start.
- **An image can't multiply its files in a volume** (step 11.7): past a million files
  with more than one name, a fill wrote each further name as a copy of its data; it's
  refused now.
- **The volume store is its user's alone** (step 11.6b): a volume filled from an image
  keeps its files' set-user-ID bits, and the volume store was only as private as the
  directory above it. It's made 0700 now, an older one narrowed.
- **`SECURITY.md` says what Docker's API sockets are** (step 11.6b): root's is
  root-equivalent, a user's is that user; on a Mac, `docker.sock` is root in the VM.

- **Registries' certificates:** the TLS dependencies are updated past
  RUSTSEC-2026-0381, -0382 and -0383 (`webpki-roots`, `webpki-root-certs` 1.0.10,
  `rustls-webpki` 0.103.16), which trusted distrusted emSign intermediates. A pull
  no longer accepts a registry certificate issued through them.

- **`shipyard __guest-init` and `__shipyard-agent` are refused outside Shipyard's
  own macOS VM** (stage 10's fresh review): run as root on a Linux host, the guest's
  init moved the root into memory and removed the rest, or powered the host off.
  They now need the kernel command line Shipyard boots its VM with (and the init,
  PID 1), and exit with 125 otherwise.
- **On a Mac, a path outside your home that the Mac hasn't but the VM has
  (`/var/lib/shipyard`, `/run`) is refused** (stage 10's fresh review): it was sent
  on, so `-v /var/lib/shipyard:/s` gave a container the VM engine's own state. A
  path that isn't there is judged by where it would be.
- **On a Mac, set-user-ID bits and device files in your home mean nothing in the VM**
  (step 10.4, F108): the home is mounted `nosuid,nodev`, and binds of it keep both.
  `SECURITY.md` says what a Mac trusts the VM with, including that a published port
  also answers on the VM's own address on the Mac's private virtual network.

## 0.2.0 - 2026-10-15

### Added

- **Containers by name** (stage 9): `shipyard run -d`, `create`, `start`, `ps`,
  `inspect` and `rm`. A container outlives the command that started it (no daemon:
  a small monitor of its own), and is found by its name, its ID, its start, or its
  ISO 6346 serial (`SHPU 304812 2`). What it writes is kept in a writable layer of
  its own until it's removed: it's there when it starts again. Every start runs
  the exact image it was made from (by digest), kept in the store while the
  container exists. `--rm` removes it once it ends. `ps` never waits for another
  command; `ps --format json` and `inspect` give JSON. `stop`, `kill`, `restart` and
  `wait` end and wait for them: the container's own monitor carries out a `stop` (its
  stop signal, then SIGKILL at its timeout), so it completes even if `stop` itself is
  killed. `rename` gives one a new name, a running one's DNS name on its networks
  too. `logs` (every start's; `--tail` across them; `-f` until it stops; `--since`),
  `attach`, `start -a` (attached before it starts: nothing missed), `exec` (`-u` by
  name, with that user's `HOME` and groups; `-i`; `127`/`126` when the command can't
  run) and `cp` work by name. `cp` reaches a stopped (or created, never started)
  container too: its writable layer and its volumes, mounted for the copy, as
  Docker's. A foreground `run` of an image or `--rootfs` is a container as well,
  Docker's way: a record and a name, attached until it ends (stdout and stderr
  apart, `-i` ending its input at your end of file), the signals `run` gets passed
  on (`--sig-proxy`), kept in `ps -a` until `rm` (`--rm` removes it as it ends, and
  `run` returns after); a killed `run` leaves it running. With `-it` its terminal
  is its monitor's: your terminal raw and its size followed, as before, and Ctrl-P
  Ctrl-Q (or `--detach-keys`) detaches, the container running on for `attach`. See [Containers](docs/user/containers.md).

- **Restart policies** (stage 9): `run`/`create --restart no|on-failure[:N]|always|
  unless-stopped`, Docker's, with no daemon: a container's monitor's clean-up starts
  it again after a backoff (100 ms doubling, at most a minute), and never after you
  stop it. `ps` shows `Restarting`, `inspect` its `restart_count`. Every start (a
  policy's or yours) keeps only the container's latest 10 starts' logs, so a crash loop
  doesn't fill the disk. `shipyard update --restart POLICY` changes a
  container's policy. Not with `--rm`.
- **Containers come back after a reboot** (stage 9): `shipyard system recover` settles
  what a reboot left and starts the containers whose restart policy brings them back
  (`always`; `unless-stopped` unless you stopped it; `on-failure` after a failure).
  `shipyard system stop` stops every container for the host's shutdown, each with its
  own stop signal and timeout, not as your stop. `contrib/systemd` has the units that
  run them at boot and at shutdown (root's, and a user unit for rootless: at boot with
  linger). `doctor` says whether the unit is enabled (or failed), whether linger is on,
  warns when logind's `KillUserProcesses=yes` would end containers made in a login
  session, and warns while a `system stop` still holds restarts off (until `system
  recover`).
- **Containers that ended unwatched are settled** (stage 9): when a container's
  monitor was killed, or the host restarted, the next `create`, `run`, `start`,
  `restart` or `rm` records its end (the boot's time for one of an earlier boot),
  cleans up what its start left, and removes a `--rm` one; each is said. Records are
  version 3 (they read versions 1 and 2): an older Shipyard refuses them.

- **`shipyard events`** (stage 9): what happens to containers (`create`, `start`,
  `die` with its exit code, `oom`, `stop`, `kill`, `restart`, `rename`, `update`,
  `destroy`, `health_status`, `network_lost`), images (`pull`, `untag`, `delete`,
  `prune`), volumes and networks (`create`, `destroy`, `prune`; a network's `connect`
  and `disconnect` of a container too), as Docker prints it or as JSON lines
  (`--format json`). It follows from now; `--since` and `--until` (a time, Unix
  seconds, or `10m`/`2h`/`3d` ago) give the past; `-f type=…`, `event=…`,
  `container=…`, `image=…`, `volume=…`, `network=…` narrow it. Each command and each
  container's clean-up appends to a journal per store (versioned JSON lines, one
  write each, rotated at 1 MiB), so an end nobody watched is there too. See
  [Events](docs/user/events.md).
- **`logs --since`** takes Unix seconds and days (`3d`) too.
- **Health checks** (stage 9): `run`/`create --health-cmd CMD` with
  `--health-interval`, `--health-timeout`, `--health-start-period`,
  `--health-start-interval` and `--health-retries`, Docker's rules: `starting`, then
  `healthy` once a check passes, `unhealthy` after the retries fail in a row (not in
  the start period). `ps` says `Up 3 minutes (healthy)`, `ps --format json` and
  `inspect` give the status (and the latest five checks), and each change is a
  `health_status` event. The checks are run, each in a process of its own, by a
  companion beside the container that its monitor starts and ends with it:
  `shipyard-runtime create --companion PROGRAM [--companion-arg ARG]…`. See
  [Containers](docs/user/containers.md#health-checks).
- **A container's lost network is said** (stage 9): when a container's pasta
  dies while it runs, it runs on with only its loopback (as with Podman), and that's
  no longer silent: `ps` shows `Up 3 minutes (no network)`, `inspect` gives
  `state.network_lost`, the events journal has `network_lost`, and a foreground run
  warns when it ends.
- **`shipyard port --sockets`** (stage 9): what a running container listens on and its
  connections, with the container at the other end named (root's networks), as a
  table or JSON.
- **`network create --isolate-host`** (stage 9): the network's containers can't reach
  the host's own services, nor the clouds' metadata addresses (all of 169.254.0.0/16,
  100.100.100.200, `fd00:ec2::254`, `fd00:ec2::23`, `fd20:ce::254`), but for their DNS
  server (an input chain of Shipyard's own; the firewall's table is rebuilt once, as
  version 7). Not for the default network. A table made by a newer Shipyard is left
  as it is; a downgrade to one without the option rebuilds it, without isolation. A
  network's DNS server also counts TCP connections per container, as it does its
  forwarded queries.
- **`network create --conntrack-limit`** (stage 9): each container address on the
  network may have at most 8,192 connections open of those it opened, counted once
  each on its first packet; the next is refused (TCP with a reset, others with an
  ICMP administratively prohibited), so one container can't fill the host's
  connection table (on a host whose table is larger than that). Counted per address
  by the kernel (`nf_conncount`, in a dynamic set); new connections from IPv6
  link-local sources to the host are refused on such a network. The firewall's
  table is rebuilt once, as version 8.
- **`network connect` and `network disconnect`** (stage 9): a running container
  joins a network, or leaves one, at once (its peers find it by name straight away),
  and a stopped one at its next start; its record keeps the change, so a restart
  joins what it's on now. Its default route, published ports, `/etc/hosts` and
  `resolv.conf` stay as its start made them; the network carrying its way out needs
  `--force`. With `connect` and `disconnect` events.
- **Interface settings for containers** (stage 9): `--sysctl net.ipv4.conf.eth1.*`
  (and `net.ipv6.conf.IF.*`) is set once the container's networks have made that
  interface, instead of being refused.
- **A container's monitor holds more, and lets go, while it runs** (stage 9, runtime):
  `shipyard-runtime create --hold-fd N=LABEL` names each hold, and the exit command
  is told the labels (`SHIPYARD_HOLD_LABELS`); while the container runs, a program can
  give its monitor another descriptor to hold, or have it let one go, by label (attach
  protocol version 3: a holder). What a network connected later needs.
- **`shipyard port`** (stage 9): the host ports a running container publishes, by
  name, as Docker's (`80/tcp -> 0.0.0.0:8080`; `port web 80` for one port's host
  side, exit 1 if none). A port on every address is recorded with its IPv6 side
  (`[::]`) where IPv6 reaches the container (a network with IPv6; pasta on a host
  with IPv6), so `ps` and `inspect` show both. `network inspect`'s keys are now in
  a fixed order (their names are unchanged).
- **Containers answer to their short ID on networks** (stage 9): the 12 characters
  `ps` shows, which is also their default hostname, so `ping $(hostname)` from a
  peer works; a reverse lookup gives the name, else the short ID. (A network's DNS
  server started by an earlier version answers to them once it's restarted: when its
  network is next idle, or `network rm`/`create`.) `network inspect` gives each
  container's ID (`Container`), and `network rm` and `volume rm` name the
  containers that keep them in use ("the network app is in use by web, db").
- **A network's DNS server looks after itself** (stage 9): it runs in a cgroup of its
  own (`shipyard/dns/…`, with memory and task limits), so a logout or a stopped unit no
  longer takes it with the command that started it, and a watchdog starts it again if
  it's killed or fails, waiting longer each time (said in its `dns.log`).
- **An image's own health check** (a Dockerfile's `HEALTHCHECK`) is run by every
  container of it, merged with the `--health-*` flags when the container is made, as
  Docker does: `--health-cmd` replaces its command, the other flags change its settings
  (they no longer need `--health-cmd`), and `--no-healthcheck` turns it off.
- **Passing descriptors** (stage 8): `--preserve-fds N` for `shipyard-runtime create`
  and `exec` (runc's) and `shipyard run` (Podman's) passes descriptors 3 and up on to
  the container's process; socket activation (`LISTEN_FDS`, `LISTEN_PID` naming
  Shipyard's process) passes a service manager's listening sockets on, with
  `LISTEN_PID` set to the process's own PID, as `sd_listen_fds` expects. Only the
  process holds them once it runs.
- **cp** (stage 8): `shipyard-runtime cp ID:SRC DEST` and `cp SRC ID:DEST` copy files
  and directories out of and into a created, running or paused container, with
  Docker's path rules, `-` for a tar stream on stdout or stdin, `-a` to keep owners and
  `-L` to follow a link. The container side is done inside the container by a helper
  of Shipyard's own: a link the container plants (or swaps in while the copy runs)
  never leads to the host's files. Files copied out are yours, without setuid bits;
  device nodes aren't copied.
- **Exec** (stage 8): `shipyard-runtime exec ID COMMAND` runs a command inside a created
  or running container, runc's way: in all its namespaces (rootless too) and its
  cgroup, confined as its own process is (from a snapshot taken when it started, not
  the bundle again), with `--env`, `--cwd`, `--user`, `--cap`, `--no-new-privs` and
  `--process` able to narrow that, never widen it; `-d` to leave it running; `-t` for a
  terminal made in the container (relayed to yours, or sent to `--console-socket`,
  runc's protocol). It exits with the command's code, or 255 if it couldn't run it.
  Nothing in the container can reach it before it runs.
- **`-i`/`--interactive`** passes your input on to the command; `-it` when your
  input isn't a terminal is refused as Docker refuses it.
- **`-t`/`--tty`: a terminal of the container's own** (stage 8): a pseudo-terminal
  made inside the container, sized as your window and resized with it, the
  command's controlling terminal. With `-it` your terminal is raw while it runs,
  so `vi`, `top`, password prompts (`su`, `sudo`) and job control inside work;
  it's put back when the run ends or Shipyard is stopped. `-t` works without
  isolation (and on macOS) too.
- **Attach** (stage 8): `shipyard-runtime attach ID` shows a container's output as it
  comes and passes your typing on (`create --stdin`; `--stdin-once` closes its input
  when the first client to type goes); for a container with a terminal, your terminal
  goes raw and follows its size. Ctrl-P Ctrl-Q (`--detach-keys`) detaches; otherwise `attach`
  ends with the container's exit code. Up to 16 clients; one that falls behind is let
  go, never slowing the container; only the container's owner (or root) may attach.
- **Container logs** (stage 8): `shipyard-runtime create --log-dir DIR` keeps a
  container's output in CRI-format log files (Kubernetes', as containerd and Podman
  write them), rotated (10 MiB × 5 by default; `--log-max-size`, `--log-max-files`),
  versioned by a `log.json` beside them; the monitor reads the output, and the logs
  outlive the container. `shipyard-runtime logs [--follow] [--tail N] [--since TIME]
  [--timestamps] ID` reads them back. The monitor's own memory stays under 1 MiB
  idle (up to 4 MiB more with 16 attached clients that read slowly).
- **OCI terminals** (stage 8): bundles that ask for a terminal
  (`process.terminal`, as every `runc spec` config does) run. `shipyard run
  --bundle` gives them one as `-t` does, sized by `process.consoleSize` when there's
  no window to copy; `shipyard-runtime create --console-socket` sends it to the
  caller's socket as runc does, so containerd's and Podman's shims can use it.

### Changed

- **`rmi` refuses an image a container was made from**, running or not (it named
  only running ones before): `rmi -f` untags it, and the container keeps what it
  needs. `image prune` never takes a container's writable layer.
- **A reader that stops early ends any command quietly, with 0** (`shipyard ps |
  head -1`): it said "Broken pipe" and exited with 125, except `logs` and `events`.
  A command that acts on each thing it's given (`stop a b c | head -1`, `rm`,
  `volume rm`, `rmi`, …) stops there too, the rest left as they are (an earlier
  failure still gives 125).
- **`volume rm` and `volume prune` keep a volume any container uses, running or not**
  (they looked only at running ones, so a stopped container's volume was removed and
  came back empty at its next start): `volume rm` names the containers, and `prune`
  asks "Remove every volume no container uses, and its data?". `system df`'s
  reclaimable figure for volumes counts the same way.
- **Tables (`ps`, `volume ls`, …) and `events` escape control and bidirectional
  characters** in names, commands and labels (`\u{1b}`), so they're printed, never
  acted on by your terminal.
- **`doctor` points rootless users at `journalctl --user`** when their boot unit
  failed.
- **`network rm` and `network prune` keep a network a stopped container is on**
  (they looked only at running ones): it would join it again when it starts.
  `network disconnect` or `rm` the container first.
- **A TCP port that only closed connections still wait on can be published**
  (`TIME_WAIT`, the minute or so after it served its last connection): a container
  started again at once gets its ports back. It was refused as "already in use".

- **`shipyard run IMAGE [COMMAND]…`, as Docker spells it** (stage 9): the first
  argument is the image, and everything after it is the command's, options too
  (`shipyard run alpine:3.20 cat /etc/os-release`). A run without an image says so:
  `--no-image` (the host's files, isolated: what a bare `shipyard run sh` did),
  `--rootfs DIR`, `--bundle DIR` or `--no-isolation`. **A bare `shipyard run CMD` now
  takes CMD as an image**: add `--no-image`. `--image REF` still works, hidden, and goes
  in a later release. `-P`, `--entrypoint` and `--pull` need an image, `--read-only` an
  image or `--rootfs`; without one each is a usage error (exit 125).
- **`attach` from the background waits for `fg`** with a terminal (stopped, as a
  background job changing its terminal is), and then sets it raw. It went on
  without raw mode.
- **`shipyard-runtime attach` exits 255 when it fails itself** (no such container,
  the monitor gone, …), as `exec` does: any other code is the container's. It exited 1,
  which a container's own exit code 1 looked the same as.
- **Other firewalls** (stage 8): a run on a bridge network recognises more of the
  firewalls that would drop its containers' traffic, and says what each would keep
  from them, with the fix in that firewall's own terms:
  - **firewalld**, which rejects by a rule rather than its policy and went
    unnoticed: on Fedora, a network's DNS server gets no questions until its bridge
    is in firewalld's trusted zone;
  - **ufw**, with its own `ufw route allow` rules;
  - a chain that ends in a reject (RHEL's `iptables-services`).

  The fix for Docker's firewall is now complete. Containers reach each other and
  their published ports too, not just the outside, and a network with IPv6 gets
  the `ip6tables` rules. A fix you've applied is recognised and not warned about
  again: the rules have to be the ones printed, so a rule for a former way out
  doesn't count. `shipyard doctor` has a `firewall` line. Shipyard still never
  changes another program's firewall.
- **A container never gets your terminal** (stage 8), from `shipyard run` or
  `shipyard-runtime exec`: when Shipyard's stdin, stdout or stderr is a terminal, the
  command gets a pipe instead, which Shipyard relays (`/dev/null` for `exec -d`), so
  it can't type into your shell or change the terminal's settings, nor keep your
  terminal through a process it leaves behind. (A container `shipyard-runtime create`
  makes from a terminal, without `--log-dir`, keeps it, as with runc.)
  Programs inside see no terminal (`tty` says "not a tty"); `--no-isolation` keeps
  it, and without an image or `--rootfs` the container's `/dev/pts` is its own. A
  `shipyard run -i … &` in the background now waits for `fg` instead of taking your
  shell's keys.
- **Shipyard's binary file never runs where the container could write it**
  (stage 8): on a host that won't make or run a sealed copy (`vm.memfd_noexec` at 2,
  or a security module's policy), the file itself ran in a container's init with a
  warning, and in a rootless `exec` too. It now runs only where nobody in the container
  could write it; otherwise the run, `create` or `exec` is refused. A root container
  without a user namespace is always refused there, and so is a rootless one when
  Shipyard is installed in your home: install it where root owns it. A hard file-size
  limit (`ulimit -f`) smaller than the binary also leaves no copy, and the error says
  so; a soft one (`ulimit -S -f`) is lifted while the copy is made.
- **Input only with `-i`** (stage 8): an isolated command's stdin is empty
  (`/dev/null`) unless `-i` (`--interactive`) is given, as with Docker. Scripts that
  pipe input into `shipyard run` need `-i`.

### Fixed

- **A run that publishes more ports than its monitor can hold is refused before
  anything is made** (stage 9), `-P`'s ports included: a port on every address takes
  two of the monitor's 2,100 holds, beside one per network and volume and a few of its
  own, within your descriptor limit (`ulimit -n`).
- **`system stop` also stops a container that's still starting** (stage 9): one being
  started as the host shut down was passed over, as it read as created.
- **A start killed half way keeps the container's last end** (stage 9): it read as
  `Created`, never run, so its restart policy didn't bring it back at boot.
- **An image's health check with a duration under 1 ms is left out**, with a warning
  (stage 9), as Docker refuses it; the container is made all the same.
- **A bundle's `linux.cgroupsPath` can't name a cgroup of Shipyard's own** (stage 9):
  one below `shipyard/monitors/` or `shipyard/dns/` (a container's monitor's, a
  network's DNS server's) is refused at `create`; the container's `delete` would have
  killed them.
- **A rootless container's pasta runs in its monitor's cgroup** (stage 9), not the
  one of the command that started it, so ending that command's session (a logout)
  doesn't take the container's network with it.
- **A shutdown stops every container gracefully, one made from a login session too**
  (stage 9): each container's monitor runs in a cgroup of Shipyard's own
  (`shipyard/monitors/<id>`), so systemd ending the session (`sudo shipyard run -d`
  over SSH) no longer kills it before `system stop` reaches it. `contrib/systemd`'s
  README now says `systemctl enable --now`: without `--now`, the first shutdown after
  installing didn't run `system stop`.
- **A mount a symbolic link leads into `/proc`, `/sys` or `/dev` is refused** (stage
  9): a bundle's mount listed before `/sys` and steered there by the image (`/s ->
  /sys`, a mount at `/s/x`) was made, then hidden by sysfs (rootless, `create`
  failed); one steered into `/dev`'s tmpfs was made there. Mounts are now checked by
  where they land as well as by its type; one written as `/dev/x` still works.
- **A flood of small attach frames, or of window sizes, no longer burdens the
  monitor** (stage 9): frames are read from an offset (a read of many tiny ones cost
  the square of its length), and the window sizes the monitor reads from a client in
  one pass set the terminal's size once, so a burst of resizes is one SIGWINCH for
  the container, not one each.
- **Killing `shipyard-runtime attach`** (`kill`, a hang-up) left your terminal raw
  when the container has a terminal: it's now put back first, however attach ends
  but `kill -9`.
- **A full disk no longer costs a container's oldest logs**: a line that couldn't be
  written made the logs rotate, dropping the oldest file for an almost empty one. Only
  a file-size limit rotates early now. A write cut short is cut back by the file's own
  length, so a log file someone truncated by hand gets no hole of NUL bytes.
- **`logs` reads only the directory a container was created with**, by identity, not
  path: one put at its path since is refused, `logs --follow` ends when the ID and the
  path are both used again, and a failed `create --pid-file` removes only the log
  directory it made (it removed whatever was at the path, by path). `logs --follow`
  waits for a `container.log` deleted by hand to come back, rather than ending.
- **Mounts are made in the order a bundle lists them** (stage 8, found by the OCI
  conformance suite): Shipyard made `/proc`, `/sys` and `/dev` first, whatever the
  order, as the specification forbids. An order that can't work (a mount below `/dev`
  listed before `/dev`, which would hide it) is now refused, saying what to move.
- **`poststart` hooks run once the program has started** (stage 8): `start` ran them as
  soon as the container was let go, before its program's `execve`.
- **A tmpfs's options on a bind mount** (`mode=`, `size=`) are ignored with a warning,
  as `runc` does, instead of refusing the bundle.
- **`runc spec --rootless` configs run:** their `/sys`, a bind of the host's, was
  refused as a mount into `/sys`; it's now the standard `/sys`.
- **A failed `create --log-dir` removes its logs safely:** it removed the directory by
  its path, which someone able to write in its parent could have pointed elsewhere
  meanwhile (a symlink); it now removes it through descriptors, only if it's still the
  one it made.
- **Logs deleted while a container runs:** a deleted `container.log` is replaced at the
  next rotation (writing went on to the deleted file, out of sight, with no limit), and
  logs whose directory is gone are no longer written. A rotation that fails no longer
  cuts the current file short.
- **`logs --follow ID`** ends when the container is deleted even if a new one takes its
  ID at once.
- **Detaching from a container without a terminal works from a terminal:** the
  terminal's flow control took Ctrl-Q for itself, so Ctrl-P Ctrl-Q (and Enter) never
  detached; it's off while attached, and put back when attach ends.
- **`exec` of a program the container doesn't have** says "command not found": it
  looked for the program among the host's files, and named one the host had
  "found, but its interpreter or loader is missing".
- **A container's monitor runs in `/`**, not in the directory `create` was run from,
  whose mount it kept busy as long as the container ran.
- **Removing an image layer with a directory its owner could read but not search**
  (mode `0600`, say: an image can hold one): it failed, and the layer stayed on disk;
  such a directory is now made searchable first, as an unreadable one already was
  (stage 8, found by fuzzing `cp`).
- **`shipyard-runtime create` as root, for a bundle with a user namespace:** the
  container stopped at its start gate (exit 125) before `start`, as its root couldn't
  open the gate; the gate is now that user's (stage 8).
- **Rootless networking works under SELinux** (Fedora; stage 8): Fedora confines
  pasta, and its policy refused how Shipyard gave pasta the container (by process
  ID), so every rootless run with its default network failed there.
- **`--network pasta` works for root** (stage 8), as the docs said: pasta dropped to
  `nobody` before joining the container's namespace, which `nobody` can't, so root's
  pasta runs always failed. pasta now
  gets the container's network namespace as a file, as with Podman.
- **Shipyard's firewall table can be saved and restored with `nft`** (stage 8): one
  of its sets was named `masquerade`, a keyword of nft's language, so `nft -f` refused
  a listing of the table (`nft list ruleset > saved` then `nft -f saved` failed on
  it). It's now `masquerade4`; the table is rebuilt on the next root run.
- **A failure is never taken for a closed pipe** (stage 9): a broken pipe anywhere
  in an error (the rootless storage helper's, say) made a command exit 0 silently, as
  if its reader had gone. Only stdout's own reader gone (`shipyard ps | head -1`) is
  quiet now; the kernel is asked (`poll`), not the error.
- **Errors, warnings and hints never send what a terminal acts on** (stage 9): a
  registry's error message or an image layer's file name could carry escape sequences
  (a terminal's title, its clipboard) into an error. They're escaped, as tables and
  `events` already were; lines and tabs are kept.
- **A container whose record can't be read can be removed** (stage 9): one damaged,
  or written by a newer Shipyard, kept every `create`, `run` and `rename` from
  working, and `rm` couldn't find it. `rm -f ID`, with its full ID, removes what's
  left of it: what still runs, its writable layer, its image's pin, its directory.
- **A restart that fails is settled before anything else can start or stop the
  container** (stage 9): its count, its logs and its next try were written after the
  container was let go, so a `start` in that moment could lose its logs and be
  restarted again without its backoff.
- **A container's monitor that can't tell who it is ends** (stage 9): it said it had
  failed, then ran the container anyway, beside the `create` undoing it.

### Security

- `rustls` 0.23.46: RUSTSEC-2026-0346 to 0349 (TLS secrets in `Debug` output, an
  unauthenticated downgrade of later connections' key exchange, a decompression's
  memory, data after an ECH rejection). Shipyard reaches registries through it.

### Known limitations

- **No default limits on a container's processes or memory:** as Docker, a container
  can use what the host has unless given `--pids-limit` and `--memory`.
- **The default network reaches the host and the cloud's metadata:** `bridge` can't
  have `--isolate-host`; make a network of your own with it for containers that
  mustn't (see [Networking](docs/user/networking.md)).
- **Restart policies:** a full disk turns them off silently (F45); the boot unit
  doesn't know stores set by environment variables (F44); a shutdown during the boot
  unit's start, or of a rootless container made from SSH without a delegated cgroup,
  ends containers without their stop signal (F51, F52).
- **Upgrades:** after the next change of the container record's format, containers
  that ran across the upgrade can't record their ends until they're restarted (F43).
  There's no such change yet.
- **Not every Docker flag:** no `-v` (use `--volume`), `-h`, `--env-file`, `-l`,
  `logs -n` (use `--tail`) or `inspect -f`; `inspect` and `images` print Shipyard's
  own fields.
- macOS doesn't isolate commands yet (v0.4); rootless containers can't share a network.
- Not a hardened security boundary yet: see [SECURITY.md](SECURITY.md).
- Open findings are tracked in the [roadmap](docs/ROADMAP.md#open-findings).

## 0.1.0 - unreleased

The first milestone: `shipyard run --image alpine sh` rootless on Linux, on your own
terminal, pulled from Docker Hub, isolated, resource-limited and networked. Built in
stages 0 to 7 of the [roadmap](docs/ROADMAP.md), then hardened (stages 7.5 and 7.6).

### Added

- **Running commands** (stage 1): `shipyard run` starts a command with a clean
  environment, a chosen user, working directory and limits; passes signals on, stops it
  gracefully (`--stop-signal`, `--stop-timeout`), cleans up everything it started, and
  reports exactly how it ended (exit codes as Docker's).
- **Isolation** (stages 2 and 3): its own processes (under a small init), mounts,
  hostname and IPC; a root filesystem of its own (`--rootfs`), with `/proc`, `/sys`,
  `/dev`, masked and read-only paths; OCI bundles (`--bundle`); an OCI runtime,
  `shipyard-runtime` (`create`, `start`, `state`, `kill`, `delete`, `pause`, `resume`,
  `update`, `ps`, `events`), as runc.
- **Users and security** (stage 4): rootless containers (user namespaces, with your
  subordinate IDs); a minimal set of capabilities, `no_new_privs`, and a seccomp filter
  (Docker's profile by default, compiled by Shipyard itself).
- **Resource control** (stage 5): cgroup v2 limits (`--memory`, `--cpus`,
  `--pids-limit`, I/O, …), rootless where a subtree is delegated; OOM reporting;
  device access control (eBPF); OCI hooks.
- **Images and storage** (stage 6): `shipyard pull`, `images`, `rmi`, `image prune`,
  `system df`; private registries (Docker's credentials and helpers); resumable
  downloads; safe, verified layer unpacking (fuzzed); a content-addressed store shared
  by concurrent runs; overlayfs, rootless too; named volumes and bind mounts.
- **Networking** (stage 7): a network of the container's own, with a way out and
  published ports (`-p`, `-P`): bridge networks as root, over Shipyard's own netlink and
  nf_tables, and pasta without root; `shipyard network` commands; names and aliases on
  your networks (a DNS server per network); several networks per container; IPv6.
- **`shipyard doctor`** checks whether the host can run containers by trying what a
  run needs (a user namespace, a mount, an overlay next to the image store, a cgroup),
  and says what to fix; exit code 1 if something fails.
- **An AppArmor profile** (`contrib/apparmor/shipyard`) for Ubuntu 23.10 and later,
  which restrict unprivileged user namespaces.

### Security

Stages 7.5 and 7.6 fixed what a review of the whole codebase found
([notes](docs/stages/07.5-hardening.md)). Among them:

- the init runs from a sealed in-memory copy of the binary (CVE-2019-5736's class);
  every entry point refuses a setuid or file-capability start;
- a container can't send as another (a bridge anti-spoofing table), nor poison a
  shared image layer for other images;
- registry credentials never follow a redirect to another host;
- a symlink in an image can't steer a mount onto a kernel interface (CVE-2019-19921's
  class).

### Known limitations

- No terminal of the container's own (`-it`), no detached containers, `exec` or logs
  (v0.2).
- macOS doesn't isolate commands yet (v0.4).
- Rootless containers can't share a network.
- Not a hardened security boundary yet: see [SECURITY.md](SECURITY.md).
- Open findings are tracked in the [roadmap](docs/ROADMAP.md#open-findings).

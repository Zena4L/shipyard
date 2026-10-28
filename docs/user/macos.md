# Shipyard on a Mac

On an Apple Silicon Mac (macOS 13 or later), Shipyard runs your containers in a
small Linux VM of its own, booted with Apple's Virtualization.framework. You use
`shipyard` as on Linux: each command runs in the VM, and you see its output, its
exit code and its errors as if it ran on your Mac.

```sh
shipyard run --rm alpine echo hello      # the first command starts the VM
shipyard run --volume "$PWD":/app -w /app alpine ls
shipyard ps
shipyard vm status                       # whether the VM runs, and which guest
shipyard vm stop                         # its containers stopped, then the VM
```

## What runs where

- **In the VM:** every command but those below. The VM starts on the first one (in
  about a second, once its guest is built), and stops by itself when it's idle (below),
  or at `shipyard vm stop`.
- **On the Mac:** `shipyard vm …` (the VM itself), `shipyard doctor` (this Mac's
  checks), `shipyard login` and `logout` (your sign-ins are the Mac's: its keychain,
  through Docker's credential helper), `--help` and `--version`.
- **Signing in, in the VM:** `shipyard push` and `shipyard pull` sign in with the Mac's
  credential for the image's registry, looked up on the Mac and handed to that one
  command (never stored in the VM, never in any process's environment there). The Mac's
  credential helper gets 15 seconds to answer for it (a keychain that asks to be
  unlocked included); one that fails or doesn't answer is warned of, and the command goes
  on without a credential (a public image needs none). A `run`
  or `create` that pulls an image itself pulls anonymously: `shipyard pull` a private
  image first.
- **Not on a Mac:** `system recover` and `system stop` (a Linux host's boot and
  shutdown units run them; the VM's own start and stop do the same for its
  containers).

## When the VM stops by itself

Nothing of Shipyard runs while you don't use it: the VM stops by itself after **5 minutes
idle**, its containers stopped in order first, and the next command starts it again (in
about a second). It's idle when:

- no `shipyard` command is running (an `attach`, a `logs -f`, a `run -it` count as
  running), and
- no container is up: running, paused, stopping, or waiting to restart. **A detached
  container (`run -d`) keeps the VM up** for as long as it runs; `shipyard rm -f` it,
  or `shipyard vm stop`, and the VM stops.

`shipyard vm stop` asks the VM's host on a socket of its own, and waits for it by the
host's own word: a stop sent while the VM starts succeeds, the VM stopped once it's up
(a check of its disk after an unclean end is never cut short: that may take minutes),
and the start, or a command that was waiting for it, says "the VM is stopping" rather
than starting it again;
and a first command that waits for a slow start waits as long as the host says it's
still within its bounds. Its log (`host.log`) is kept short by the host itself (moved
to `host.log.1` past 1 MiB). A VM started by a `shipyard` from before this version has
no such socket: it's stopped by its PID, as before.

`SHIPYARD_VM_IDLE` sets the wait in seconds (`SHIPYARD_VM_IDLE=60`), read when the VM
starts. A command that comes just as the VM stops waits for the stop and starts it again;
it runs once. A Ctrl-C while a command waits for the VM ends it before it's sent. A VM
whose Linux stops answering isn't stopped as idle (what it runs isn't known): the host's
log (`host.log`) says so, and `shipyard vm stop` stops it.

## The VM's size, its clock and its disk

**Size.** The VM gets a quarter of your Mac's memory (at least 4 GiB, at most 16 GiB)
and half its cores (at most 8). `shipyard vm set` changes either, kept for every start
after:

```sh
shipyard vm set --memory 8g --cpus 6     # taken at the VM's next start
shipyard vm set --memory default         # back to this Mac's default
shipyard vm status                       # the size a start takes, and from where
```

Memory takes `8g`, `6144m` and the like (Docker's `--memory` spellings); a VM may have from
2 GiB to your Mac's memory less 2 GiB, and from 1 CPU to your Mac's cores. A running VM
keeps its size until it stops (`shipyard vm stop`); `vm status` says what the next start
takes, the host's log (`host.log`) what this one took. The size is kept in `vm.json` in the
VM's folder; one that can't be read is said in that log and the default used (`vm set`
writes a new one). **The memory the VM has used isn't
given back to macOS while it runs:** Virtualization.framework's memory balloon was
measured and gave nothing back, so the VM holds what its containers have touched until it
stops (an idle stop included). Give it less with `vm set` if your Mac needs the room.

**Clock.** A Mac's sleep pauses the VM, and its Linux clock stops with it. The VM's host
gives it the Mac's time with each look at its ports (every 2 s while a container runs,
every 10 s while none does), so it's right again within seconds of a wake (a registry's TLS, `make` and anything that compares times need it).

**Disk.** The VM's disk (`data.img`) is a sparse file on your Mac: it takes only what the
VM writes, up to 64 GiB. What the VM frees (a pruned image, a removed container) goes back
to your Mac when the VM stops, the idle stop included. `shipyard doctor` says what the disk
takes and how much free space your Mac has: a warning under 10 GiB free, a failure under
2 GiB (a disk that can't grow makes the VM's files read-only).

**A VM that hangs or reboots by itself.** The host asks the VM's Linux every 10 seconds
which boot it is. One that rebooted in place (its kernel panicked: it reboots rather than
hanging) is stopped in order once its disk's check is done; one that hasn't answered six
asks in a row (a minute or more) is asked to stop, and stopped by force if it hasn't
within a minute and a half (or, while it checks its disk, as long as a repair may take). Either is said in the host's log
(`host.log`), and the next command starts a fresh one. The console log (`console.log`)
is moved aside to `console.log.1` whenever it grows past 4 MiB.

## amd64 images (Rosetta)

On an Apple Silicon Mac, the VM runs `linux/amd64` images too, through Apple's Rosetta
for Linux, when your Mac has it (`softwareupdate --install-rosetta --agree-to-license`
installs it; `shipyard vm stop` then, and the next start shares it with the VM).
`shipyard doctor` says whether it's there (`rosetta`), and the VM's own checks whether
the VM registered it.

- An image with no arm64 build (many older or x86-only images) is pulled as amd64, with
  a warning: it runs, slower than an arm64 one (each program's start is translated: a
  few milliseconds more).
- `--platform linux/amd64` on `pull`, `run`, `create` and `build` asks for amd64 where
  an arm64 build exists (Docker's flag; `docker` and `docker compose`'s `platform:` send
  it through the API).
- Without Rosetta, an amd64 image is refused, saying how to have it. Nothing that runs
  nothing needs it: `save` still writes the image out, and `cp` still copies from a
  stopped container made from it.
- amd64 programs run in the same sandbox as any container's (its namespaces, seccomp
  profile and capabilities): Rosetta translates them in the container itself.

## Checking your Mac

`shipyard doctor` checks what the VM needs and how it stands, one line each:

```text
host:      macos / aarch64
backend:   linux-vm (Virtualization.framework)
isolation: in Shipyard's Linux VM: containers are isolated there as on Linux

ok    macos            macOS 15.1, Apple Silicon
ok    entitlement      signed to run a VM
ok    guest            Alpine 3.24.2, kernel 6.18.57-0-virt, shipyard 1a2b3c4d5e6f
ok    vm               stopped: it starts on first use, and it stops after 5 min with nothing running
ok    ports            published ports are reached on this Mac's loopback only
ok    shares           /Users/you, /Volumes, /private/tmp, /private/var/folders shared with the VM, read-write
ok    files            /Users/you/Library/Application Support/Shipyard/vm
ok    disk             takes 3.2 GiB; 212.4 GiB free on this Mac
ok    rosetta          installed: amd64 images run in the VM through it (slower than arm64 ones)
```

- **`macos`:** macOS 13 or later on Apple Silicon; anything else fails, and the VM
  doesn't start there (said by name).
- **`entitlement`:** `shipyard` must be signed to run a VM (`scripts/macos/sign.sh`);
  the line says by whom. Signed ad hoc, macOS asks again for your guarded folders after
  each rebuild; signed with an identity (`scripts/macos/make-signing-identity.sh`, once),
  it asks once.
- **`guest`:** the VM's Linux, built by `scripts/guest/build.sh`; another build's than
  this `shipyard`'s is a warning.
- **`vm`:** a running VM that booted another build's Linux than the one there now (it
  was started before an upgrade, and kept up by its containers) is a warning, with what
  to do: `shipyard vm stop` (it starts again as this build's), and with
  `shipyard vm install`, `shipyard vm install` again from this `shipyard`. `vm status`
  says the same, and so does each command while it runs; a request that VM can't read
  is refused with those words ("the running VM is another build's …"). So is a VM that
  shares another home, or booted the guest from another directory, than this
  `shipyard` would (a command run with another `HOME` or `SHIPYARD_GUEST_DIR`).
- **`ports`:** while the VM runs, a published port that can't be forwarded (another
  program has it) is a warning, with why.
- **`launchd`:** an agent an older `shipyard` installed (`shipyard vm install`) is a
  warning: launchd would cut its VM's stop short at a logout. `shipyard vm install`
  again.
- **`data disk`** (among the VM's own checks): after an unclean stop (a power cut, a
  Mac that ended the VM before it had stopped), the VM checks its disk at boot, and
  repairs it if the check leaves errors. While it does, every command is refused at
  once with "the VM is still starting", so try again shortly; a large disk can take
  minutes. A repair is then said, as a warning, for the next ten boots, with how many
  entries it left in `lost+found` on that disk: files of your images, containers or
  named volumes may be among them, and nothing reaches that folder from your Mac yet.
  If even the repair fails, the VM powers off at each start, saying why in its console
  log (`~/Library/Application Support/Shipyard/vm/console.log`); the way out today is
  to move its disk (`data.img` there) aside, which loses every image, container and
  volume in it.

With the VM running, the VM's own Linux is checked too, as on a Linux host, and listed
after (`guest:     Linux 6.18…`, then cgroups, images on overlayfs, the network with
nf_tables, the runtime's state). A stopped VM isn't started to be checked: `doctor` says
so.

Its exit code is 1 if a check failed (the Mac's or the VM's), or if the VM runs but its
checks can't be had (a guest of another build), so a script can ask.

## Your files

The VM shares the places Docker Desktop shares by default, each at the same path:

- your home;
- `/Volumes`: external drives and disk images, those you attach later too;
- `/tmp` (`/private/tmp`) and `/var/folders` (`/private/var/folders`, where each user's
  `$TMPDIR` is).

A command run from a directory in one of them runs in that directory in the VM, and
`--volume "$PWD":/app` binds it. A path outside them, or one a symbolic link takes out
of them, is refused before anything runs: it would mean something else in the VM. So
is one that isn't on your Mac but would be outside them (`/var/lib/shipyard`, `/run`:
the VM's own files); one in a share that isn't there is refused by the VM, as on Linux.

The VM refuses the same binds when they come from Docker's tools
([Docker's CLI on your Mac](#dockers-cli-on-your-mac)), as Docker Desktop does:
`mounts denied: the path /opt/data isn't shared from the Mac`. It refuses a bind of
the VM's own files (its directory, its disk), however they're spelled, and a socket
(`$SSH_AUTH_SOCK`): the VM can't reach a socket on the Mac.

The VM can read and write all of these, as Docker Desktop's file sharing can: what's
in `/tmp` and `/Volumes` is any of your Mac's users' as much as yours, and a
container you bind it into sees it. A container sees only what you bind into it.
Set-user-ID bits and device files mean nothing in the VM, nor in a container you bind
them into. The VM's own files are hidden in every share by a mount over them, which a
container given `--cap-add SYS_ADMIN` (or `--privileged`) can undo, or it can mount a
share whole, binding nothing (as on Docker Desktop); a `--device` of the VM's disk
reaches every image and volume: give those only to containers you trust with the whole
VM and every share.

**The folders macOS guards** (Desktop, Documents, Downloads, iCloud Drive) are read by
the VM only with your permission. A VM started from your terminal has the terminal's;
one the desktop app started has the app's (Shipyard, which macOS asks about once it's
signed with your local identity): grant the one that started the VM that's running. One
that launchd started (`shipyard vm install`) asks for its own the first time it reads
one. Until you allow it, a command run from one of those folders, or a bind of
one, is refused and the refusal names the folder:
`macOS hasn't let the VM read /Users/you/Desktop: allow shipyard in System Settings →
Privacy & Security → Files and Folders`. External and network drives under `/Volumes`
are guarded the same way (Removable Volumes, Network Volumes). `shipyard doctor` tries each one and says
which the VM can't read; trying makes macOS ask if it hasn't yet.

**Building:** `shipyard build`'s context (and `-o type=local`'s directory) works from your
home. From `/tmp`, `$TMPDIR` (`/var/folders`) or `/Volumes` it's refused, saying the
builder can't enter it: files there may be other Mac users', and the VM can't tell from
their modes which the builder could read (an unreadable one would be copied empty, without
a word). Build from a directory in your home.

**File speed:** reading, listing and writing large files through a share is about as
fast as OrbStack's. Making and removing many small files is two to four times slower
(an `npm install` or a `cargo build` writing into a bind). For those, a named volume
(`--volume deps:/app/node_modules`) lives on the VM's own disk and is as fast as Linux.

## Your terminal, your input, your signals, your variables

- **Terminal:** when `shipyard`'s input and output are both your terminal, the command
  runs on a terminal in the VM too, so it behaves as it would on Linux: `run -it`
  gives you the container's shell, raw (each key goes to it, Ctrl-C included), at your
  window's size and following it; full-screen programs work; a prune asks before it
  acts. On a terminal, the command's errors come on that terminal's one output, as a
  program's do on a terminal: redirect `shipyard`'s output (`> file`, `| less`) to have
  them apart. Closing the window hangs the command up, as it would on Linux.
- **Input:** a command reads your input only when it asks for it (`run -i`,
  `exec -i`, `attach`, `cp -`, a prune's question): otherwise your terminal's typing
  and a script's next lines stay yours.
- **Signals:** a Ctrl-C, or a `kill` of `shipyard`, is passed on to the command in
  the VM (and from there to the container, as a foreground `run` passes them), and so
  are a hang-up and your window's new size. They go to the VM on a connection of their
  own, so they reach a command that doesn't read its input (`yes | shipyard run -i …
  sleep 1000`), or one whose terminal is full of a paste it hasn't read. One exception:
  on a terminal you type into (`run -it`), Ctrl-C is a key, sent with your typing, and
  waits behind a paste the command hasn't read; a `kill -INT` of `shipyard` doesn't.
- **Variables:** a command gets your terminal's and your language's (`TERM`, `COLORTERM`,
  `LANG`, `LC_ALL`, `LC_CTYPE`, `NO_COLOR`), and those you name with a bare `-e KEY` (those reach the
  container only, never the VM's own `shipyard`: `-e HTTPS_PROXY` is the container's
  proxy, not the VM's). Nothing else of your environment reaches the VM.

## Ports

A port a container publishes is reachable on your Mac, at the same number, as soon as the
container runs:

```sh
shipyard run -d -p 8080:80 nginx:alpine     # http://localhost:8080
```

- **On your Mac's loopback only:** `-p 8080:80` listens on `127.0.0.1:8080` and
  `[::1]:8080`, so `localhost` reaches it from both; `-p 127.0.0.1:8080:80` on
  `127.0.0.1` only. Nothing on your Mac's network (your Wi-Fi's other devices) can
  reach a container. Other users of your Mac can, as they can any program's port on
  `localhost`. The VM's own address on the Mac's private virtual network
  (`192.168.64.x`, which other VMs on your Mac reach) doesn't answer a published port:
  the VM's firewall refuses it there.
- **A port another program on your Mac already has** (Postgres on 5432, say) is left to
  it: `shipyard vm status` says `not forwarded: 127.0.0.1:5432 (in use on this Mac)`, and
  it's forwarded as soon as that program lets it go. `localhost:5432` keeps reaching
  your own program, never the container.
- **A program you start on your Mac after the port is forwarded**, listening on every
  address, starts without complaint, but `localhost` on that port still reaches the
  container: `shipyard vm status` and `shipyard doctor` say so
  (`shadowed: 8080: another program on this Mac listens on every address of it…`).
  Stop that program, or publish the container on another port. To find
  such a program, `vm status` binds every address of each forwarded port for an
  instant; one starting on that port in that instant may be refused.
- **`shipyard vm status`** lists the ports forwarded, and those that aren't, with why.
- **UDP too:** `-p 5353:53/udp` listens on `127.0.0.1:5353` and `[::1]:5353` for
  datagrams, and the container's answers go back to whoever sent them (each program on
  your Mac is told apart by its address and port; one silent for a minute is
  forgotten, as a NAT forgets it). When more arrives, either way, than the link to the
  VM can carry at that moment, the newest datagrams are dropped, as a full network
  drops them; nothing waits. `vm status` lists it as `127.0.0.1:5353/udp`. On a
  network made with `--isolate-host`, a container that speaks first, after half a
  minute of quiet, isn't heard (the VM's firewall lets only answers through there).
  The container sees one address and port for each program on your Mac; after the VM
  restarts or the forwarding is set up again, it's a new port, so a server that keeps a
  session by its client's address (WireGuard, QUIC) starts a new one.
- **Each connection and datagram goes to the container's own address**, not through the
  VM's port rules: a container restarted with a new address is reached at it within
  two seconds.

## Docker's CLI on your Mac

While the VM runs, it serves Docker's API on a socket in its directory, so Docker's own
tools (`docker`, `docker compose`, Testcontainers) use Shipyard as they would a Docker
daemon:

```sh
export DOCKER_HOST="unix://$HOME/Library/Application Support/Shipyard/vm/docker.sock"
docker run --rm alpine echo hi
docker compose up -d
```

`shipyard compose` does the same for you: it starts the VM and runs Docker's own
`docker compose` against its socket (Docker's CLI and its compose plugin installed).

Or, to switch between Shipyard and another engine, a Docker context of your own:

```sh
docker context create shipyard \
  --docker "host=unix://$HOME/Library/Application Support/Shipyard/vm/docker.sock"
docker --context shipyard ps
```

- **What's served is Shipyard's API** ([the Docker API](api.md)): the same subset as on
  a Linux host, run by the guest.
- **Any time, with `shipyard vm install`** (once): launchd, macOS's own service
  manager, then holds the socket while the VM is stopped and starts the VM at the first
  connection, as systemd's socket does on a Linux host. Nothing runs until then (launchd
  runs Shipyard once as it loads the agent, at install and at each login, only to make
  the socket). `shipyard vm uninstall` undoes it; run `vm install` again after moving
  `shipyard` (`shipyard doctor` says when the agent runs another binary). launchd starts
  it at most every 10 seconds, so a `docker` right after a stop may wait that long.
  `shipyard` must live outside the folders macOS guards (Desktop, Documents, Downloads,
  iCloud Drive), as in `/usr/local/bin`: launchd's programs read there only with your
  permission, so `vm install` refuses a `shipyard` (or a guest) there. In a VM launchd
  started, a command run from one of those folders, or a container that binds one, makes
  macOS ask you whether `shipyard` may read it (once, if `shipyard` is signed with an
  identity: `scripts/macos/make-signing-identity.sh`; signed ad hoc, again after each
  rebuild); until you allow it, the VM can't see that folder.
  A VM started from your terminal has the terminal's permission.
  Without it, the socket is there while the VM runs: start it with any `shipyard`
  command, or `shipyard vm start`; a `docker` that connects just as the VM stops by
  itself is told so (a 503) and doesn't try again, where launchd's socket would have
  kept it waiting for the next start. `vm status` and `doctor` say when the agent is
  installed but launchd hasn't loaded it (taken out by hand, or turned off in System
  Settings › General › Login Items): nothing serves the socket then, and `vm install`
  loads it again.
- **At login, with `shipyard vm install --at-login`:** the VM starts as you log in, but
  only when containers with a restart policy would come back (`always`, `unless-stopped`
  unless you stopped it, `on-failure` as it decides), as they would on a Linux host after
  a reboot; they're brought back in it, and the VM stops by itself once none runs.
  `vm status` says how many would come back, as the VM last said (when it stopped). One
  whose last start failed before it ran isn't counted. An `always` container you stopped
  still counts, as Docker brings it back after a reboot too: remove it (or give it
  `--restart unless-stopped`) to let the VM stay stopped. `vm install` without
  `--at-login` turns it off.
- **The socket is yours alone** (only your user can open it). A connection
  open (`docker events`, an attached container) keeps the VM up, as a command does, and
  so does a tool that keeps asking (an editor's Docker view polling `docker ps`). One
  the service has closed and the client keeps open is let go after a minute. While the
  VM is stopping, Docker's CLI is told so (try again once it has stopped); a large
  upload refused so may show only "broken pipe".
- **Paths:** a bind of a directory in a share (your home, `/Volumes`, `/tmp`,
  `/var/folders`) works as it is: the VM has it at the same path. One outside them is
  refused, as Docker Desktop refuses it ([Your files](#your-files)), and so is a bind of
  Docker's own socket: Testcontainers' Ryuk, which binds it, needs
  `TESTCONTAINERS_RYUK_DISABLED=true` on a Mac for now.
- **Ports** a container publishes reach your Mac's `localhost` as with `shipyard run -p`
  ([Ports](#ports)).

## Upgrading from 0.4

The VM's guest is built apart from `shipyard`, and launchd's agent keeps what the
`shipyard` that installed it wrote. After building this version:

1. `shipyard vm stop`: the running VM is the older build's (`vm status` and `doctor` say
   so).
2. `scripts/guest/build.sh`: the guest of this build. If the VM directory has a `guest`
   of its own (`~/Library/Application Support/Shipyard/vm/guest`), it's taken before the
   one beside `shipyard`: remove it, or build into it.
3. `shipyard vm install` again if you'd installed it: the older agent's stop is cut short
   by launchd at a logout (`doctor` warns of it).

A command that names a place shared beyond your home (`/tmp`, `/Volumes`) is refused by a
guest from before this version, as another build's, rather than bound to an empty
directory of the guest's own. And once this version's VM has run, an older `shipyard`
refuses the VM directory's `network.json`: remove it to go back.

## Without the VM

`SHIPYARD_VM=off` runs a command on the Mac itself, as Shipyard did before its VM: as an
ordinary process, without isolation, and without images (`shipyard run --no-image
COMMAND`). It's for Shipyard's own development and tests.

## Limits (this version)

- **Without `shipyard vm install`, Docker's socket is there only while the VM runs:**
  after an idle stop, `docker` can't connect until a `shipyard` command starts the VM
  again.

- **After the Mac restarts,** containers with a restart policy come back when you next
  run `shipyard` (any command starts the VM, and the VM's start brings them back), or at
  login with `shipyard vm install --at-login`.

- **Ports:** a port a second container publishes on the same number but another of the
  VM's addresses isn't forwarded (the first keeps it; the host's log, `host.log` beside
  `console.log`, says so).
- **The guest is built from the source tree** (`scripts/guest/build.sh`, into the
  tree's `target/guest`, which a `shipyard` built there finds by itself; a guest in the
  VM's directory, `~/Library/Application Support/Shipyard/vm/guest`, comes first, and
  `SHIPYARD_GUEST_DIR` before both), and `shipyard` must be signed with the
  virtualization entitlement (`scripts/macos/sign.sh`): see the developer notes
  (`docs/DEVELOPMENT.md`).
- **Detach keys:** Ctrl-P Ctrl-Q goes to the command in the VM: a `run -it` detaches
  there, as on Linux, and its container runs on. `shipyard` itself has no detach keys:
  a third SIGINT (a Ctrl-C, when you aren't typing into the command: then
  it's the command's key), or a SIGTERM the command doesn't end within 10 s, makes it
  let go, and the command in the VM is then hung up on.
- **File watching:** a change on the Mac isn't seen as it happens by a program in a
  container that waits for it (a development server's reload): it has to poll.

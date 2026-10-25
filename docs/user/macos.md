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

`SHIPYARD_VM_IDLE` sets the wait in seconds (`SHIPYARD_VM_IDLE=60`), read when the VM
starts. A command that comes just as the VM stops waits for the stop and starts it again;
it runs once. A Ctrl-C while a command waits for the VM ends it before it's sent. A VM
whose Linux stops answering isn't stopped as idle (what it runs isn't known): the host's
log (`host.log`) says so, and `shipyard vm stop` stops it.

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
ok    home             /Users/you is shared with the VM, read-write
ok    files            /Users/you/Library/Application Support/Shipyard/vm
```

- **`macos`:** macOS 13 or later on Apple Silicon; anything else fails.
- **`entitlement`:** `shipyard` must be signed to run a VM (`scripts/macos/sign.sh`).
- **`guest`:** the VM's Linux, built by `scripts/guest/build.sh`; another build's than
  this `shipyard`'s is a warning.
- **`ports`:** while the VM runs, a published port that can't be forwarded (another
  program has it) is a warning, with why.
- **`data disk`** (among the VM's own checks): after an unclean stop (a power cut, a
  Mac that ended the VM before it had stopped), the VM checks its disk at boot, and
  repairs it if the check leaves errors. While it does, every command is refused at
  once with "the VM is still starting", so try again shortly; a large disk can take
  minutes. A repair is then said, as a warning, for the next ten boots. What it
  couldn't place is in `lost+found` on that disk: files of your images, containers or
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

The VM shares your home, at the same path: a command run from a directory under
your home runs in that directory in the VM, and `--volume "$PWD":/app` binds it. A path
outside your home (`/tmp`, `/Volumes/…`), or one a symbolic link takes out of it, is
refused before anything runs: it would mean something else in the VM. So is one that
isn't on your Mac but would be outside your home (`/var/lib/shipyard`, `/run`: the
VM's own files); one in your home that isn't there is refused by the VM, as on Linux.

The VM can read and write your whole home, as Docker Desktop's file sharing can. A
container sees only what you bind into it. Set-user-ID bits and device files in your
home mean nothing in the VM, nor in a container you bind them into.

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
  the VM (and from there to the container, as a foreground `run` passes them).
- **Variables:** a command gets your terminal's and your language's (`TERM`, `LANG`,
  `LC_*`, `NO_COLOR`), and those you name with a bare `-e KEY` (those reach the
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
  `localhost`. A port published on every address (`-p 8080:80`) also answers on the
  VM's own address on the Mac's private virtual network (`192.168.64.x`), which only
  your Mac and other VMs on it (another Virtualization.framework, Lima or UTM VM using
  its shared network) can reach.
- **A port another program on your Mac already has** (Postgres on 5432, say) is left to
  it: `shipyard vm status` says `not forwarded: 127.0.0.1:5432 (in use on this Mac)`, and
  it's forwarded as soon as that program lets it go. `localhost:5432` keeps reaching
  your own program, never the container.
- **A program you start on your Mac after the port is forwarded**, listening on every
  address, starts without complaint, but `localhost` on that port still reaches the
  container: stop the container (or publish it on another port) to reach your program.
- **`shipyard vm status`** lists the ports forwarded, and those that aren't, with why.
- **TCP only:** a UDP port (`-p 53:53/udp`) isn't forwarded yet; `vm status` says how
  many.

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
  macOS ask you whether `shipyard` may read it (once for a release; a build of your own
  is asked again after each rebuild); until you allow it, the VM can't see that folder.
  A VM started from your terminal has the terminal's permission.
  Without it, the socket is there while the VM runs: start it with any `shipyard`
  command, or `shipyard vm start`.
- **The socket is yours alone** (only your user can open it). A connection
  open (`docker events`, an attached container) keeps the VM up, as a command does, and
  so does a tool that keeps asking (an editor's Docker view polling `docker ps`). One
  the service has closed and the client keeps open is let go after a minute. While the
  VM is stopping, Docker's CLI is told so (try again once it has stopped); a large
  upload refused so may show only "broken pipe".
- **Paths:** a bind of a directory under your home works as it is (the VM shares your
  home at the same path). A path outside it is the VM's own, not your Mac's, and
  isn't refused yet: `docker run -v /tmp/proj:/w` gets an empty directory made in the
  VM, and what's written there stays in the VM. Keep projects under your home (the
  `shipyard` CLI refuses such paths; Docker's socket doesn't yet: F143).
- **Ports** a container publishes reach your Mac's `localhost` as with `shipyard run -p`
  ([Ports](#ports)).

## Without the VM

`SHIPYARD_VM=off` runs a command on the Mac itself, as Shipyard did before its VM: as an
ordinary process, without isolation, and without images (`shipyard run --no-image
COMMAND`). It's for Shipyard's own development and tests.

## Limits (this version)

- **Without `shipyard vm install`, Docker's socket is there only while the VM runs:**
  after an idle stop, `docker` can't connect until a `shipyard` command starts the VM
  again.

- **After the Mac restarts,** containers with a restart policy come back when you next
  run `shipyard` (any command starts the VM, and the VM's start brings them back): no
  login item starts the VM by itself yet.

- **Ports:** TCP only; a port published on the guest's IPv6 loopback
  (`-p [::1]:…`) isn't forwarded; nor is a port a second container publishes on the
  same number but another of the VM's addresses (the first keeps it; the host's log,
  `host.log` beside `console.log`, says so).
- **The guest is built from the source tree** (`scripts/guest/build.sh`), and
  `shipyard` must be signed with the virtualization entitlement
  (`scripts/macos/sign.sh`): see the developer notes (`docs/DEVELOPMENT.md`).
- **Detach keys:** Ctrl-P Ctrl-Q goes to the command in the VM: a `run -it` detaches
  there, as on Linux, and its container runs on. `shipyard` itself has no detach keys:
  a third SIGINT (a Ctrl-C, when you aren't typing into the command: then
  it's the command's key), or a SIGTERM the command doesn't end within 10 s, makes it
  let go, and the command in the VM is then hung up on.
- **File watching:** a change on the Mac isn't seen as it happens by a program in a
  container that waits for it (a development server's reload): it has to poll.

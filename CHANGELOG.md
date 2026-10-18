# Changelog

All notable changes to Shipyard. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (before 1.0, a minor version may change
behaviour; each change is listed here).

## 0.3.0 - unreleased

### Added

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

### Changed

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

### Fixed

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

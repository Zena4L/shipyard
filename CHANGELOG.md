# Changelog

All notable changes to Shipyard. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (before 1.0, a minor version may change
behaviour; each change is listed here).

## 0.2.0 - unreleased

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

# Shipyard's systemd units

Two sets: the boot units (below), which start and stop containers with the host, and
the [Docker API's units](#the-docker-api), which start `shipyard system service` when a
Docker client connects.

## Starting containers when the host starts

Shipyard has no daemon, so nothing runs it when the host starts: after a reboot,
containers with a restart policy stay ended until something runs
`shipyard system recover`. These units run it at boot, and `shipyard system stop` at a
shutdown: `shipyard-recover` starts the containers, and `shipyard-stop` stops them.
`shipyard-recover` settles what the last boot left (containers that ended unwatched),
then starts each container whose restart policy brings it back:

| Policy | Started when the host starts |
|---|---|
| `always` | yes, even if you stopped it |
| `unless-stopped` | unless you stopped it |
| `on-failure[:N]` | if it last ended with a failure, and N isn't used up |
| `no` | no |

### Root's containers

```sh
sudo install -m 0644 shipyard-stop.service shipyard-recover.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now shipyard-stop shipyard-recover
```

### Your (rootless) containers

Rootless containers live under your systemd user manager, which runs only while you're
logged in unless you let it linger. With linger, it starts at boot, and so does the unit:

```sh
sudo loginctl enable-linger "$USER"   # Fedora's polkit refuses it without sudo
mkdir -p ~/.config/systemd/user
install -m 0644 user/shipyard-stop.service user/shipyard-recover.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now shipyard-stop shipyard-recover
```

Without linger, your user manager starts at your first login after a boot, and the
unit with it; and it stops at your last logout, ending the containers the unit
started. (A container made in a login session runs on after you log out where logind
has `KillUserProcesses=no`, as on Ubuntu 24.04; with `yes`, it ends with the session:
make it under your user manager, `systemd-run --user --scope shipyard run -d …`.)

### Notes

- **Two units, so a shutdown always stops the containers.** systemd runs a unit's
  stop only once the unit has started, and `shipyard-recover` is still starting while
  it starts the containers at boot. So the stop is `shipyard-stop`'s: it starts at
  once (there's nothing to start), and is ordered before `shipyard-recover`, so at a
  shutdown it's stopped after it. `shipyard-recover` then ends only its own `recover`
  (`KillMode=process`), if it's still starting containers, and leaves every container
  to `shipyard-stop`'s `system stop`. (`shipyard-recover` wants `shipyard-stop`, so
  enabling it alone brings the stop too, once both files are installed.)
- `--now` starts the units at once, as well as at every boot: `system recover` runs
  now (a container that's up is left as it is), and the stop runs at this boot's
  shutdown. Without it, the first shutdown after installing ends the containers
  without their stop: they come back at the next boot all the same, but ungracefully.
- **From an install before 0.3** (one file, `shipyard-recover.service`, with the stop
  in it): install both files again, `daemon-reload`, and enable `shipyard-stop`.
  `shipyard doctor` warns while the stop unit isn't enabled.

- Edit `ExecStart` and `ExecStop` if `shipyard` isn't in `/usr/local/bin`.
- The units are `Type=oneshot` with `RemainAfterExit=yes`, so they stay active and
  the stop runs at the shutdown. Each container's monitor runs in a cgroup of
  Shipyard's own (`shipyard/monitors/<id>`), out of the unit's and out of any login
  session's, whoever started it: a shutdown ends it only after the units' stops, so
  `system stop` reaches every container, one made with `sudo shipyard run -d` from a
  session too. (Where there's no cgroup tree, a rootless login without delegation,
  the monitor stays where it was started.)
- A container that can't be started is said in the journal (`journalctl -u
  shipyard-recover`; `journalctl --user -u shipyard-recover` for the user unit), with
  exit code 125, and the unit still counts as started (`SuccessExitStatus=125`): else
  systemd would end every container it did start. If nothing could be done at all
  (the store can't be read), `system recover` exits 1 and the unit fails.
- At shutdown, and whenever `shipyard-stop` stops (`systemctl stop shipyard-stop`),
  `shipyard system stop` stops **every** container that's up, each with its own stop
  signal and timeout, all at once (a paused one is thawed for it). That isn't your
  stop: their restart policies bring them back at the next boot. Then
  `KillMode=mixed` kills what's left of the unit's own. `TimeoutStopSec=5min` bounds
  it: a container whose `--stop-timeout` is longer is killed before its own, and
  `shipyard doctor` says so; raise it with `systemctl [--user] edit shipyard-stop`.
  A user's units are bound by their user manager's stop too, `user@UID.service`'s:
  120 s upstream, but **5 s on Ubuntu 24.04** (`user@.service.d/timeout.conf`), so
  there a rootless container with the default 10 s stop timeout is killed at a
  shutdown unless it stops within 5 s. `shipyard doctor` compares with the earlier of
  the two, and says which to raise (`sudo systemctl edit user@.service` for the
  user manager's, for every user).
- `shipyard-recover` is stopped with `shipyard-stop` (`PartOf=`), so `systemctl restart
  shipyard-stop` stops every container and brings back those their restart policies
  bring back, as a reboot would; after `systemctl stop shipyard-stop`, `systemctl
  start shipyard-stop shipyard-recover` does. Until then restart policies are held
  off, and `shipyard doctor` says so. Stopping `shipyard-recover` alone stops no
  container.
- `shipyard doctor` says whether the units are enabled, and (rootless) whether linger
  is on.
- **Stores moved by the environment.** The units run with systemd's environment, not
  your shell's: containers made where `SHIPYARD_IMAGE_STORE`, `SHIPYARD_CONTAINER_DIR`,
  `SHIPYARD_NETWORK_DIR`, `SHIPYARD_EVENTS_DIR` (or, for your own user,
  `XDG_DATA_HOME`) was set are in stores they don't look in. Give both the same
  variables: `systemctl [--user] edit shipyard-recover`, then under `[Service]`, one
  `Environment=NAME=VALUE` line each, and the same for `shipyard-stop`. `shipyard
  doctor` says when this is so.

## The Docker API

`shipyard.socket` and `shipyard.service` serve Docker's Engine API (a subset: see
[the Docker API](../../docs/user/api.md)) for Docker's own clients. systemd listens on
the socket and starts the service at the first connection; the service exits once
nothing has been asked for 60 s, and the next connection starts it again. Nothing runs
while nothing asks.

Root's, for root's containers (the socket is root's alone: anyone who can use it can
run containers as root):

```sh
sudo install -m 0644 shipyard.socket shipyard.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now shipyard.socket
export DOCKER_HOST=unix:///run/shipyard/api.sock
```

Yours (rootless), for your containers:

```sh
mkdir -p ~/.config/systemd/user
install -m 0644 user/shipyard.socket user/shipyard.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now shipyard.socket
export DOCKER_HOST=unix://$XDG_RUNTIME_DIR/shipyard/api.sock
```

- Enable the socket, not the service: the service is started by its socket alone.
- `KillMode=process`: when the service exits (idle) or is stopped, only its own
  process ends. The containers it started run on: their monitors are in Shipyard's
  own cgroup (or, where there's no cgroup tree, left in the unit's).
- `Environment=SHIPYARD_API_IDLE=SECONDS` (`systemctl [--user] edit shipyard.service`)
  sets another idle time. The stores' variables go there too, as for the boot units.
- Edit `ExecStart` if `shipyard` isn't in `/usr/local/bin`.
- `shipyard doctor` says whether the socket unit is enabled, and where clients find it.

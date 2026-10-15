# Starting containers when the host starts

Shipyard has no daemon, so nothing runs it when the host starts: after a reboot,
containers with a restart policy stay ended until something runs
`shipyard system recover`. These units run it at boot. It settles what the last boot
left (containers that ended unwatched), then starts each container whose restart
policy brings it back:

| Policy | Started when the host starts |
|---|---|
| `always` | yes, even if you stopped it |
| `unless-stopped` | unless you stopped it |
| `on-failure[:N]` | if it last ended with a failure, and N isn't used up |
| `no` | no |

## Root's containers

```sh
sudo install -m 0644 shipyard-recover.service /etc/systemd/system/
sudo systemctl enable --now shipyard-recover
```

## Your (rootless) containers

Rootless containers live under your systemd user manager, which runs only while you're
logged in unless you let it linger. With linger, it starts at boot, and so does the unit:

```sh
sudo loginctl enable-linger "$USER"   # Fedora's polkit refuses it without sudo
mkdir -p ~/.config/systemd/user
install -m 0644 user/shipyard-recover.service ~/.config/systemd/user/
systemctl --user enable --now shipyard-recover
```

Without linger, your user manager starts at your first login after a boot, and the
unit with it; and it stops at your last logout, ending the containers the unit
started. (A container made in a login session runs on after you log out where logind
has `KillUserProcesses=no`, as on Ubuntu 24.04; with `yes`, it ends with the session:
make it under your user manager, `systemd-run --user --scope shipyard run -d …`.)

## Notes

- `--now` starts the unit at once, as well as at every boot: it runs `system recover`
  now (a container that's up is left as it is), and its stop runs at this boot's
  shutdown. Without it, the first shutdown after installing ends the containers
  without their stop: they come back at the next boot all the same, but ungracefully.

- Edit `ExecStart` if `shipyard` isn't in `/usr/local/bin`.
- The units are `Type=oneshot` with `RemainAfterExit=yes`, so they stay active and
  their stop runs at the shutdown. Each container's monitor runs in a cgroup of
  Shipyard's own (`shipyard/monitors/<id>`), out of the unit's and out of any login
  session's, whoever started it: a shutdown ends it only after the units' stops, so
  `system stop` reaches every container, one made with `sudo shipyard run -d` from a
  session too. (Where there's no cgroup tree, a rootless login without delegation,
  the monitor stays where it was started.)
- A container that can't be started is said in the journal (`journalctl -u
  shipyard-recover`; `journalctl --user -u shipyard-recover` for the user unit), with
  exit code 125, and the unit still counts as started (`SuccessExitStatus=125`): else
  systemd would end every container it did start.
- At shutdown, and whenever the unit stops (`systemctl stop`, or `restart`, which then
  starts them again), `shipyard system stop` stops **every** container that's up, each
  with its own stop signal and timeout, all at once. That isn't your stop: their
  restart policies bring them back at the next boot. Then `KillMode=mixed` kills what's
  left. `TimeoutStopSec=5min` bounds it: raise it for a container whose
  `--stop-timeout` is longer.
- A shutdown while the unit is still starting containers at boot skips its stop:
  systemd runs a unit's `ExecStop` only once it has started. The containers it has
  started by then end without their stop signal (they come back at the next boot all
  the same).
- `shipyard doctor` says whether the unit is enabled, and (rootless) whether linger
  is on.

# Containers

A **container** is something you make once and can start, and start again once it
has ended: a
record of how to run it (its image, command, networks, ports, volumes and limits),
its own writable layer, and, while it runs, its processes. Containers outlive the
`shipyard` command that started them: there's no daemon, each one has a small
monitor of its own.

(On Linux, and in a Mac's Linux VM. A foreground `shipyard run` of an image or `--rootfs` is a container
too, attached until it ends, or until you detach from one made with `-it`: see
[Running commands](run.md#a-container-in-the-foreground). `--no-image`, `--bundle`
and `--no-isolation` runs, and in this version `--preserve-fds` ones, make no
record; nor does a run where there's no directory for containers' state, such as
outside a login session.)

Every command here is also in Docker's `container` group: `container ls` (or
`ps`, `list`) is `ps`, `container rm` is `rm`, and so on for `run`, `create`,
`start`, `stop`, `restart`, `kill`, `logs`, `exec`, `inspect`, `cp`, `export`, `port`,
`rename`, `wait`, `attach` and `update`. As Docker's, `run` and `create` take `-v`
for `--volume` and `-h` for `--hostname` (their help is `--help`).

## Making and starting one

```sh
shipyard run -d --name web -p 8080:80 nginx       # make it and start it: prints its ID
shipyard create --name db postgres:16              # make it only: prints its ID
shipyard start db                                  # start it, in the background
```

- `run -d` and `create` take every flag `run` does (see [Running
  commands](run.md)), and the image first: `shipyard run -d IMAGE [COMMAND]...`.
- `create` pulls the image (as `--pull` says) and records the container; nothing
  runs and nothing is held until `start`.
- `start` starts containers in the background; one that's running is left as it is.
  It prints each container as you named it (name, ID, prefix or serial). It runs the **exact image the container was
  made from** (by its digest), never pulls, and never looks at a tag again: a tag
  moved since doesn't change a container.
- A start that fails leaves the container made and not started (`Created`, its
  last end cleared), as Docker does.
- A container's published ports are free again as soon as it has ended: `start`
  right after gets them back, even just after they served connections.
- `--rm` removes the container once it ends (its record, writable layer and logs);
  a start of it that fails removes it too.
- A Ctrl-C while `run -d` makes the container undoes everything; once the container
  is made, it waits until the ID is printed.
- `-l KEY=VALUE` (`--label`, repeatable) and `--label-file FILE` (a `KEY=VALUE` per
  line, as `--env-file`'s) label it: `inspect` shows them, and `ps --filter label=…`
  through the API finds them. `-l` wins over the file.
- `--cidfile FILE` writes its ID to FILE once it's made (a new file only: one that's
  there already is refused before anything is pulled or made, as Docker refuses it).
- `--expose PORT[/PROTO]` (or a range, `8000-8010`) adds to the ports its image
  exposes: `inspect` shows them in `ExposedPorts`, and `-P` publishes them beside the
  image's. They're kept beside its record (`created.json`).
- `-q` (`--quiet`) pulls without printing the pull's progress.
- `--privileged` and `--init` are refused, saying what to use instead (the
  capabilities and devices it needs; the command without an init).

Not with containers (they're foreground runs only): `--no-isolation`, `--bundle`,
`--no-image` (a container needs a root filesystem of its own: an image or
`--rootfs`) and `--preserve-fds`. A container's interfaces' own settings
(`--sysctl net.ipv4.conf.eth1.rp_filter=2`, and `net.ipv4.neigh.eth1.*`) are set once
its networks have made them, at each start (and at a `network connect`); one for an
interface its networks don't make (`eth3` with two networks) is refused. They name an
interface by its place (`eth1` is the second network), so after a `network
disconnect` the next start numbers what's left again: a setting for an interface it
no longer has is passed over, with a warning.

## Names, IDs and serials

Every container has:

- a **full ID**: 64 hex digits. `ps` shows the first 12.
- a **name**: the one you gave with `--name`, or a made-up `adjective_noun`
  (`brave_tugboat`). Letters, digits, `_` and `-`, starting with a letter or digit,
  at most 63 characters (`--name` and `rename` alike): a DNS label, so it's also the
  container's name on its networks, where it answers to its short ID too. So a
  name can't be another running container's short ID on a network they share.
- an **ISO 6346 serial**, like a shipping container's: `SHPU 304812 2`.

Every command that takes a container takes any of them: a name, the full ID, the
serial (with or without its check digit and spaces), or the start of the ID if
only one container's ID starts that way.

## Listing them: `ps`

```text
$ shipyard ps
CONTAINER ID   IMAGE     COMMAND                  CREATED         STATUS         PORTS                  NAMES
3f2a91c04b7e   nginx     "nginx -g 'daemon of…"   2 minutes ago   Up 2 minutes   0.0.0.0:8080->80/tcp   web
```

- Without `-a`, the containers that are up (running, paused, stopping, or waiting to
  restart); with `-a`, all of them. Newest first.
- `-q`: only their (short) IDs; `--no-trunc` doesn't cut IDs and commands short.
- `-n N`: only the N made last, `-l` the last one (of all of them, up or not).
- `-f KEY=VALUE` and `--format`: see [Filters](#filters) and [Formats](#formats)
  below. `-s` is taken (`Size` is `0B`: Shipyard doesn't measure it).
- `PORTS` shows a container's published ports while it's up.
- `STATUS` is `Created`, `Up 3 minutes`, `Paused`, `Stopping`,
  `Restarting (1) 2 seconds ago` (its [restart policy](#restart-policies) starts it
  again soon; the code is how it last ended), or `Exited (137) 2 hours ago` (`Exited (?)` when how it ended isn't known: its
  monitor was killed, say; see [Ended unwatched](#ended-unwatched)). A container
  with a [health check](#health-checks) adds what it says: `Up 3 minutes (healthy)`,
  `(unhealthy)`, `(health: starting)`. One whose network was lost while it ran (its pasta died:
  see [Networking](networking.md)) adds `(no network)`.
- `COMMAND` is the command it runs: yours, or its image's entrypoint and command where
  you gave none (as a start resolves them).
- `ps` never waits for another command: it reads what's there.

### Filters

`-f KEY=VALUE` (repeatable; a key given twice matches either value, but `label`
matches only a container with every label asked for), as Docker's:

| Key | Matches |
|---|---|
| `id` | the start of its ID |
| `name` | what its name contains: Docker's regular expression, as far as text, `^`, `$` and `.` (any character) go (`^web$` for exactly `web`); `*`, `+`, `?`, `[`, `(`, `{`, `\|` and `\` are refused |
| `label` | `KEY` (it has the label) or `KEY=VALUE` |
| `status` | `created`, `running`, `paused`, `restarting`, `exited` (looks at every container, as `-a` does) |
| `exited` | how it ended (`exited=0`; every container too) |
| `health` | `starting`, `healthy`, `unhealthy` or `none` |
| `ancestor` | its image as given (a tag left out is `latest`), or the start of its image's ID |
| `network` | a network's name, or the start of its ID |
| `volume` | a volume's name, or where a mount is in the container |
| `before`, `since` | made before or after the container named (its ID, its name, or a start of its ID no other has) |

A key is taken in any case, with spaces around it (`-f ' Status = exited'`), as
Docker's. Any other key is refused (exit 1), never ignored; one without `=` is a
usage error (125).

### Formats

`--format` is Docker's, for `ps`, `images`, `volume ls`, `network ls` and `events`:

- `table` (the default): the list's columns, aligned, with a header.
- `json`: each object, as Docker's fields, one compact JSON object per line.
- `table TEMPLATE`: a Go template, its tab-separated cells aligned under a header
  (`table {{.Names}}\t{{.Status}}`); `\t` and `\n` typed as two characters are a
  tab and a line break.
- `TEMPLATE`: a Go template, written once per object (`{{.Names}} {{.Image}}`).

`ps`'s fields are Docker's: `ID`, `Names`, `Image`, `Command`, `CreatedAt`,
`RunningFor`, `Ports`, `State`, `Status`, `Size`, `Labels`, `Mounts`,
`LocalVolumes`, `Networks`, `Platform` (empty), `HealthStatus`, and `.Label "KEY"`
for one label's value. Docker's functions are there: `json`, `split`, `join`,
`title`, `lower`, `upper`, `pad`, `truncate`, and Go's own (`printf`, `index`,
`len`, `eq`, `if`, `range`…). A field an object hasn't, or a template that doesn't
parse, fails (exit 1). Times (`CreatedAt`) are in UTC. With `-q`, a format is
ignored (a warning says so), as Docker's.

What a template writes from a container (a name, a label, a command) can't act on
your terminal, or make a row or a column of its own: a control character in it, a
tab and a line break too, is escaped (`\u{1b}`). `--format json` writes the
object's own text, those characters escaped as JSON escapes them (`\u001b`), so
it stays the same JSON; `{{json .Field}}` inside a template writes the field as the
template reads it, already escaped. `pad` adds at most 4,096 spaces a side.

Not yet as Docker's: `Ports` lists each port on its own (Docker groups a range,
`0.0.0.0:8000-8002->8000-8002/tcp`); `--format json` writes `<`, `>` and `&` as
they are (Docker's rows write `\u003c`, the same JSON).

## Looking closer: `inspect`

```sh
shipyard inspect web
shipyard inspect -f '{{.State.Status}} {{.Config.Image}}' web
shipyard inspect --type volume data
```

`inspect` finds any object, as Docker's does: each name is the first of a
container, an image, a network and a volume that has it (`--type container|image|
network|volume` looks for one kind only). It prints a JSON array, one object per
name, each in Docker's shape (indented 4 spaces); `-f json` one compact array;
`-f TEMPLATE` a Go template over each object's JSON, a line each, with Docker's
functions ([Formats](#formats)). A key an object hasn't fails the template (exit 1):
Docker does that for a field and prints `<no value>` for a map's key, Shipyard can't
tell the two apart (`{{index .Config.Labels "k"}}` is empty for a label it hasn't).
`-s` is taken. A container's object is Docker's `ContainerInspectResponse`:

| Field | What Shipyard fills |
|---|---|
| `Id`, `Name` (`/web`), `Created` | Who it is, and when it was made (RFC 3339) |
| `Path`, `Args` | The program it runs and its arguments: what you gave, else its image's entrypoint and command (as a start resolves them) |
| `State` | `Status` (`created`, `running`, `paused`, `restarting`, `exited`), `Running` (true while it's up, waiting to restart too, as Docker says), `Paused`, `Restarting`, `OOMKilled`, `Pid` (its init's, on the host; 0 when it isn't up), `ExitCode`, `StartedAt`, `FinishedAt` (`0001-01-01T00:00:00Z` for none), `Health` (with a [health check](#health-checks): `Status`, `FailingStreak`, `Log`) |
| `Image` | The digest every start runs |
| `LogPath` | Its last start's log directory |
| `ResolvConfPath`, `HostnamePath`, `HostsPath` | Its `/etc` files on the host while it's up; empty once it has ended (they go with the run) |
| `RestartCount` | How many times its restart policy started it again since you last started it |
| `Mounts` | Its binds and volumes |
| `GraphDriver` | `overlay`, with its `LowerDir` (the image's layers, top first), `UpperDir`, `WorkDir` and `MergedDir` once it has started (`null` before) |
| `Config` | `Hostname`, `User`, `Tty`, `OpenStdin`, `Env` (its image's variables, then yours, which win), `Cmd`, `Entrypoint`, `Image`, `WorkingDir` (`User`, `Cmd`, `Entrypoint` and `WorkingDir` are its image's where you gave none), `Labels`, `ExposedPorts` (the image's and those you published), `Healthcheck` (times in nanoseconds), `StopSignal`, `StopTimeout` |
| `HostConfig` | `Binds`, `PortBindings`, `RestartPolicy`, `AutoRemove`, `NetworkMode`, capabilities, DNS, `ExtraHosts`, `SecurityOpt`, `ReadonlyRootfs`, the resource limits, `Ulimits`, `Sysctls`, `LogConfig` (`json-file`, the driver Docker's clients know: Shipyard keeps every container's logs itself, in its own records, read with `logs`) |
| `NetworkSettings` | `Ports` (where each is published while it's up), `Networks` (each network it joins, with its address, prefix, gateway and hardware address there while it's up, and `IPAMConfig`: the addresses you asked for) |

Each field is Docker's, with Docker's type (checked against a real Docker's `inspect`
of the same flags); an empty list is `null`, as Docker writes it. Some of Docker's
fields aren't there yet (its `ShmSize`, the image's manifest descriptor, …): see the [limitations](README.md#limitations). A
container that's stopping shows as `running`, as Docker's does until its end.

`shipyard inspect --shipyard web` shows Shipyard's own view instead, with what Docker
has no field for:

| Field | What it is |
|---|---|
| `id`, `serial`, `name` | Who it is |
| `created` | When it was made (RFC 3339, UTC) |
| `image`, `image_id` | The image as you gave it, and the digest every start runs |
| `state` | `status` (`created`, `running`, `paused`, `stopping`, `restarting`, `exited`), `running`, `pid`, `exit_code`, `oom`, `started_at`, `finished_at`, `health` (`status`, `failing_streak`, `log`), `network_lost` (while it's up, if its pasta died: `time`, `how`) |
| `ports` | The host ports its last start published |
| `labels` | Its labels |
| `auto_remove` | Whether it goes once it ends (`--rm`) |
| `restart_count` | Its restarts since you last started it |
| `log_dir` | Its last start's log directory |
| `config` | How it runs, as its record keeps it |

A name found nowhere is said on stderr (`no such object: NAME`); the others are
still printed, and the exit code is 1 (Docker's). An image, a network and a volume
are shown as `image inspect`, `network inspect` and `volume inspect` show them.

## What it mounts

`--volume` (`-v`) and Docker's `--mount` mount a named volume or a host path into
the container, and `--tmpfs` a tmpfs, in memory and empty at each start: see
[Volumes](volumes.md#dockers---mount-and---tmpfs). A container whose create can't
be read again at a start (its `created.json` beside the record, damaged) isn't
started: it may hold tmpfs mounts it would write to disk without. Remove it and
make it again.

## Sharing another's IPC: `--ipc container:NAME`

`--ipc container:NAME` starts a container in another running container's IPC namespace:
its SysV shared memory, semaphores and message queues, and its `/dev/shm` (POSIX shared
memory: a file one writes there, the other reads). Its processes, files and mounts are
still its own. Compose's `ipc: "service:NAME"` is the same, and so is the API's
`IpcMode`.

```sh
shipyard run -d --name db postgres:16
shipyard run --rm --ipc container:db alpine:3.20 ls /dev/shm
```

- `--ipc private` (the default) and `shareable` give a container its own; any running or
  paused container's can be joined. `--ipc host` and `none` are refused.
- The other must be running at each start (`cannot join IPC of a non running container:
  NAME`). It's kept by its ID: a rename changes nothing. `stop`, `restart` and `rm` of it
  say who shares its namespaces. One that joins another's IPC namespace itself can't be
  joined: join that one.
- IPC settings (`kernel.shm*`, `kernel.msg*`, `kernel.sem`, `fs.mqueue.*`) are the
  other's: refused with it.
- Without root, a container's network and IPC namespace can be one other container's, not
  two (its user namespace, joined with them, owns them). As root, a container with a user
  namespace of its own (`--uidmap`) can't join another's IPC namespace.
- Its `/dev/shm` is the other's: its size (64 MiB) is shared, so one can fill it for both,
  and pages are charged to the memory of the container that writes them. It's mounted
  `nosuid,nodev,noexec` and private in this one, whatever the other did to its own. One
  whose `/dev/shm` isn't a tmpfs (another mount put there) can't be joined.
- It needs a root filesystem of its own (an image or `--rootfs`). As root, a container
  made with `--uidmap` can't be joined (its shared memory is its user namespace's).
- `inspect` shows `HostConfig.IpcMode` as `container:ID` (`private` otherwise).

## Its ports: `port`

`shipyard port CONTAINER` shows the host ports a running (or paused) container
publishes, one line each, as Docker does:

```console
$ sudo shipyard network create --ipv6 app
$ sudo shipyard run -d --name web --network app -p 80 -p 127.0.0.1::53/udp nginx:alpine
$ sudo shipyard port web
53/udp -> 127.0.0.1:41377
80/tcp -> 0.0.0.0:38021
80/tcp -> [::]:38021
$ sudo shipyard port web 80
0.0.0.0:38021
[::]:38021
```

- Lines are in the order of the container's ports (as Docker's).
- A port published on every address shows `0.0.0.0`, and `[::]` too where IPv6
  reaches the container: on a network with IPv6 (`network create --ipv6`), or,
  rootless, with pasta on a host with IPv6. `ps`'s `PORTS` column says the same.
- `shipyard port CONTAINER PORT[/PROTO]` shows only the host side of that port
  (`tcp` unless you say `udp`). If nothing is published there, it says so ("no
  public port '81/tcp' published for web") and exits 1.
- A container that isn't running (or paused) publishes nothing: `port` prints
  nothing, and `port CONTAINER PORT` says "no public port" and exits 1.

`shipyard port --sockets CONTAINER` shows the container's own sockets instead (Linux):
what it listens on, and its connections, as `ss` would inside it, with the container
at the other end named where it's one on your networks (root's: a rootless
container's peers aren't named):

```console
$ sudo shipyard port --sockets web
PROTO   LOCAL              REMOTE            STATE    PEER
tcp     0.0.0.0:80         -                 LISTEN   -
tcp     10.89.0.2:80       10.89.0.3:41562   ESTAB    api
```

`--format json` gives the same as JSON (`protocol`, `local`, `remote`, `state`,
`peer`). Those it listens on come first. A container that isn't running has none
(exit 1).

## Its files: kept until `rm`

What a container writes is kept in its **own writable layer**, stacked on its
image's layers, from its first start until it's removed: a file written before it
stopped is there when it starts again. `shipyard image prune` never removes it, nor
the image layers it stacks.

## Stopping one: `stop`, `kill`, `restart`, `wait`

```sh
shipyard stop web              # its stop signal, then SIGKILL after its timeout
shipyard stop -t 2 web         # 2 seconds before SIGKILL (--timeout, or --time; -1: no SIGKILL, and
                               # rm -f of it waits meanwhile; kill still works)
shipyard kill web              # SIGKILL at once; kill -s HUP web sends HUP
shipyard restart web           # stop, then start
shipyard wait web              # waits for it to end; prints its exit code
```

- `stop` sends the container's stop signal (`--stop-signal` when it was made,
  `SIGTERM` by default) to its main process, and SIGKILL once its stop timeout
  (`--stop-timeout`, 10 s by default) is up; `-s` and `-t` override them. It
  returns once the container has ended and let go of everything (its ports, its
  writable layer), so a `start` right after works. One that isn't running is left
  as it is.
- The container's monitor carries the stop out: if `stop` itself is killed (or
  the terminal closed) while it waits, the SIGKILL still comes at the timeout.
- A second `stop` (or an `rm -f`) while one is waiting doesn't wait behind it: the
  earliest SIGKILL of the two comes.
- A paused container can't act on its stop signal: stopping one takes its whole
  timeout, then SIGKILL.
- `kill` sends a signal (SIGKILL unless `-s` says) and returns at once. It works
  while a `stop` is waiting. One that isn't running (or is still starting) is an
  error (exit code 1).
- `restart` is a `stop` then a `start`, with nothing let in between (another
  `start`, `stop` or `rm` of it waits). One that isn't running is started.
- `wait` prints each container's exit code once it has ended (128 + N for signal
  N: 143 for SIGTERM, 137 for SIGKILL; 125 if how it ended isn't known); at once
  for one that has ended already, and 0 for one never started. A `--rm`
  container's code is printed too, though the container goes as it ends.

Each takes several containers: each one that fails is said, the others still
go, and the exit code is 1 (Docker's). `stop`, `kill`, `restart` (and `update` and `rm`)
print each container as you named it once it's done, as `start` does. A reader that
goes away early (`shipyard stop a b c | head -1`) ends them quietly, the rest left as
they are: that isn't a failure (a failure before it still gives 1).

## Restart policies

```sh
shipyard run -d --name web --restart unless-stopped nginx:alpine
shipyard update --restart no web        # change it, the container kept
```

`--restart` (on `run` and `create`) says what happens when the container ends:

| Policy | Started again when it ends |
|---|---|
| `no` (the default) | never |
| `on-failure[:N]` | with an exit code other than 0 (or one not known), at most N times |
| `always` | however it ends |
| `unless-stopped` | however it ends |

- **Never after you stop it:** `stop`, `kill` (with SIGKILL or its stop signal),
  `restart` and `rm -f` end the restarts, whatever the policy. `always` and
  `unless-stopped` differ [when the host starts again](#after-a-reboot).
- **It waits a little first:** 100 ms, doubling after each run shorter than 10 seconds,
  up to a minute; a run of 10 seconds or more starts it over. Meanwhile `ps` shows it
  `Restarting`, and it counts as up: `rm` refuses it (stop it first, or `rm -f`).
- **A full disk delays it:** when a container's end can't be recorded, Shipyard
  keeps how it ended and tries again, waiting as above (up to a minute apart), until
  there's room, then applies the policy. It stops trying at `shipyard system stop`,
  `rm` or a reboot. A `stop` that couldn't be recorded on a full disk either isn't
  known then: the policy may start the container again.
  `update --restart` takes effect at once, also on one waiting to restart.
- **Each restart is a start of its own**, from its record, as `start` makes one: new
  logs (`logs` shows every start's), and a new address unless it has `--ip`. Only the
  latest 10 starts' logs are kept (every start counts, yours too), so a crash loop
  doesn't fill the disk. A restart that can't start (an image or a
  bind mount gone) counts as a failed run.
- `inspect` shows `RestartCount`: its restarts since you last started it (`start`
  counts them afresh). `on-failure:N` counts those.
- `wait` and a foreground `run` return at its first end, with that code; the
  container goes on restarting.
- `--rm` can't go with a policy (a container removed as it ends can't be started
  again), nor can `update` give one to a `--rm` container.

No daemon does this: when a container ends, its monitor's clean-up starts it again
after the delay, and the new start's monitor does the same at the next end. So the
restarts run the Shipyard that made the container, with the environment of the
command that made it (after an upgrade, `rm` and `run` it again to change that).

## Health checks

```sh
shipyard run -d --name web --health-cmd 'wget -qO- http://localhost/ || exit 1' nginx:alpine
shipyard ps
```

```text
CONTAINER ID   IMAGE          COMMAND   CREATED          STATUS                    PORTS   NAMES
3f2a91c04b7e   nginx:alpine   ""        30 seconds ago   Up 29 seconds (healthy)           web
```

A **health check** is a command run in the container, as `exec` runs it, every
`--health-interval` while the container runs: its exit code says whether the
container is well (`0`) or not (anything else). An image may bring its own (a
Dockerfile's `HEALTHCHECK`: its config's `Healthcheck`), which every container of it
runs; `--health-cmd` gives one instead, run with the container's shell (the image's
`SHELL`, if it sets one, else `/bin/sh -c`).

| Flag | Default | What it is |
|---|---|---|
| `--health-cmd CMD` | the image's | The check |
| `--health-interval D` | `30s` | Between two checks |
| `--health-timeout D` | `30s` | The longest a check may take: then it's a failure, and it's killed |
| `--health-start-period D` | none | After a start, how long failed checks don't count |
| `--health-start-interval D` | `5s` | Between two checks during the start period |
| `--health-retries N` | `3` | How many failed checks in a row make it unhealthy |

`D` is a duration: `500ms`, `30s`, `1m`, `2h` (never zero: leave a flag out for its
default). The defaults are the image's check's settings, where it has them, else
Docker's (those in the table).

How the flags and the image's check combine, Docker's way, once, when the container
is made (`inspect`'s `Config.Healthcheck` shows the result; every start runs it):

- **No flags:** the image's check, as it is (none, if it has none).
- **`--health-cmd`:** that command, with the image's settings for any flag left out.
- **Other `--health-*` flags alone:** the image's check, those settings changed. With
  an image that has no check, there's nothing to change: they're ignored, with a
  warning.
- **`--no-healthcheck`:** no check at all, not even the image's (refused with any
  `--health-*` flag). An image whose check is `NONE` has none either, unless
  `--health-cmd` gives one.
- **An image's check that can't be run** (`["CMD"]` with no program, say, or a
  duration under 1 ms, which Docker refuses) is left out, with a warning; the
  container is made all the same. A `--health-cmd` that's empty is refused (Docker
  would take it as not given).

What the checks say, Docker's way:

- **`starting`** from the start until the first check passes, or until
  `--health-retries` checks in a row have failed after the start period;
- **`healthy`** once a check passes (in the start period too), and while they do;
- **`unhealthy`** after `--health-retries` failures in a row; a check that passes
  makes it healthy again. Once it has ended (or waits to restart), it's `unhealthy`
  whatever its checks last said, its failing streak and latest results kept, as
  Docker says it once its checks stop.

In the start period, checks come every `--health-start-interval` until one passes,
and failures don't count while it's still `starting`; once it has passed, they do.

A check that can't run (no such command: `127`), or that runs out of time
(`Health check exceeded timeout (30s)`), is a failure. A check under way when the
container ends, or is paused, counts for nothing.

`ps` shows it after the status while it runs (`starting` from the start, as Docker's), `inspect` shows it with the latest five checks'
output (the first 4 KiB of each), and each change is an event:
`shipyard events -f event=health_status` (see [Events](events.md)). Each start
begins `starting` again. A paused container isn't checked; its checks go on once
it's unpaused. Unhealthy changes
nothing else, as in Docker: a restart policy starts a container again only when it
ends.

The checks are run by a small companion process beside the container, which its
monitor starts and ends with it (`ps` on the host shows it as `__health`); each check
is a process of its own, stopped with what it started if it runs out of time. The
check is read from the container's record, never put on a command line; but like
any process in a container, the command it runs (`/bin/sh -c CMD`) is visible to
`ps` on the host: don't put secrets in `--health-cmd`.

## Pausing one: `pause`, `unpause`

```sh
shipyard pause web       # its processes frozen where they are
shipyard unpause web     # and running on
```

`pause` freezes every process of a running container at once, through its cgroup's
freezer (`cgroup.freeze`): none of them is told, none can run, until `unpause`.
`ps` says `Up 3 minutes (Paused)` and `inspect`'s `State.Status` is `paused`. While
it's paused, `exec` is refused; `stop` and `kill` still work (a stop thaws it first,
so its stop signal is seen at once). Pausing one that's paused, or unpausing one
that isn't, fails (exit 1, as Docker's), as does either on one that isn't running.
Each is said as a `pause` or `unpause` event. It needs the container's own cgroup:
root, or a delegated one rootless (`shipyard doctor` says whether you have one).

## Changing its limits: `update`

```sh
shipyard update -m 512m --cpus 1.5 --pids-limit 200 web
shipyard update --restart on-failure:3 web
```

`update` changes a container's limits without making it again: `-m`/`--memory`,
`--memory-reservation`, `--memory-swap`, `--cpus`, `--cpu-period`, `--cpu-quota`,
`-c`/`--cpu-shares`, `--cpuset-cpus`, `--cpuset-mems`, `--pids-limit`,
`--blkio-weight`, and its [restart policy](#restart-policies). The ones not given
stay as they were: `--cpu-period` alone keeps its quota, and a swap that was the
default (as much as its memory) follows a new `--memory`. Its record is changed
first, for its next starts; then a running (or paused) container's cgroup, at
once. A limit the kernel won't take is refused: its record is put back, and its
cgroup's earlier limits are written again. A stopped container's take effect at
its next start, so they're refused now where a start couldn't apply them (rootless
without the cgroup controller delegated). Not yet: device rates
(`--device-read-bps` and the like: give them when it's made), and lifting a limit
(`--pids-limit -1`, the API's `CpuQuota` of `-1`: give a number). Without any flag,
or with only Docker's zeros, it's refused (exit 1).

A `kill` with a signal other than SIGKILL lets a paused container run on, so its
processes see the signal, as Docker's does.

## Its processes and what it uses: `top`, `stats`

```sh
shipyard top web                      # its processes, in `ps -ef`'s columns
shipyard stats                        # what the running ones use, every second
shipyard stats --no-stream web db     # once
shipyard stats --no-stream --format '{{.Name}} {{.CPUPerc}} {{.MemUsage}}'
```

`top` lists a running (or paused) container's processes, the ones in its cgroup,
as Docker's does: `UID PID PPID C STIME TTY TIME CMD`, `ps -ef`'s columns, with
host PIDs (its main process's is `inspect`'s `State.Pid`). Shipyard reads them
from `/proc` itself, so `ps`'s own options (`top web -o pid`) aren't taken (exit
1; `-ef` is what's shown anyway), and `STIME` is in UTC. A container that isn't running is refused (exit 1).

`stats` shows what each container uses, as Docker's CLI works it out from two
samples a second apart: `CPU %` (its CPU time's share of the host's, times the
host's CPUs: `200%` is two CPUs' worth), `MEM USAGE / LIMIT` (its memory less the
page cache nothing is using; the limit is the host's memory when it has none),
`MEM %`, `NET I/O` (every interface but `lo`, received / sent; none on the host's
network, `--network host`, as Docker's), `BLOCK I/O` (read
/ written) and `PIDS`. Without names it shows the running containers (`-a`: all),
listed again each second; it refreshes every second, the screen cleared first on a
terminal, until Ctrl-C, or prints once with `--no-stream`. `--format` takes
Docker's fields (`.ID`, `.Name`, `.Container` (as you named it), `.CPUPerc`,
`.MemUsage`, `.MemPerc`, `.NetIO`, `.BlockIO`, `.PIDs`), `table …` and `json`;
`--no-trunc` shows whole IDs. One that isn't running (or stops meanwhile) shows zeros; one that can't be
read (no such container; rootless without a cgroup of its own) is said and left
out, and `stats` exits 1. Both need the container's own cgroup: root, or a
delegated one rootless.

## After a reboot

Shipyard has no daemon, so nothing runs it when the host starts: after a reboot, every
container that was up reads as ended (`Exited (?)`), until something runs

```sh
shipyard system recover
```

It settles what the reboot left (as the next `run` would: [Ended
unwatched](#ended-unwatched)), then starts each container its restart policy brings
back, printing their names:

| Policy | Started when the host starts again |
|---|---|
| `always` | yes, even if you stopped it |
| `unless-stopped` | unless you stopped it |
| `on-failure[:N]` | if it last ended with a failure (one nothing recorded counts), and N isn't used up |
| `no` | no |

A container that's up, or was never started, is left as it is. A container that
can't be started is said, and the exit code is 1; it isn't tried again (as Docker's
daemon doesn't at its start: a restart policy follows a run, never a failed start), so
start it yourself once what failed is fixed.

At shutdown, `shipyard system stop` stops every container that's up, each with its own
stop signal and timeout (`--stop-signal`, `--stop-timeout`), all at once. A
container waiting to restart stays down, and one its restart policy is starting at
that moment is stopped once it has started. That isn't your stop: their restart
policies bring them back at the next boot, and none is restarted meanwhile. Until
`shipyard system recover` runs (the boot unit runs it), `shipyard doctor` warns that
restarts are held off: run it yourself if you ran `system stop` by hand. Each
container's monitor runs in a cgroup of Shipyard's own (`shipyard/monitors/<id>`), so
the shutdown's stop reaches one you made from a login session (`sudo shipyard run -d`
over SSH) as surely as one the boot unit started.

To run both, install the systemd units in `contrib/systemd` (see its README):
`shipyard-recover.service` runs `system recover` at boot, and `shipyard-stop.service`
runs `system stop` at a shutdown, even one that comes while the boot's start is still
going; for root's containers, and user units for yours. Rootless, your user manager
runs at boot only with linger (`sudo loginctl enable-linger $USER`); without it, the
user units run at your first login after the boot. `shipyard doctor` says whether the
units are enabled, whether linger is on, and when a container's stop timeout is
longer than the stop unit's.

A paused container (`shipyard-runtime pause`) is thawed for its stop, so its stop
signal is had at once, as Docker does.

## Logging out (rootless)

A container made from a login session (an SSH login, say) keeps its monitor in that
session. What happens at logout depends on logind's `KillUserProcesses`:

- **`no`** (Ubuntu 24.04's default, for one): the container runs on. Its monitor keeps
  the session from closing, and with it your user manager and `$XDG_RUNTIME_DIR`.
- **`yes`** (systemd's own default, where a distribution keeps it): logind ends the
  session's processes at logout, the container's monitor among them, and the container
  with it, even with linger. Make containers under your user manager instead, where
  logind leaves them: `systemd-run --user --scope shipyard run -d …` (with linger).
  `shipyard doctor` warns when this is so.

The same goes for **root's containers made with `sudo`** from a login session: their
monitors are in that session too, so with `yes` they end at its logout (logind spares
root's own logins, not a user's that ran `sudo`). Make them outside it with
`sudo systemd-run --scope shipyard run -d …`; the boot unit's always are.
Networks' DNS servers aren't affected either way: each runs in a cgroup of its own
(see [Networking](networking.md#names)).

Containers the **user unit** starts live under your user manager: without linger it
stops at your last logout, and they end with it.

## Ended unwatched

A container's monitor records how it ended. When nothing is left to do that (the
monitor was killed outright, the host restarted, or, rootless, your last logout
removed `$XDG_RUNTIME_DIR`), the container's record still says it started and
never finished: `ps -a` shows `Exited (?)`, with no time.

The next command that changes containers (`create`, `run`, `start`, `restart`,
`rm`) settles every such container first:

- its end is recorded: the time the host booted for a container of an earlier boot,
  else the time it was noticed; how it ended stays unknown (`Exited (?) 5 seconds
  ago`, exit code 125 for `wait`);
- what its start left behind is cleaned up (its runtime state and cgroup);
- a `--rm` container is removed, its writable layer with it, as its own clean-up
  would have.

Each is said on stderr (`warning: container NAME ended while nothing recorded its
end …`). A container another command is working on is left alone, for the next one:
settling never waits. `ps` and `inspect` settle nothing: they show what's recorded.

## Renaming one: `rename`

```sh
shipyard rename web site
```

gives a container a new name (`--name`'s rules: letters, digits, `_` and `-`,
starting with a letter or digit, at most 63), running or not; it
prints nothing. A running container's name on its networks changes too: within a
second, other containers find it as `site`, and `web` is nobody's (once no other
container has it). The new name must be no other container's, and, on each of its
networks, no name another container there answers to (its name, an alias or its run
ID, ignoring case, as DNS does: two containers named `web` and `Web` can't be on one
network together).

If a running container's name couldn't be changed on one of its networks (it says
so), renaming it to the same name again finishes the job.

## Removing one: `rm`

```sh
shipyard rm db          # a stopped container
shipyard rm -f web      # a running one: killed first
shipyard rm -v cache    # and its anonymous volumes (see volumes.md)
```

`rm` removes the container's record, its writable layer and its logs, and lets go
of its image (`rmi` and `image prune -a` can then remove it). A running (or restarting)
container is refused without `-f`. It prints each container as you named it once
it's removed. Several can be given: each one that can't be removed is said,
the others still go, and the exit code is 1. Named volumes outlive containers.

A container whose record can't be read (damaged, or written by a newer Shipyard) is
left out of `ps`, with a warning, and keeps `create`, `run` and `rename` from
working: its name may be one a new container would get. Remove it by its full ID,
with `-f`:

```sh
shipyard rm -f 3f2a91c04b7e…   # all 64 characters, as the warning gives them
```

What's left of it is killed, and its writable layer, its hold on its image and its
directory (its logs) go. Its volumes and networks are left as they are.

## Removing what's stopped: `container prune`, `system prune`

```sh
shipyard container prune -f                       # every stopped container
shipyard container prune -f --filter until=24h    # made more than a day ago
shipyard system prune -f                          # stopped containers, unused networks, dangling images
shipyard system prune -a --volumes -f             # and every unused image, and unused anonymous volumes
```

`container prune` removes every stopped container: made and never started, or
ended. It never touches one that's running, paused, or about to be restarted by
its policy. Each one is removed as `rm` removes it, under its own lock, so a
container started meanwhile is kept. It prints Docker's `Deleted Containers:`
with their IDs, then `Total reclaimed space:` (their writable layers).
`--filter` takes `until` (a time, or a duration such as `24h`: made before
then), `label=KEY[=VALUE]` and `label!=KEY[=VALUE]`.

`system prune` does it all at once, each part in its own section: the stopped
containers; the networks no container uses (as root: rootless has none of its
own); the unused anonymous volumes with `--volumes` (a named volume is never
taken); then the dangling images, or every image no container was made from with
`-a`; then what nothing in the image store needs. `until` applies to containers,
networks and images, and the labels to all of them; `until` isn't taken with
`--volumes` (a volume's age isn't kept), as Docker's. A part that fails is said,
and the rest go on; it exits 1 then. A container its restart policy is about to
start again is never pruned (nor removed by `rm` without `-f`: it's restarting).

Both ask first, with Docker's warning, unless given `-f`. Without a terminal to
ask on they're refused (exit 1): Docker reads an answer from any input, but a
prune removes data, so Shipyard asks only someone who can see the question, as
`volume prune` does.

## Its image

A container keeps the image it was made from, by digest. `shipyard rmi` refuses an
image a container uses (`… is used by container 3f2a91c04b7e`); `rmi -f` untags it
anyway, and the container keeps what it needs to start again.

## Its logs: `logs`

```sh
shipyard logs web              # everything it wrote, every start's, oldest first
shipyard logs -n 20 web        # the last 20 lines (across its starts; --tail)
shipyard logs -f web           # then what comes, until it stops
shipyard logs --since 10m -t web
shipyard logs --until 5m web   # only what it wrote up to five minutes ago
```

Each start's output is kept in its own log directory, beside the container's record
(`logs/0000000001`, `logs/0000000002`, …), the latest 10 starts' kept; `logs` reads
them all, a stopped container's too. Its stdout goes to your stdout and its stderr to your stderr.
`-f` follows the newest start until the container stops, as Docker's does: if it's
started again, run `logs -f` again for the new start; it also ends once nothing reads
its output (`logs -f web | head -n1`). `--tail` counts lines across
starts. A line a start left unfinished (it ended mid-line) is ended before the next
start's output. `--since` and `--until` take a time (RFC 3339), Unix seconds, or a
duration ago (`10m`, `2h`, `3d`): with `--until`, `--tail` counts the lines up to it,
and `-f` ends once it's past. `-n all` (`--tail all`) is every line, as Docker's.
`-t` starts each line with the time it was written.

What happened to a container (made, started, ended with which code, stopped,
renamed, removed) is in [`shipyard events`](events.md).

## Attaching to it: `attach`, `start -a`

```sh
shipyard attach web            # its output, and your input; Ctrl-P Ctrl-Q leaves
shipyard start -a -i db        # start it attached: from its first byte
```

`attach` connects you to a running container: what it writes comes to you, and
what you type goes to it (unless `--no-stdin`; it needs `-i` when it was made to
take input). With a terminal (`-t` when it was made), your terminal is put in raw
mode and its size follows yours. Ctrl-P Ctrl-Q (or `--detach-keys`) leaves it
running. Without one, your terminal sends what you type a line at a time, so the keys
take effect when you press Enter (as with Docker). Started in the background (`attach … &`) with a terminal, it waits for
`fg` before it changes your terminal. `attach` exits with the container's exit code
when it ends, `0` when you detach, `1` if it isn't running. `start -a` starts a created or stopped container
with you attached before its command can write anything, so nothing is missed (`-i`
passes your input on, `--detach-keys` as `attach`'s); one that's running already is
attached to as it is. Ctrl-C reaches a container with a terminal (your terminal is
in raw mode: it's a keystroke for the container's own terminal); without one, it
ends your attachment and leaves the container running (in this version).

## Running a command in it: `exec`

```sh
shipyard exec web cat /etc/hostname
shipyard exec -it web sh                 # a shell, with a terminal
shipyard exec -u www-data -w /srv web ls
shipyard exec -e DEBUG=1 web ./check
shipyard exec -e TOKEN web ./deploy      # TOKEN's value from your shell
```

`exec` runs a command in a running container: in its namespaces and its cgroup, as
its own process runs, never with more than it has. Without `-i`, the command reads
end-of-file; with it, your input. `-t` gives it a terminal. `-u USER[:GROUP]` takes
names (from the container's own `/etc/passwd` and `/etc/group`, with the groups that
list the user, and any the container was given with `--group-add`) or numbers;
`-e KEY=VALUE` sets a variable, and a bare `-e KEY` copies your shell's value (left
out if your shell has none), as `run` takes them. `HOME` is that user's home unless
you set it with `-e`, or the container has a
`HOME` of its own (given with `-e`, or by its image). `-d` returns once it runs,
and can't go with `-t`. The exit code is the command's (128 + N if a signal
ended it), `127` if it isn't found, `126` if it can't be run, `1` if Shipyard
couldn't (the container isn't running, say), as Docker's. `--env-file FILE` reads
variables as `run --env-file` does (before `-e`, which wins). `--detach-keys` and
`--privileged` are refused: an exec's command ends with your session, so there's
nothing to detach from yet (`-d` runs it in the background).

## Copying files: `cp`

```sh
shipyard cp web:/etc/nginx/nginx.conf ./nginx.conf
shipyard cp ./site web:/usr/share/nginx/html
shipyard cp web:/var/log - | tar -t      # a tar stream
```

As `docker cp`: one side is the container (`NAME:PATH`), the other a host path or
`-` (a tar stream on stdin or stdout, never to a terminal). Files copied in become
root's in the container unless `-a` keeps their owners (where the container has
those IDs); `-L` follows a source that's a symbolic link.

The container doesn't have to be running. A stopped container's files are its own
writable layer over its image, and its volumes: `cp` mounts them in a short-lived
helper process of its own, as a start would, and copies through them. What you copy
in is there at its next start. A container made with `create` and never started
works too (its writable layer and its volumes are made then, as its first start
would make them); a volume removed since a container last ran is said, never made
again empty. Its volumes are mounted at their
places, so `cp db:/var/lib/data/x .` reaches a volume's files; read-only ones stay
read-only, and so does the root of a `--read-only` container. A `start` that comes
during such a copy waits for it to finish.

One thing only a running container has: its `/etc/hostname`, `/etc/hosts` and
`/etc/resolv.conf` come from its network. A stopped container shows what's under
them in its own files there (its image's, or the empty ones its start made), and
a copy into one of them is hidden by its network's at the next start.

While a copy works on a container, running or not, it holds the container: a
`start`, `stop` or `rm` that comes meanwhile waits for it (up to two minutes).

## Its whole filesystem: `export`

```sh
shipyard export web > web.tar           # or: shipyard export -o web.tar web
shipyard export web | shipyard import - web-snapshot:1
```

As `docker export`: the container's own filesystem as one tar archive (its image's
files and what it changed, named from its root: `bin/sh`), running or stopped, to
standard output (never a terminal) or `-o FILE` (written beside it and renamed at the
end, as `save -o`, and yours alone: mode 0600). **Never its volumes, binds or tmpfs:** a directory a volume is
mounted on is exported with what's under it in the container's own files, as
Docker's. Running, it reads the container's root mount alone (a copy of the mount
with nothing mounted on it, made inside the container's namespaces); where the kernel
refuses that copy, it walks the root as it's mounted and leaves out whatever is
mounted on it (each place written as an empty directory or file), and says so (a
warning from `shipyard export`; Docker's API has no way to say it): a directory a
volume covers is then exported empty. That walk tells a mount by its ID, which needs
Linux 5.8. Stopped, it reads the container's
writable layer over its image, and takes no volume at all (a removed volume, or a
bind's source gone, is nothing to it). Like `cp`, it never follows a link (one that
points out of the container is written as a link) and never opens a device or a FIFO.
Like `cp`, it holds the container while it streams: a `start`, `stop` or `rm` waits
for it (up to two minutes), so a slow reader of a large export holds those up. What's
exported is what `import` makes an image of again.

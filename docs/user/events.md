# Events

`shipyard events` tells you what happens: containers made, started, ended and
removed; images pulled and removed; volumes and networks made and removed. Every
`shipyard` command that changes something writes it down, and so does each
container's clean-up, so a container that ends on its own while nobody is watching
is in the record too.

```sh
shipyard events                          # what happens from now on, until Ctrl-C
shipyard events --since 1h --until 0s    # what happened in the last hour, then exit
shipyard events --until 0s               # everything kept, then exit
shipyard events --since 1h               # the last hour, then what comes
shipyard events -f container=web -f event=die
shipyard events --since 3d --until 0s --format json
```

## What it prints

One line for each event, as Docker prints them: its time (RFC 3339, UTC), its type,
what happened, what it's about, and more about it in brackets (a control character
in a name is printed escaped, `\u{1b}`, never sent to your terminal):

```text
2026-10-14T09:00:00.123456789Z container create 3f2a…c1 (image=alpine, name=web)
2026-10-14T09:00:00.231940117Z container start 3f2a…c1 (image=alpine, name=web)
2026-10-14T09:00:04.002318554Z container die 3f2a…c1 (exitCode=3, image=alpine, name=web)
2026-10-14T09:01:12.554100002Z volume destroy data
```

With `--format json`, each line is in the journal's own format (written out again
as Shipyard writes it: times to the nanosecond): one JSON object, with its
format's version (`v`, 1 for now) first.

```json
{"v":1,"time":"2026-10-14T09:00:04.002318554Z","type":"container","action":"die","id":"3f2a…c1","attributes":{"exitCode":"3","image":"alpine","name":"web"}}
```

| Type | Actions | Attributes |
|---|---|---|
| `container` | `create`, `start`, `stop`, `kill`, `restart`, `die`, `oom`, `rename`, `update`, `destroy`, `health_status`, `network_lost` | `name`, `image`; `exitCode` (`die`), `signal` (`kill`, by name: `SIGKILL`, where Docker gives its number), `oldName` (`rename`), `restartCount` (a [restart policy's](containers.md#restart-policies) `start`), `restartPolicy` (`update`), `healthStatus` (`health_status`: `healthy` or `unhealthy`), `how` (`network_lost`) |
| `image` | `pull`, `untag`, `delete`, `prune` | `removed` (`prune`: how many blobs) |
| `volume` | `create`, `destroy`, `prune` | `removed` (`prune`: how many volumes) |
| `network` | `create`, `destroy`, `prune`, `connect`, `disconnect` | `removed` (`prune`: how many networks), `container` (`connect`, `disconnect`: the container's ID) |

- A container's `id` is its full ID; a volume's or a network's is its name.
- An image's is its reference in full (`docker.io/library/alpine:latest`) for `pull`
  and `untag`. `rmi` gives `untag`, then `delete` (by the image's digest) if that was
  the last reference to it.
- `stop` and `kill` say what you asked for; `die` says the container ended,
  however it ended, with its exit code. A container the OOM killer killed gives
  `oom`, then `die`.
- `network_lost` is said when a container's pasta dies while it runs (attribute
  `how`: `pasta was killed (SIGKILL)`, `pasta ended (code 1)`): it runs on without a
  network (see [Networking](networking.md)).
- `health_status` is said when a container's [health checks](containers.md#health-checks)
  change what they say: Docker's `health_status: healthy`, with the status as an
  attribute, so `-f event=health_status` finds them all.
- A volume `run --volume NAME:…` makes on its first use gives `volume create`; so
  does the default `bridge` network, the first time a container uses it.

## Past events, and following

- **Without `--since`**, `events` prints only what happens from now on, and waits
  for more until you stop it (Ctrl-C), as Docker's does.
- **`--since TIME`** prints what happened from then on first.
- **`--until TIME`** stops there. A time already gone by (`--until 0s`: now) prints
  what there is and exits; one still to come waits until then. Without `--since`,
  it starts from the oldest event kept, as Docker's does. A `--until` before the
  `--since` is refused (exit code 125).

`TIME` is a time (`2026-10-14T09:00:00Z`), Unix seconds (`1760432400`), or a
duration ago (`30s`, `10m`, `2h`, `3d`).

Each event has the time it happened, which isn't always when it was written down: a
container's `die` has the time its monitor saw it end, and one found to have ended
unwatched (see [Ended unwatched](containers.md#ended-unwatched)) has the best time
there is: its monitor's, if it saw the end; the time it was found otherwise; the
machine's boot, for one that was running when the machine went down. So the times in the journal aren't always in order; `events` keeps the
order they were written in, and bounds them by their own times.

A reader that goes away (`shipyard events | head -n 5`) ends it quietly.

## Filters

`-f`/`--filter KEY=VALUE` keeps the events that match. Give a key more than once for
any of its values; different keys must all match.

| Key | Matches |
|---|---|
| `type` | `container`, `image`, `volume` or `network` |
| `event` | the action (`start`, `die`, …) |
| `container` | a container's name, or its ID (or the start of it); a network's `connect` and `disconnect` of it too, by its ID |
| `image` | an image (`alpine` is `docker.io/library/alpine:latest`), or a container made from it |
| `volume` | a volume's name |
| `network` | a network's name |

```sh
shipyard events -f type=container -f event=start -f event=die
shipyard events --since 1d --until 0s -f image=alpine
```

A key it doesn't know is refused (exit code 125).

## Where they're kept

The journal is a file of JSON lines in a private directory:

- as root, `/var/lib/shipyard/events/`;
- for a normal user, `~/.local/share/shipyard/events/`;
- when `SHIPYARD_CONTAINER_DIR` moves the containers' records, beside them:
  `events/` beside a directory named `containers`, else `NAME.events/`
  (`/tmp/c`'s is `/tmp/c.events/`);
- wherever `SHIPYARD_EVENTS_DIR` says, when it's set.

Every command adds to the end of `events.log` with one write, so events written at
the same moment by several commands never mix. Past 1 MiB, it's moved to
`events.log.1` (the one before is dropped) and a new one begun: between 1 and 2 MiB
of events are kept, some thousands of them. `events` reads both, and a follower
carries on across the move (even two moves between its looks, five times a second).
A line the journal can't read (another version's, or a damaged one) is skipped.

Writing an event never fails the command that does: if the journal can't be written
(a full disk, say), the command says so once and goes on.

## What isn't there yet

- Docker's `exec_create`/`exec_start`, `attach` and `pause` events.
- `--format` with a Go template.
- Events from `shipyard-runtime` (it has its own `events`, for a container's cgroup:
  see [the OCI runtime](runtime.md)).

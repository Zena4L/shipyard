# Volumes

A volume is persistent storage a container mounts. Unlike the container's own
writable layer — which is thrown away when the container is removed — a volume's
contents outlive the container.

> **Status:** named volumes and the `--volume` flag work on Linux and, through
> Shipyard's Linux VM, on a Mac, where a bind's source must be in a place the VM
> shares ([macOS](macos.md)). Mounting into a container needs a root filesystem, so
> use them with an image or `--rootfs`.

## Named volumes

```sh
shipyard volume create data              # create a named volume
shipyard volume ls                       # list volumes
shipyard volume inspect data             # Docker's shape, as JSON (-f a template)
shipyard volume rm data                  # remove a volume and its contents
shipyard volume prune                    # remove every volume no container uses
```

`volume ls` (`volume list`) lists them as Docker's does (`DRIVER` and `VOLUME
NAME`, by name): `-q` the names, `-f` Docker's `name` (what it contains), `label`,
`dangling` (no container uses it) and `driver`, `--format` its fields (`Name`,
`Driver`, `Scope`, `Mountpoint`, `Labels`, `.Label "KEY"`; `Links`, `Size`,
`Group`, `Availability`, `Status` are `N/A`), as [`ps`'s](containers.md#formats).

A named volume is a directory Shipyard owns, under the store
(`volumes/<name>/_data`). A name may contain letters, digits, `_`, `.` and `-`,
and must start with a letter or digit — no slashes.

## Mounting into a container

```sh
shipyard run --volume data:/var/lib/data alpine sh -c 'echo hi > /var/lib/data/f'
shipyard run --volume data:/var/lib/data alpine cat /var/lib/data/f    # hi
```

`--volume SOURCE:DEST[:ro]`:

| Part | Meaning |
|---|---|
| `SOURCE` | a **named volume** (a bare name, no slash — created on first use) or a **host path** (absolute, and it must exist — resolved, symlinks followed, and bind-mounted) |
| `DEST` | where it appears inside the container (an absolute path) |
| `:ro` | mount it read-only (`:rw`, the default, is read-write) |

Repeat `--volume` for more than one. A named volume that doesn't exist yet is
created the first time you mount it, as Docker does.

```sh
# A named volume, created on first use:
shipyard run --volume cache:/cache alpine …

# A host directory, bind-mounted read-only:
shipyard run --volume /etc/myapp:/config:ro alpine …
```

Volumes mount into the container's root filesystem, so they need one — use
`--volume` with an image or `--rootfs`.

### Docker's `--mount`, and `--tmpfs`

```sh
shipyard run --mount type=volume,source=data,target=/var/lib/data alpine …
shipyard run --mount type=bind,source=/etc/myapp,target=/config,readonly alpine …
shipyard run --mount type=tmpfs,target=/cache,tmpfs-size=64m alpine …
shipyard run --tmpfs /run:size=16m,mode=1777 alpine …
```

`--mount` takes Docker's `KEY=VALUE` list: `type` (`volume`, the default, `bind` or
`tmpfs`), `source` (or `src`: a volume's name, or a bind's absolute host path),
`target` (or `destination`, `dst`), `readonly` (or `ro`), and for a tmpfs
`tmpfs-size` (bytes, or with `k`, `m` or `g`: `64m`; Docker's `64MiB` and `1.5g`
aren't taken) and `tmpfs-mode` (octal). A bind or a volume is the
same as `--volume`'s, and shows as one in `inspect`'s `Binds`.
`volume-nocopy` keeps the volume from being filled from the image (below).
`bind-propagation=rprivate` (or `private`) and `consistency` are taken (they
change nothing here: a container's mounts never reach the host). Anything else
is refused. A volume without a source is an anonymous one (below).

### A new volume gets the image's files

```sh
shipyard run -v pgdata:/var/lib/postgresql/data postgres   # pgdata gets the image's files there
shipyard run -v pgdata:/var/lib/postgresql/data:nocopy …   # stays as it is
```

As Docker's: when a container is created (`create`, or `run`), each of its
volumes (named or anonymous) that's still empty is filled with what the image
has at its place, owners, modes and links kept, and the volume's own directory
takes the owner and mode the image gives that place. A volume with anything in
it is never touched, and a container's starts never fill: a volume the
container empties stays empty. A bind (a host directory) is never filled. What
the copy left out or changed (an owner the user namespace can't have, a socket)
is said as a warning.

`-v NAME:/path:nocopy` (with `ro`: `:ro,nocopy`), `--mount …,volume-nocopy` or
the API's `VolumeOptions.NoCopy` asks for none. One fill of a volume runs at a
time (Ctrl-C stops a create waiting for another's). A fill that fails (a full
disk, a file the copy refuses) fails the create, and what was copied stays. The
volume is marked unfinished, naming the container whose create began it: if
that create was killed and its container is left, its first start finishes the
fill before it runs, if nothing else has used the volume meanwhile. Another
container's create never copies into a volume that holds anything, as Docker's: it
says the fill was left unfinished and gives it up (what it writes there is never
copied over later), and a volume removed and made again is filled anew. Nor does
that first start finish it if another container mounts the volume too (one made
with `nocopy`, say): the fill is given up, said, and what's there is kept.

A fill leaves the store's free space alone, as a pull does: 5 % of the filesystem,
at least 1 GiB and at most 10 GiB (on a filesystem of 1 GiB or less, no fill fits).
An image with more than a million files that have several names each is refused
rather than copied, as each further name would be a copy of its data. A fill is on
the disk (`syncfs`) before its volume is marked whole.

`--tmpfs PATH[:OPTIONS]` mounts a tmpfs: a file system in memory, empty at each
start (what's written there is gone when the container stops), charged to the
container's memory limit. It's `noexec`, `nosuid` and `nodev` unless its options
say `exec`, `suid` or `dev`, as Docker's; it also takes `ro`, `size` (`64m`;
without one, the kernel's default, half the host's memory), `mode`, `uid`, `gid`
and `nr_inodes`. `inspect` shows them in `HostConfig.Tmpfs` and `Mounts`. One
place can't take two mounts (`--volume`, `--mount` or `--tmpfs`): it's refused.
Mounts are made a parent first, as Docker's, so a tmpfs on `/data` never hides a
volume on `/data/db`.

## Removing volumes

A volume a container uses is never removed, whether the container is running or
stopped (as Docker's): `volume rm` refuses it, naming the containers ("the volume
data is in use by web, db"; "in use by a running container" if what holds it has no
record, such as a `--no-image` run), and `volume prune` keeps it and says so. Once
the container is removed (`shipyard rm`), both remove it. A container's record that
can't be read (damaged, or a newer Shipyard's) might use any volume, so while there's
one, `volume rm` refuses and `volume prune` removes nothing, naming it; and a
container made while either runs is waited for, never missed.

`volume prune` deletes data, so it asks first:

```text
Remove every volume no container uses, and its data? [y/N]
```

Anything but `y` (or `yes`) keeps everything. Where there's no terminal to ask on
(a script, CI), it refuses unless you give `-f`/`--force`.

`volume rm` takes several names; one it can't remove is reported, the rest are
still removed, and the command exits 1.

**Rootless.** A rootless container's other users (any user but root inside) are
your subordinate IDs on the host (from `/etc/subuid`), so what they write into a
volume isn't yours to delete directly. Shipyard removes it from a user namespace
with the same mapping, as it does for image layers.

## Where volumes live

Volumes sit beside images, under the store directory (see
[Images](images.md#where-the-store-lives)): `volumes/<name>/_data`. Set
`SHIPYARD_IMAGE_STORE` to move the whole store.

## Through Docker's API

`docker volume` works against Shipyard's [API](api.md) too, and a volume made there can
have labels (`docker volume create --label tier=db data`, as `docker compose` makes
them). They're kept beside the volume's data, in `labels.json`, and shown by
`docker volume inspect`. `shipyard volume create -l tier=db data` labels one the same
way; without a name it makes one with a new name of its own (64 hex digits), as
Docker's. One that's there already is left as it is, its labels too (Docker's
`local` driver does the same). `-d local` is the only driver; driver options (`-o type=nfs`) are refused: a
volume is a directory of the store's.

## Anonymous volumes

```sh
shipyard run -v /data alpine …                        # a new volume at /data
shipyard run --mount type=volume,target=/cache alpine …
shipyard rm -v web                                    # the container and its anonymous volumes
```

A volume asked for without a name (`-v /path`, `-v /path:ro`, a `--mount` volume
without a `source`) is made when the container is: a new volume with a name of 64
hex digits, labelled `com.docker.volume.anonymous`, as Docker's. So is each of
the image's own `VOLUME`s that no `--volume`, `--mount` or `--tmpfs` covers. One
at a place the runtime or the run keeps for itself (under `/proc`, `/sys` or
`/dev`, or the run's `/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf`) is said
and left in the container's own layer; an image declaring more than 128 is
refused. The container keeps the same volumes across its starts; `inspect`
shows them in `Binds` and `Mounts`.

A plain `rm` keeps them, as Docker's: `volume prune` takes them later (`shipyard
volume prune` takes every volume no container names, as older Docker did; the API's
`docker volume prune` takes only anonymous ones unless `--all`, as Docker's since 1.42). `rm -v` removes the anonymous volumes the
container's create made, each that no other container names. A volume it was
given by name (`-v NAME:/path`), a named one or another's anonymous one, is
never removed with it, as Docker keeps it. A container run with `--rm` takes its
anonymous volumes when it goes. A create that fails removes the ones it made.

## Not yet

- Volume drivers and options.
- A bind outside the places the Mac's VM shares (the home, `/Volumes`, `/tmp`,
  `/var/folders`): refused there ([macOS](macos.md)).

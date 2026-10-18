# Volumes

A volume is persistent storage a container mounts. Unlike the container's own
writable layer — which is thrown away when the container is removed — a volume's
contents outlive the container.

> **Status:** named volumes and the `--volume` flag work on Linux (mounting into
> a container needs a root filesystem, so use them with an image or `--rootfs`).
> The `volume` commands manage directories and work on macOS and Linux.

## Named volumes

```sh
shipyard volume create data              # create a named volume
shipyard volume ls                       # list volumes
shipyard volume inspect data             # show its mount point on the host
shipyard volume rm data                  # remove a volume and its contents
shipyard volume prune                    # remove every volume no container uses
```

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
still removed, and the command exits 125.

**Rootless.** A rootless container's other users (any user but root inside) are
your subordinate IDs on the host (from `/etc/subuid`), so what they write into a
volume isn't yours to delete directly. Shipyard removes it from a user namespace
with the same mapping, as it does for image layers.

## Where volumes live

Volumes sit beside images, under the store directory (see
[Images](images.md#where-the-store-lives)): `volumes/<name>/_data`. Set
`SHIPYARD_IMAGE_STORE` to move the whole store.

## Not yet

- Volume drivers, labels, and `--mount`'s longer syntax.
- `--volume` on macOS (it needs the Linux VM a later version manages): it's refused
  there.

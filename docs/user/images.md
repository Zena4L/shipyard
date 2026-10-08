# Images

Shipyard pulls container images from a registry, keeps them in a local store,
and runs them. An image's layers become a container's root filesystem.

> **Status:** on Linux, `shipyard pull` and `shipyard run --image` pull, store and
> run images, rootless or as root. Running an image needs Linux (it uses
> overlayfs); on macOS it will run in the VM a later version manages. Building,
> pushing and signature verification aren't here yet.

## Running an image

```sh
shipyard run --image alpine:3.20 cat /etc/alpine-release
```

The first time, Shipyard pulls `alpine:3.20` from Docker Hub, verifies and stores
its layers, and runs the command inside the image's filesystem. The next time, the
store already has it, so it starts immediately.

With no command, the image's own default runs:

```sh
shipyard run --image alpine:3.20          # runs the image's CMD (here, /bin/sh)
```

The image's baked-in **environment**, **working directory**, **user**, and
**entrypoint/command** are the defaults, combined as Docker combines them. Any
flag you give wins: `-e KEY=VALUE` adds to (or overrides) the image's
environment, a `COMMAND` replaces its default command (and follows its
entrypoint), `--entrypoint` replaces the entrypoint, `-w` sets the working
directory, `-u` the user. The rules are in
[Running commands](run.md#image---image).

`--image` works with the rest of `run`: `--read-only`, limits (`--memory`,
`--cpus`, …), `--volume`, `--cap-add`/`--cap-drop`, and so on. See
[Running commands](run.md).

## Pulling

`run --image` pulls an image only if the store doesn't have it; after that, runs
need no registry and no network. `shipyard pull` fetches one ahead of time, and
always asks the registry, so a tag that moved is brought up to date:

```sh
shipyard pull alpine:3.20       # prints each layer, then Digest: sha256:…
shipyard pull -q alpine:3.20    # the digest only
```

The digest printed is the manifest of this machine's platform, the one `images`
lists; for a multi-platform tag, Docker prints the index's instead.

`run --pull` chooses, as Docker's flag does: `missing` (the default), `always`,
or `never` (the store's copy, or an error). Layers download in parallel.

## Image references

A reference names the registry, repository and version:

```
[registry[:port]/]repository[:tag][@digest]
```

| Reference | Resolves to |
|---|---|
| `alpine` | Docker Hub `library/alpine:latest` |
| `alpine:3.20` | Docker Hub `library/alpine`, tag `3.20` |
| `someone/app:1.2` | Docker Hub `someone/app`, tag `1.2` |
| `ghcr.io/owner/app:v1` | GHCR `owner/app`, tag `v1` |
| `localhost:5000/app` | a local registry on port 5000 |
| `alpine@sha256:…` | the exact content, pinned by digest |

The rules follow Docker's: a bare single name is an official image under
`library/`; the registry is the first path segment only if it has a `.` or `:`,
is `localhost`, or is an IPv6 address in brackets (`[::1]:5000`); with no tag or digest, the tag is `latest`. A digest pins the
exact bytes and wins over a tag.

**Multi-architecture images.** A tag like `alpine:3.20` is usually a *manifest
list* with one image per platform. Shipyard picks the one matching the host
(`linux/amd64`, `linux/arm64`, …). If an index has no image for the host, it says
which platforms it does have.

**Registries.** Docker Hub, GHCR and other registries that speak the OCI
distribution protocol work. A `localhost`/`127.0.0.1` registry is reached over
HTTP, as a local development registry; others over HTTPS only.

## Private registries

Shipyard signs in with the credentials Docker keeps, so `docker login` works for
both:

```sh
docker login ghcr.io
shipyard pull ghcr.io/owner/private-app:v1
```

It reads `$DOCKER_CONFIG/config.json` (or `~/.docker/config.json`), as Docker
does:

- `credHelpers` names a registry's own credential helper;
- `credsStore` names the helper for every other registry (`osxkeychain`,
  `desktop`, `pass`, `secretservice`, `ecr-login`, …);
- `auths` holds credentials written into the file itself.

A helper is the program `docker-credential-<name>`, found on `PATH`, the same one
Docker runs. Shipyard asks it once per pull, when the registry first asks for
authentication (Docker Hub does for every image, public ones too; signed in, you
get its higher pull limits). A helper gets 60 seconds to answer (a keychain may
ask you to unlock it first). If it fails, or isn't installed any more, Shipyard
warns and goes on without signing in, which is all a public image needs. Identity
tokens (Azure ACR, some single sign-on logins) work too.

Credentials are sent only to the registry and to the token service it names,
over HTTPS (plain HTTP only to a registry, and its token service, on this
machine), and never to another host a redirect leads to, such as a download CDN.
They never appear in messages or logs. If a registry turns them down, Shipyard
says where they came from, so you know what to sign in to again.

## Managing the store

```sh
shipyard images                 # list images in the store
shipyard rmi alpine:3.20        # remove an image (shared layers survive)
shipyard image prune            # remove everything no image references
shipyard system df              # the store's disk usage
```

`shipyard images` lists each reference, its manifest digest, and its layer count:

```text
REFERENCE                       MANIFEST       LAYERS
docker.io/library/alpine:3.20   2c3e5c8f1a2b   1
```

**Deduplication and garbage collection.** Identical layers shared by two images
are stored once. `rmi` removes a reference and reclaims only the blobs and layer
directories no *other* image still needs — a shared base layer survives removing
one of the images that use it. `image prune` reclaims everything unreferenced,
and what interrupted work left behind.

An image a running container uses isn't removed (`rmi --force` removes its name;
its layers stay until the container ends and a prune collects them). A prune or
`rmi` never takes what a pull or a starting run is using: it waits for them, up
to 15 seconds, then says the store is busy.

`system df` shows what images, running containers' writable layers and volumes
take, and how much of each nothing uses now (reclaimable):

```text
TYPE         COUNT   SIZE       RECLAIMABLE
Images       2       41.3 MiB   3.9 MiB
Containers   1       12.0 KiB   0 B
Volumes      1       2.1 MiB    0 B
```

Rootless, some directories are your subordinate IDs' (an image's other users), and
can't all be read: a size then shows as `≥`, at least that much.

`rmi` takes several references; one it can't remove is reported, the rest are still
removed, and the command exits 125.

## Where the store lives

| Who runs it | Store directory |
|---|---|
| root | `/var/lib/shipyard/images` |
| anyone else | `$XDG_DATA_HOME/shipyard/images`, or `~/.local/share/shipyard/images` |

Set `SHIPYARD_IMAGE_STORE` to override it — useful where the default is on an
overlay filesystem, which can't hold an image's writable layer. The store keeps
blobs under `blobs/sha256/`, extracted layers under `layers/`, per-container
writable areas under `containers/`, and the reference index in `index.json`.

## When the overlay can't be mounted

An image's root filesystem is an overlay: its layers stacked read-only under a
writable layer of the container's own, mounted inside the container. Where the
system refuses that mount, `run` says why and what to do:

| Message says | Cause | What to do |
|---|---|---|
| the store's filesystem can't hold an overlay's writable layer | the store is itself on overlayfs (inside another container, say), or, rootless, on a filesystem without user extended attributes | set `SHIPYARD_IMAGE_STORE` to a directory on a disk filesystem (ext4, xfs, btrfs) |
| a rootless overlay needs Linux 5.11 or later | an older kernel | run as root, or on a newer kernel |
| this system doesn't let the container's root mount overlay here | an AppArmor or other security policy, or an image store the container's root can't reach | run as root, or lift the policy |
| the image has N layers, more than this kernel can stack | a kernel older than 6.8 takes about 50 layers in one mount | a newer kernel (6.8 and later stack up to 500) |
| no overlay filesystem | the `overlay` module isn't loaded | `modprobe overlay` |

A store path can't contain `,`, `:` or `\`: an older kernel's mount options would
split or unescape them.

## Who owns an image's files

An image's files keep the owners, permissions and capabilities the image gives
them: `/tmp` is writable by everyone, a `postgres` image's data directory is
`postgres`'s, `ping` keeps its capability.

As root, that's what's on disk. Rootless, the files belong to your **subordinate
IDs** (`/etc/subuid` and `/etc/subgid`, used through `newuidmap` and `newgidmap`
from the `uidmap` package): the image's root is you, its other users are IDs in
your range, as with Podman. Shipyard extracts and removes them inside a user
namespace of yours, since only there can you own them. Where you have no
subordinate IDs, every file is yours instead, setuid bits are dropped, and
Shipyard warns: "the image's files will all belong to you, not to its users".

## How a pull is kept honest

Every blob — the manifest, the config, and each layer — is named by its sha256
digest, and verified against it as it streams to disk. A registry that served
different bytes under a digest Shipyard trusts is caught, and nothing is stored.
An image pinned by digest (`alpine@sha256:…`) must be exactly those bytes, and
every blob must be the size its manifest says.

Layers are extracted carefully: an entry can't reach outside its layer, through
`..`, a symlink or a hard link; an archive that would take too much memory or
disk is refused; an image's deletions (whiteouts) become overlay's own markers,
never written through a link. Each layer is checked against both of its digests,
compressed and uncompressed. The engineering detail is in
[the stage 6 notes](../stages/06-images.md).

**Interrupted downloads go on where they stopped.** A layer that's cut off, by
the network or by stopping the pull, is kept in the store as a partial download,
and the next attempt (or the next pull) asks the registry for only the rest. A
registry that can't send part of a blob sends it all again. What was kept is
checked like everything else, so a damaged partial download is simply fetched
again. `shipyard pull` (or `--pull always`) also re-checks the layers the store
already has, and fetches again any that a damaged disk changed. `image prune`
removes partial downloads.

## Not yet

- Building images, `push`, and signature verification (cosign/sigstore).
- `tag`, `inspect`, `history`, `save`/`load`, `import`/`export`.
- zstd-compressed layers (gzip works today; a zstd layer is refused by name).
- A fallback to `fuse-overlayfs` where the kernel won't let a normal user mount
  overlay.
- Running an image on macOS (it needs the Linux VM backend).

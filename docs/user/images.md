# Images

Shipyard pulls container images from a registry, keeps them in a local store,
and runs them. An image's layers become a container's root filesystem.

> **Status:** `pull`, `push`, `build`, `save`/`load`, `import`/`export` and
> `history` work, rootless or as root; on a Mac, in Shipyard's VM (see
> [macOS](macos.md)). Signature verification isn't here yet.

## Running an image

```sh
shipyard run alpine:3.20 cat /etc/alpine-release
```

The first time, Shipyard pulls `alpine:3.20` from Docker Hub, verifies and stores
its layers, and runs the command inside the image's filesystem. The next time, the
store already has it, so it starts immediately.

With no command, the image's own default runs:

```sh
shipyard run alpine:3.20          # runs the image's CMD (here, /bin/sh)
```

The image's baked-in **environment**, **working directory**, **user**, and
**entrypoint/command** are the defaults, combined as Docker combines them. Any
flag you give wins: `-e KEY=VALUE` adds to (or overrides) the image's
environment, a `COMMAND` replaces its default command (and follows its
entrypoint), `--entrypoint` replaces the entrypoint, `-w` sets the working
directory, `-u` the user. The rules are in
[Running commands](run.md#image).

Its **exposed ports** are what `-P` publishes, and its **health check** (a
Dockerfile's `HEALTHCHECK`) is every container's of it, unless `--health-cmd`
replaces it or `--no-healthcheck` turns it off (see
[Health checks](containers.md#health-checks)).

The image is `run`'s first argument; Shipyard's options go before it, and
everything after it is the command's (see [Arguments](run.md#arguments)). An image
works with the rest of `run`: `--read-only`, limits (`--memory`, `--cpus`, …),
`--volume`, `--cap-add`/`--cap-drop`, and so on:

```sh
shipyard run --memory 512m -e A=1 alpine:3.20 env
```

See [Running commands](run.md).

## Pulling

`run` pulls an image only if the store doesn't have it; after that, runs
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

Shipyard signs in with the credentials Docker keeps, and keeps its own sign-ins
there too, so `shipyard login` and `docker login` work for both:

```sh
shipyard login ghcr.io                       # asks for the username and password
echo "$TOKEN" | shipyard login -u me --password-stdin ghcr.io
shipyard pull ghcr.io/owner/private-app:v1
shipyard logout ghcr.io
```

`login` checks the credential against the registry first (a refused one is said,
`unauthorized: …`, and nothing is kept), then keeps it where `docker login` would:
the registry's credential helper (below) if the config names one, else in
`config.json` itself, base64 and **not encrypted** (said, as Docker says it). The
password is asked on the terminal without echo (a Ctrl-C cancels it, the terminal put
back); `--password-stdin` reads it from standard input (a trailing newline dropped);
`-p` works but is warned of (anyone listing processes sees it). `logout` removes the
sign-in from the helper and every spelling of the registry in the file (`ghcr.io`,
`https://ghcr.io`), and says "Not logged in to …" when there was none. The rest of the
file (its other keys, `currentContext`, `proxies`…) is kept as it was, its mode and
owner too; a config that's a link (dotfiles) stays one; a file that isn't Docker's JSON
is refused, never overwritten. A `docker login` at the same moment as a `shipyard login`
can lose one of the two (each rewrites the file whole), as two `docker login`s can.

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
tokens (Azure ACR, some single sign-on logins) work too, and so does a registry's own
bearer token (`registrytoken`), sent to it as it is.

Credentials are sent only to the registry and to the token service it names,
over HTTPS (plain HTTP only to a registry, and its token service, on this
machine), and never to another host a redirect leads to, such as a download CDN. A
token service that answers a request carrying credentials with a redirect is refused
("its token service redirected elsewhere, and credentials aren't sent on"); one asked
without credentials may redirect.
They never appear in messages or logs. If a registry turns them down, Shipyard
says where they came from, so you know what to sign in to again.

## Pushing (`push`)

```sh
shipyard tag app:1 ghcr.io/owner/app:1
shipyard push ghcr.io/owner/app:1      # NAME[:TAG]: :latest if none
shipyard push -a ghcr.io/owner/app     # every tag of the repository the store has
```

An image is pushed as it's stored: its blobs and manifest byte for byte, so the
registry's copy has the same digests and the same ID (`digest:` in the last line, as
Docker's: `1: digest: sha256:… size: 1234`). Each blob the registry has already is
skipped (`already there`); the others are sent, up to three at once, and the manifest
last, so the tag never names an image the registry hasn't got whole. A push signs in as
a pull does (the sign-in for the name's registry, asking for push access); one whose
credential can't be read (a helper that fails) fails, rather than going on anonymously.
A registry that takes the credential but won't let the account push to the repository
is said as that (`denied: …`), not as a sign-in to redo.

What a push trusts of the registry: nothing it says leads the upload to another host
(an upload `Location` elsewhere, a redirect: refused, never sent there); each upload
step goes where the step before said; a step that fails, or a token that expires mid-way,
starts that blob's upload over, and a blob whose commit went through though its answer
was lost isn't sent twice. An image pulled through a multi-platform index keeps only
this platform's manifest, so it's pushed as that platform's image; pushed back to the
very tag that still names that index, it's refused (it would make that multi-platform
image this platform's alone): push to another tag.

## Managing the store

```sh
shipyard images                 # list images in the store
shipyard tag alpine:3.20 base   # another name for the same image
shipyard image inspect base     # the image, in Docker's shape (JSON)
shipyard rmi alpine:3.20        # remove an image (shared layers survive)
shipyard image prune -f         # dangling images, and everything no image references
shipyard image prune -a -f      # every image no container was made from, too
shipyard system df              # the store's disk usage
```

The `image` group has Docker's names for these too: `image ls` (`images`),
`image rm` (`rmi`), `image pull`, `image push`, `image tag`, `image save`, `image load`,
`image history`, `image import`, `image build`, `image inspect` and `image prune`.

`tag SOURCE TARGET` gives an image another name: SOURCE is a reference or an ID,
TARGET a reference with a tag (a digest can't be given, as Docker's). The image
isn't copied; `rmi` of one name leaves the other. `image inspect` prints each image
as Docker's `docker image inspect` does, as one JSON array (`-f` a template, as
[`inspect`'s](containers.md#looking-closer-inspect)); one that isn't there is
said, and the command exits 1.

`shipyard images` lists them as Docker's `docker images` does (its classic table):
a row per name, newest first, the ID the manifest digest's first 12 hex digits.

```text
REPOSITORY   TAG       IMAGE ID       CREATED       SIZE
alpine       3.20      2c3e5c8f1a2b   2 weeks ago   3.62MB
```

`SIZE` is the compressed layers' sum (what was downloaded). `images alpine` lists
only that repository (`alpine:3.*` a pattern); `-q` the IDs; `--digests` adds a
`DIGEST` column; `--no-trunc` the whole ID; `-a` is taken (there are no
intermediate images). `-f` takes Docker's `reference` (a pattern, `*` and `?`),
`label` and `dangling`; `--format` its fields (`ID`, `Repository`, `Tag`,
`Digest`, `CreatedSince`, `CreatedAt`, `Size`, `SharedSize`, `UniqueSize`,
`Containers`; the last three are `N/A`), as [`ps`'s](containers.md#formats).

**Deduplication and garbage collection.** Identical layers shared by two images
are stored once. `rmi` removes a reference and reclaims only the blobs and layer
directories no *other* image still needs — a shared base layer survives removing
one of the images that use it. `image prune` removes the dangling images (images
with no name: rare in Shipyard, where removing a name that was an image's last
removes the image), then reclaims everything unreferenced and what interrupted
work left behind. With `-a` it also removes every image no container was made
from, running or not; the check is made again under the store's lock, so an image
a container is made from meanwhile is kept. It prints Docker's `Deleted Images:`
(`untagged: NAME`, then `deleted: sha256:…` for each blob) and `Total reclaimed
space:`. `--filter` takes `until`, `label` and `label!`, as [`container
prune`'s](containers.md#removing-whats-stopped-container-prune-system-prune). It
asks first, with Docker's warning, unless given `-f` (without a terminal to ask
on, it's refused: before 0.3.0 it never asked).

An image a [container](containers.md) was made from isn't removed while the
container exists, running or not (`rmi --force` removes its name; the container
keeps what it needs to start again, until it's removed): a foreground image run
is a container too, so its image stays until the container is removed (`run --rm`
removes it as it ends). Nor is one a run without a record is using (`rmi --force`
removes its name; its layers stay until the run ends and a prune collects them).
`image prune` never takes a container's own writable layer (`container prune`
and `rm` do). A prune or `rmi` never takes what a pull or a starting run is
using: it waits for them, up to 15 seconds, then says the store is busy.

`system df` shows what images, containers' writable layers and volumes take, and
how much of each nothing uses now (reclaimable), as Docker's: `TOTAL` how many,
`ACTIVE` how many are in use (images a container's layers use, containers
running, volumes a container names). A stopped container's layer counts in `SIZE`,
not as reclaimable (as Docker's: `rm`, or a `container prune`, reclaims it). There's no `Build Cache` row:
Shipyard builds no images yet.

```text
TYPE            TOTAL     ACTIVE    SIZE      RECLAIMABLE
Images          2         1         43.3MB    4.09MB (9%)
Containers      3         1         12.29kB   0B (0%)
Local Volumes   1         1         2.2MB     0B (0%)
```

`--format` takes Docker's fields (`Type`, `TotalCount`, `Active`, `Size`,
`Reclaimable`), as [`ps`'s](containers.md#formats).

Rootless, some directories are your subordinate IDs' (an image's other users), and
can't all be read: a size then shows as `≥`, at least that much (in `--format`'s
values too, where Docker's never have it). `system df` reads the image store as it
is; `info`'s `Images` counts as its `TOTAL` does (each image once, however many names
it has).

`rmi` takes several references; one it can't remove is reported, the rest are still
removed, and the command exits 1. It takes an image's ID too: its manifest's
digest, whole or its first 4 hex digits or more (as `IMAGE ID` shows it), or its
config's, whole. That removes every reference of the image, all of them or none; one
with references in more than one repository is refused unless `--force`, as Docker's
`rmi` does.

## Saving and loading (`save`, `load`)

```sh
shipyard save -o images.tar alpine:3.20 app:1   # images to a file
shipyard save alpine | gzip > alpine.tar.gz     # every alpine tag, to standard output
shipyard load -i images.tar                     # back, here or on another host
ssh build-host shipyard save app:1 | shipyard load   # straight across
```

`save` writes the images as one tar archive, as Docker 25 writes one: an OCI image
layout (`oci-layout`, `index.json`, each blob at `blobs/sha256/<hex>`) with Docker's
`manifest.json` beside it, so Docker, Podman and containerd load it too. Each
image's own blobs go out as they're stored (its manifest, config and compressed
layers): it keeps its ID and digests, a layer two images share goes once, and the
same images give the same bytes. A name with a tag is that image; a repository
alone (`alpine`) is every tag of it in the store; an ID is the image with no name
in the archive (as Docker's). `-o FILE` writes it beside FILE and renames it at the
end, so a failed save leaves no half archive (one killed half way leaves its
`FILE.shipyard-PID` beside it), and the file is yours alone (mode 0600, as Docker's:
an image may hold secrets); `-o` naming a device or a FIFO
(`/dev/null`, a named pipe) writes into it, never replaces it. Without `-o` it goes
to standard output, never to a terminal. Each blob is held open until it's written:
`save` raises its descriptor limit as far as it may, and a save of more blobs than
that is refused before it writes anything (save fewer images at once, or raise
`ulimit -n`).

`load` reads an archive from standard input, or `-i FILE`: Docker's (`docker save`,
both the current form and the older one with a directory a layer), an OCI layout
alone (BuildKit's `--output type=oci`, named by its `io.containerd.image.name`
annotations), plain or gzipped. It says each name it loaded (`-q` leaves out the
tags it moved):

```text
Loaded image: alpine:3.20
```

An archive is anyone's, and `load` treats it so:

- **Nothing is extracted by its names.** Members are matched to blobs and documents
  by name, and a blob is stored only if its bytes are its digest. A link is a name
  too, never followed on disk; a member whose name climbs (`../`) refuses the whole
  archive.
- **Nothing reaches the store until the whole archive is read and sound.** It's read
  first into a staging directory in the store (`blobs/.load-…`, removed at the end;
  one a killed load left is removed by the next load, `rmi` or `image prune`). An archive cut
  short, or missing a layer its image needs, loads nothing; nothing is ever fetched
  from a registry for it.
- **A name moves, and says so.** A name the store has for another image moves to
  the archive's, as a pull's does, and `load` says which: `The image app:1 already
  exists, renaming the old one with ID sha256:… to empty string` (Docker's words).
  The old image keeps its other names; a container made from it keeps it.
- **An image with no name isn't loaded** (an archive saved by ID): it's said, and
  the rest load. Give it a name when saving (`save NAME`).
- **Only this host's platform loads;** another is left out, said. A name with a
  digest (`alpine@sha256:…`) loads only if it's the image's own.
- **Bounds:** 64 GiB decoded, 16 MiB of documents, 1024 names, 1024 entries in
  `manifest.json` or an index, 128 layers an image; the store's free space reserve is
  kept. bzip2, xz and zstd archives are refused by name.

An image from Docker's older archive form gets a manifest of its own on loading (its
layers as they were), so its ID differs from the one Docker showed for it; its
layers and config are the same.

## Building one (`build`)

```sh
shipyard build -t app:1 .                       # ./Dockerfile, the image named app:1
shipyard build -t app:1 -f ci/Dockerfile --build-arg VERSION=2 --target final .
shipyard build -t app:1 --secret id=npm,src=$HOME/.npmrc .
shipyard build -o type=local,dest=dist .        # the build's files, no image
shipyard builder prune -f                       # the build cache, emptied
```

A Dockerfile is built by [BuildKit](https://github.com/moby/buildkit) (Docker's own
builder: every Dockerfile feature it has, multi-stage builds, `RUN --mount=type=cache`,
`--mount=type=secret`), run by Shipyard for the build in a container of its own
(`shipyard-build-…`, gone at the build's end) from `moby/buildkit:v0.34.1`, pinned by
its digest (pulled on the first build; a `load` of it serves a host offline). The image
is loaded under the build's names alone (`-t`, at least one: Shipyard keeps no untagged
images), its ID printed with `-q`.

The flags, as Docker's: `-t NAME[:TAG]` (repeat it), `-f FILE` (`-f -` reads it from
standard input; a `FILE.dockerignore` beside it is used), `--build-arg KEY=VALUE` (or
`KEY`, this shell's value), `--target STAGE`, `--no-cache`, `--pull`, `--label`,
`-q`, `--progress auto|plain|quiet` (`tty` taken, as `plain`), `--platform` (this host's only), `--secret
id=ID,src=FILE` (at most 1 MiB; reaches `RUN --mount=type=secret,id=ID` and no
layer), `-o type=local,dest=DIR` (or `-o DIR`) and `-o type=tar,dest=FILE` (`-` for
standard output): the build's files, owned by you, instead of an image (`-t` and `-o`
together are refused: one or the other; a `type=tar` file is yours alone, mode 0600, as
`save -o`'s). As root, `-o type=local`'s directory is reached without following a
symbolic link (another user's link in `/tmp` could lead the build's files anywhere): give
its real path. The context is
a directory (from standard input or a URL isn't supported). On a Mac, the context and
every file named are in your home, as `cp`'s.

**The store's images are built on.** As Docker's builder does, a `FROM`, `COPY --from`
or `RUN --mount=from=` that names an image Shipyard has (one you built, pulled, loaded
or imported) uses it, and asks no registry: `shipyard build -t base .` then `FROM base`
works offline. `--pull` asks the registry for each base image again, as Docker's flag
does (a name only this host has then fails). BuildKit is given each image of the store
whose name the Dockerfile or a `--build-arg` mentions (by any of its names, or its
digest; at most 256), its blobs linked from the store read-only, never copied (unless
the builder's directory is on another filesystem than the store).

**What the builder is given.** It runs in a user namespace (rootless: yours; as root,
IDs from 2100000000, a range no user is given: a build as root is refused if
`/etc/subuid` or `/etc/subgid` gives anyone part of it) with `CAP_SYS_ADMIN` in that
namespace alone, and **seccomp unconfined** (BuildKit needs the kernel's keyring there):
a wider set of system calls than a container gets, so build only Dockerfiles you'd run.
BuildKit runs a build's `RUN` steps without a sandbox of their own: they see the
builder's processes (and `buildctl`'s arguments: build args are recorded in the image
anyway, as Docker's; secrets go in `--secret`, never in args), not its files. It sees
the context read-only, its own cache, the build's Dockerfile and secrets (copied in,
each apart, removed after, a Ctrl-C included) and where it writes its result. Its
network is the default one (base images are pulled; as root, the default bridge, so a
build's steps reach what a container on it reaches). The Dockerfile and its own
`.dockerignore` are read from inside the context, or `-f`'s own directory: one that's a
link leading anywhere else is refused (a repository's link could hand BuildKit any file
of yours).

**The context must be readable to the builder, whole.** BuildKit would copy a file it
can't read as an empty one, so `build` refuses one first, by name, `.dockerignore` or
not: as root, every file must be readable to every user (`chmod o+r`, a directory
`o+rx`), the context's own directory too and every directory above it enterable
(`o+x`: not under `/root`), since the builder is another user; rootless, every file must
be readable to you. On a Mac, the home the VM shares is read by the builder as you
read it, so nothing there needs opening to others. Rootless builds need a delegated
cgroup (a login session's), as rootless containers do.

**The cache is the context's.** Each context directory has a cache of its own
(BuildKit's state, in the store, `builder/state-…`; as root beside it, in
`images-builder/`, which must be root's and writable by no one else, or a build refuses
it): a project's builds reuse it, and nothing crosses projects (a
hostile Dockerfile can affect its own project's later builds, never another's).
Builds of one context run one at a time (another waits, said); of different contexts,
at once. BuildKit keeps each under about 10 GB, and a context's cache no build has used
for a week is removed by the next build of another context (said), so a host that
builds in a new directory each time, as CI does, doesn't keep every one. `system df` counts it (`Build Cache`; one
a build is using counts as active), and `builder prune -f` (or `system prune`, or the
API's `/build/prune`) empties every one no build is using, with what killed builds left.
A prune with filters (`system prune --filter …`, `/build/prune?filters=…`) leaves it: a
cache has no labels or times to read.

A build that fails says BuildKit's last lines and exits 1; one interrupted (Ctrl-C)
removes its builder; one whose `shipyard` was killed leaves its builder running until
the next build of that context, or `builder prune` (with the cache it's using), removes
it. `container prune` and `system prune` leave a build's builder to it.

## A filesystem made an image (`import`)

```sh
shipyard export web | shipyard import - web-snapshot:1
shipyard import -c 'CMD ["/bin/app"]' -c 'ENV MODE=prod' rootfs.tar.gz app:1
```

As `docker import`: a root filesystem's tar archive (a file, or `-` for standard
input; plain or gzipped) made an image of one layer, named `REPOSITORY[:TAG]`, its
ID printed. **A name is required:** Shipyard keeps no untagged image (Docker would
make a `<none>` one). A URL isn't fetched: download it, then import the file. Only
this host's platform is made (`--platform` this host's or refused).

`-c`/`--change` sets the image's config as a Dockerfile's instruction would: `CMD`
and `ENTRYPOINT` (a JSON array, or a line run by `/bin/sh -c`), `ENV` and `LABEL`
(`KEY=VALUE` pairs, quoted as a Dockerfile's, or the older `ENV KEY value`),
`EXPOSE` (`80`, `53/udp`, a range of at most 1024 ports), `USER`, `VOLUME`,
`WORKDIR` and `STOPSIGNAL`; repeat it for more (64 at most). Docker's `ONBUILD` and
`HEALTHCHECK` aren't taken. `-m`/`--message` is its history's comment.

The archive is read whole before anything is kept: an empty or cut one, or one that
isn't a tar, makes nothing (16 GiB decoded at most; bzip2, xz and zstd refused by
name). Its files are checked as any layer's are when it's unpacked (no `..`, no
device nodes). The layer is stored gzipped (its digest is the gzip's, its `diff_id`
the archive's own). An image archive (`save`'s) given to `import` by mistake is
warned of: `load` reads those. A name that named another image moves to the new one,
said.

## How an image was built (`history`)

```text
$ shipyard history app:1
IMAGE          CREATED        CREATED BY                                      SIZE      COMMENT
2c3e5c8f1a2b   2 weeks ago    /bin/sh -c #(nop)  CMD ["sh"]                   0B
<missing>      2 weeks ago    /bin/sh -c #(nop) ADD file:1f4a… in /           3.62MB
```

A row per step of the image's build, newest first, from its config's `history`;
an image whose config has none shows a row a layer. The newest row has the image's
ID, the rest `<missing>` (as Docker shows an image it didn't build). `SIZE` is the
step's layer as stored (compressed, as `images`' `SIZE`; Docker's is unpacked).
`--no-trunc` shows whole IDs and instructions, `-H=false` bytes and RFC 3339 times,
`-q` the IDs; `--format` takes Docker's fields (`ID`, `CreatedSince`, `CreatedAt`,
`CreatedBy`, `Size`, `Comment`).

## Where the store lives

| Who runs it | Store directory |
|---|---|
| root | `/var/lib/shipyard/images` |
| anyone else | `$XDG_DATA_HOME/shipyard/images`, or `~/.local/share/shipyard/images` |

Set `SHIPYARD_IMAGE_STORE` to an absolute path to override it (a relative one is
refused) — useful where the default is on an overlay filesystem, which can't hold an
image's writable layer. The store keeps
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
[the stage 6 notes](https://github.com/Zena4L/shipyard/blob/main/docs/stages/06-images.md).

**One image can't change another's files.** Images that share a layer share
its extracted files, but a layer whose entries leave out a directory they're in
(`usr/local/bin/tool` with no `usr/local/` entry) takes that directory's owner
and permissions from the layers below it, so it's extracted again for each set
of layers below it.

**Limits.** Extracting stops while the store's filesystem still has 5 % of its
space (at least 1 GiB, at most 10 GiB) and 5 % of its inodes (at most a million)
free, whatever the layer holds: a layer of millions of empty files is stopped as
surely as a large one. An image may have at most 128 layers (Docker builds at most
127); one with more is refused before anything is downloaded.

**Interrupted downloads go on where they stopped.** A layer that's cut off, by
the network or by stopping the pull, is kept in the store as a partial download,
and the next attempt (or the next pull) asks the registry for only the rest. A
registry that can't send part of a blob sends it all again. What was kept is
checked like everything else, so a damaged partial download is simply fetched
again. `shipyard pull` (or `--pull always`) also re-checks the layers the store
already has, and fetches again any that a damaged disk changed. `image prune`
removes partial downloads.

## Not yet

- Signature verification (cosign/sigstore); pushing a multi-platform index; a blob
  mounted from another repository of the registry (each is uploaded); an upload resumed
  where it stopped (a broken one starts over).
- Untagged images (a `build`, `load` or `import` without a name is refused).
- `docker build` through the API (use `shipyard build`), a build context from standard
  input or a URL, `--ssh`, `--cache-from`/`--cache-to`, other platforms.
- zstd-compressed layers (gzip works today; a zstd layer is refused by name).
- A fallback to `fuse-overlayfs` where the kernel won't let a normal user mount
  overlay.

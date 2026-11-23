# Threat model

What Shipyard protects, from whom, by which boundary, and what it leaves to you. It's
the map behind [SECURITY.md](https://github.com/Zena4L/shipyard/blob/main/SECURITY.md),
which says how to report a vulnerability. Each boundary named here has its tests (the
gaps are recorded as findings); what isn't defended yet is listed at the end, each with
its reason.

In one paragraph: a container is confined by Linux's namespaces, a minimal set of
capabilities, `no_new_privs` and Docker's seccomp profile, and, on Linux for anyone but
root, a user namespace whose root is the user who ran it. Shipyard runs no daemon:
nothing of it runs while it isn't used but each container's monitor (and, for containers
on a network, their DNS server or pasta). Its Docker API socket takes connections from
its own user alone. On a Mac, containers run in a Linux VM of Shipyard's own, **as root
in that VM**, which shares your home and a few folders with them. **Run only code you'd
run as your own user (rootless on Linux), or trust with root (as root, and on a Mac with
the VM and what it shares).**

## What it protects

- **The host:** its files, processes, binaries (Shipyard's own included), network and
  firewall, and its process table from a container's runaway use (2,048 processes by
  default). Its memory, CPU and disk only as far as you limit them (below).
- **Other users on the host:** their files, sockets and containers. On a Mac, not the
  ports you publish: the Mac's loopback is every user's.
- **Other containers:** their network identity, their images' files, their limits.
- **Your Mac's files outside what the VM shares**, and its network from other machines.
- **Images, as their digests say, and your registry credentials:** a credential goes only
  to the registry it's for.

## From whom, and what stops them

### A process in a container

It's confined, whoever runs it ([Isolation](run.md#isolation)):

- Its own mount, PID, UTS, IPC, network and cgroup namespaces; `pivot_root` into its
  root, with `/proc` and `/sys` masked and read-only where Docker's are. A mount's
  destination is resolved inside the root: a link or a `..` in the image never leads it
  out, nor into `/proc`, `/sys` or `/dev`.
- **On Linux, for anyone but root, a user namespace** whose root is you
  ([Rootless](run.md#rootless)): root inside holds no power over the host. Root's
  containers get one only with `--uidmap`; without it, root inside is root on the host's
  files the container is given. On a Mac, every container is root's in the VM.
- Docker's 14 [capabilities](run.md#capabilities-and-privileges) and no more,
  `no_new_privs` on, and Docker's [seccomp profile](run.md#system-calls-seccomp) (32-bit
  and x32 calls killed). The init keeps only `CAP_KILL`.
- It can't overwrite the `shipyard` the host runs next (the init runs from a sealed
  copy). It can't use a device it isn't given: an eBPF device program checks every open
  (it may make a node with `mknod`, and can't open it); where there's no such program,
  `CAP_MKNOD` is dropped. It can't join a PID namespace where any process may trace it as
  it's set up.
- Its network traffic goes out only as itself (an anti-spoofing table on every bridge),
  and nothing outside the host reaches it but through a port it publishes
  ([The way out](networking.md#the-way-out-and-what-gets-in)).
- At most 2,048 processes by default, where the `pids` controller reaches its cgroup.

### An image

Every layer is checked against its digest and unpacked so no entry leaves its layer
(fuzzed); a layer shared between images is never one another image's writes can reach;
a store keeps its reserve of space and inodes. `load` reads an archive's members by
name and never extracts them, bounded at every count. See
[How a pull is kept honest](images.md#how-a-pull-is-kept-honest).

### A registry

Digests are checked, documents bounded, and its parsers fuzzed. Credentials go only to
the registry and its token service: a push follows no redirect, and continues an upload
only at a location on the registry itself. Plain HTTP is spoken only to a registry on
the loopback.

### Another user on the host

Each user's runtime state and Docker API socket are theirs alone (a socket 0600 in a
0700 directory, and every connection's user checked, `SO_PEERCRED`, before a byte is
read): [Who can use it](api.md#who-can-use-it). The lock files that say whether
something lives are out of other users' reach (with two exceptions: F59, F77 below).
Root's image and volume stores are root's alone. BuildKit's builder, as root, runs as
IDs no user is given (checked against `/etc/subuid` at each build).

### The network

Nothing reaches a container but a port it publishes. When Shipyard turns forwarding on,
the host doesn't start routing between the interfaces it already had (one added later
is: F111 below). On a Mac, a published port listens on the Mac's loopback only, and the
VM's firewall takes nothing new on its own address on the Mac's virtual network: other
VMs there reach no published port, no `--network host` container and none of the VM's
services ([Ports](macos.md#ports)).

### A client of Docker's API

Only the socket's own user connects, so a client is that user already. Each request is
read as hostile (its parser fuzzed); every part of a create goes through the CLI's own
checks, and what Shipyard can't do is refused, never half-done
([Creating containers](api.md#creating-containers)).

### A compose file, or an OCI bundle

Compose is Docker's own `docker compose`, against the API, so it gets the API's checks.
A bundle's config is read as hostile input; anything in it Shipyard can't apply, and
that would leave the container less confined, is refused
([What's refused](bundles.md#whats-refused)). A bundle's hooks run with your power: a
bundle is a program you chose to run.

### What you download

Each release's files are attested by the workflow that built them, from their tag; the
Linux binaries are built twice, on two machines, and the Mac's guest twice, each
compared byte for byte before anything is published; SBOMs list what's in them. The
workflow's actions and tools are pinned. The release notes give the check.

## What you trust it with, by design

These are your choices, and Shipyard doesn't defend against them:

- **Docker's API socket is its user.** Root's is root-equivalent. Never loosen its mode,
  or hand its directory to a group: Docker's `docker` group is root.
- **Root's containers, without `--uidmap`,** are root on what they're given.
  `--no-image` gives one the host's own files (said when it runs). A `--uidmap` onto a
  range a user owns (on Ubuntu, Debian and Fedora the first user gets `100000:65536`)
  makes that user root of the container.
- **What you grant a container:** `--cap-add` (`SYS_ADMIN`, `ALL`), `--security-opt
  seccomp=unconfined` or `no-new-privileges=false`, `--device` and
  `--device-cgroup-rule`, root's `--oom-score-adj` down to -1000 (the kernel's
  out-of-memory killer then picks the host's services first), a bind mount (followed as
  Docker follows it), `--network host`, `--pid host` (root only), and joining another
  container's network, IPC or PID namespace (rootless, the joiner shares its user
  namespace). `--privileged` itself is refused: give the capabilities and devices it
  needs.
- **A container given Docker's socket** (`-v /var/run/docker.sock:…`) runs containers as
  that socket's user: on a Mac, as root in the VM, with every share.
- **On a Mac, the VM and its containers** are root's in it, and the VM can read and write
  what's shared: your home, `/Volumes`, `/tmp` and `/var/folders`. A container sees only
  what you bind into it, and a bind outside those is refused; but one given `SYS_ADMIN`
  or a device of the VM's can reach a share whole ([Your files](macos.md#your-files)).
  Containers reach your Mac's own services at the VM's gateway (`192.168.64.1`), and
  other users of your Mac reach the ports you publish (its loopback is theirs too).
- **The default network, `bridge`,** reaches the host's own services and a cloud's
  metadata address (`169.254.169.254`), as Docker's does. A network made with
  `--isolate-host` doesn't.
- **Registries and tags:** a tag is whatever the registry names, and images carry no
  signature Shipyard checks (yet): pin an image by its digest to get exactly that one.
- **Volumes** keep the files an image put in them, set-user-ID ones too.
- **BuildKit's builder** runs with seccomp unconfined and `SYS_ADMIN`, in a user
  namespace of its own: build only Dockerfiles you'd run.
- **Shipyard's AppArmor profile** isn't a boundary for containers: it lets users who can
  run Shipyard make user namespaces where Ubuntu restricts them (what rootless needs), as
  Podman's does. Containers get no AppArmor or SELinux label of their own.
- **Not limited by default, as with Docker:** a container's memory (`--memory`), its CPU
  and I/O (`--cpus`, `--blkio-weight`…), its disk (a writable layer or a volume has no
  quota), and the host's connection-tracking table (`--conntrack-limit` on a network);
  and a container's output reaches your terminal as it is, control sequences too.

## Not defended yet

Each is recorded in the roadmap's
[accepted findings](https://github.com/Zena4L/shipyard/blob/main/docs/ROADMAP.md#accepted-at-v10-151-2026-10-31),
with why it's accepted at 1.0. The ones that touch security:

- **F24:** a bundle's bind source swapped for a link between its check and the start
  (only the bundle's author can; runc has the same gap).
- **F31:** residuals needing root or a rare race: link-local IPv6 isn't tied to its
  container, a store root that's a link is only warned about, and a device node steered
  by a link isn't checked yet.
- **F59:** the DNS server runs as `nobody`, which other `nobody` processes share: one
  can hold a lease's lock, and read containers' names and addresses.
- **F62:** no `--isolate-host` on the default bridge (as Docker).
- **F77:** without a runtime directory, the port reservation lock is a name any user can
  take: a port can be said to be in use wrongly, or, in a race between two users, root's
  published port can shadow a user's, its traffic reaching root's container.
- **F111:** an interface that appears after Shipyard turned forwarding on (a VPN,
  another engine's bridge) can be routed to and from.
- **F142, F149:** no cap on the API's connections or a request body's time (only its own
  user connects).
- **F153:** a link above a bind source Shipyard makes is followed (as Docker).
- **F161:** root's builds share one range of IDs, so one build can starve another.
- **F173, F180:** on a Mac, an older guest binds its own `/tmp` (also through Docker's
  API), `/Volumes` reaches Time Machine's local snapshots, a container with `SYS_ADMIN`
  reaches shares, and the local signing identity lets any program of yours sign as
  Shipyard (adding no reach: they reach the VM through `docker.sock` already).
- **F185, F186:** on a Mac, a local process evicting a UDP peer, and a buggy caller
  stepping the guest's clock back.
- **F210:** before Linux 5.12, a mount covered below a read-only bind stays writable
  (reaching it needs `CAP_SYS_ADMIN`).

And these known limits: with a single-ID map, a container keeps your supplementary
groups; a firewall another program flushes stays open until Shipyard's next run; a host
without bridge filtering in nf_tables has no anti-spoofing (`run` warns); on cgroup v1
there are no limits or device control.

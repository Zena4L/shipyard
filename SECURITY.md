# Security

## Reporting a vulnerability

Please report vulnerabilities **privately**, through GitHub's private vulnerability
reporting: the repository's **Security** tab, then **Report a vulnerability**. Don't
open a public issue for one.

Say what an attacker can do, from where (a container, an image, a registry, another
local user, the network), and how to reproduce it. We aim to answer within a week.
A fix is made on a private branch and released with a note in the
[changelog](CHANGELOG.md), crediting you unless you'd rather not be named.

## Supported versions

Shipyard is before 1.0: only the latest release gets fixes.

## What Shipyard defends, and what it doesn't yet

Shipyard (0.5, an internal milestone before 1.0) confines containers, but **isn't a
hardened security boundary yet**. Run
only code you would run as your own user (rootless) or trust with root (as root).

What a container is kept from, and tested for:

- **The host's files and processes:** its own mount, PID, UTS, IPC, network and cgroup
  namespaces, and a user namespace for rootless runs (and root's `--uidmap`);
  `pivot_root` into its root filesystem, with `/proc` and `/sys` masked and read-only
  where Docker's are; mounts never followed through a symlink in the image.
- **Privileges:** a minimal capability set, `no_new_privs`, and a seccomp filter
  (Docker's profile); rootless, root inside is the user outside. The init runs from a
  sealed copy of the binary, so a container can't overwrite the one the host runs next.
- **Devices:** an eBPF device program where cgroups allow; `CAP_MKNOD` dropped where
  they don't.
- **Other containers:** a container sends network traffic only as itself, can't change
  another image's files, and gets its own cgroup and limits.
- **Hostile input:** image layers are verified against their digests and unpacked so
  no entry escapes its layer; manifests, tar, netlink and DNS parsers are fuzzed;
  registry credentials go only to the registry and its token service.

What a container isn't kept from by default (as with Docker):

- **The host's memory running out:** there's no default limit on a container's
  memory; give `--memory` to one you don't trust. Its processes are limited to 2,048
  by default where the `pids` controller reaches its cgroup (as root; rootless where
  it's delegated: `shipyard doctor` says), so a fork bomb can't take the host's whole
  PID space; give a lower `--pids-limit` to one you don't trust.
- **The host's services and the cloud's metadata:** a container on the default
  network, `bridge`, can reach the host's own addresses and `169.254.169.254`; on a
  cloud VM that can be the instance's credentials. A network made with
  `--isolate-host` keeps its containers from both.
- **The container whose network it joins** (`--network container:NAME`): the two share
  a network, so each sees the other's traffic and ports. Rootless, the one that joins
  shares the other's user namespace too, whose root holds privileges over the joiner's
  own namespaces (its mounts, processes, names): join only a container you'd trust
  with yours, as with Docker.

**Docker's API socket is root, or you:** whoever can connect to the socket of
`shipyard system service` runs containers as its user, as with Docker's own socket:
root's (`/run/shipyard/api.sock`, the systemd units' `shipyard.socket`) is
root-equivalent, a user's (`$XDG_RUNTIME_DIR/shipyard/api.sock`) is that user. Each
socket is its user's alone (mode 0600 in a 0700 directory), and the service checks
every connection's user (`SO_PEERCRED`) before it reads a byte. Don't loosen the
socket's mode or hand its directory to a group, as you would for Docker's `docker`
group: that group is root.

**Volumes hold what images put there:** a volume filled from an image keeps the
image's files as they are, set-user-ID ones too. The volume store is root's alone
(0700) as root, so no other user can reach them.

**The AppArmor profile** (`contrib/apparmor/shipyard`) lets anyone who can run
Shipyard's binaries make user namespaces through them, where Ubuntu otherwise
restricts them: the kernel code user namespaces expose is then within those users'
reach, as with Podman's profile. Install it only where that's what you want, which it
is wherever rootless containers are.

**On a Mac**, containers run in Shipyard's own Linux VM, and the VM is part of what
you trust:

- **Your files, shared:** the VM can read and write your whole home, `/Volumes`, `/tmp`
  and `/var/folders` (Docker Desktop's set), so a container that escaped into the VM
  could too. A container sees only what you bind into it, and a bind of anything else
  (the VM's own files among them, `-v /:/host` too) is refused as Docker Desktop's
  "mounts denied", by the `shipyard` CLI and through Docker's socket on the Mac
  (`docker.sock`, `shipyard vm install`) alike. That socket reaches the VM's Docker API
  as root in the VM. A container given `--cap-add SYS_ADMIN` (or `--privileged`, or a
  `--device` of the VM's own) isn't held by that: it can mount a share whole, or uncover
  the VM's own files in one, as on Docker Desktop. Give those only to containers you
  trust with the VM and every share. It's yours alone (mode 0600, your user checked), as Docker
  Desktop's is. Set-user-ID bits and device files in your home mean nothing in
  the VM, nor in a container you bind them into (a file copied into a container with
  `shipyard cp` keeps its mode, as on Linux). The VM's own files (its disk, its
  socket) are hidden from a bind of your home.
- **Ports:** a published port listens on your Mac's loopback only, never on its
  network, and the VM's firewall refuses it on the VM's own address on the Mac's
  private virtual network (`192.168.64.x`), so other VMs on that network don't reach
  it. What isn't a published port isn't covered there: a container on
  `--network host` listening in the VM answers on that address, as a port published
  on one of the VM's addresses reaches its holder's socket (a TCP reset; UDP waits
  unread).
- **The Mac, from a container:** a container can reach your Mac's own services at the
  VM's gateway (`192.168.64.1`), as it can a Linux host's.
- **The VM's agent** answers only the Mac (vsock's host, CID 2). Its port is held by
  the VM's init from its boot to its end, so a container that can open vsock sockets
  (one started with `--security-opt seccomp=unconfined`; the default profile refuses
  them) can never take it, even while the agent restarts. A command that waited
  longer than a minute for a restarting agent is refused rather than run late.
- **Your local signing identity** (`scripts/macos/make-signing-identity.sh`) lets
  `codesign` use its key without asking, so any program you run can sign with it, and
  carry the folder permissions macOS gave `shipyard` (Desktop, Documents, Downloads) to
  another binary. A program you run can already reach those folders through the VM
  (`docker.sock` is yours), so this adds no reach; it's a key for this Mac's
  development, never for anyone else's (F180).

What isn't done yet (tracked in the [roadmap](docs/ROADMAP.md#open-findings)):

- an independent security review, and the OCI runtime conformance suite;
- testing on more distributions and security modules (SELinux; AppArmor beyond
  Ubuntu 24.04);
- the items in the roadmap's open findings, each with its severity and home.

The engineering record of what was defended and how is in
[`docs/stages/`](docs/stages/), in particular
[the hardening notes](docs/stages/07.5-hardening.md).

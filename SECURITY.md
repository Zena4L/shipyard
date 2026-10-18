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

Shipyard (0.3, an internal milestone before 1.0) confines containers, but **isn't a
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

- **The host's processes and memory running out:** there's no default limit on a
  container's processes or memory; give `--pids-limit` and `--memory` to one you
  don't trust.
- **The host's services and the cloud's metadata:** a container on the default
  network, `bridge`, can reach the host's own addresses and `169.254.169.254`; on a
  cloud VM that can be the instance's credentials. A network made with
  `--isolate-host` keeps its containers from both.

**The AppArmor profile** (`contrib/apparmor/shipyard`) lets anyone who can run
Shipyard's binaries make user namespaces through them, where Ubuntu otherwise
restricts them: the kernel code user namespaces expose is then within those users'
reach, as with Podman's profile. Install it only where that's what you want, which it
is wherever rootless containers are.

**On a Mac**, containers run in Shipyard's own Linux VM, and the VM is part of what
you trust:

- **Your home, shared:** the VM can read and write your whole home (as Docker
  Desktop's file sharing can), so a container that escaped into the VM could too. A
  container sees only what you bind into it, and a path outside your home is refused
  before anything runs. Set-user-ID bits and device files in your home mean nothing in
  the VM, nor in a container you bind them into (a file copied into a container with
  `shipyard cp` keeps its mode, as on Linux). The VM's own files (its disk, its
  socket) are hidden from a bind of your home.
- **Ports:** a published port listens on your Mac's loopback only, never on its
  network. One published on every address also answers on the VM's address on the
  Mac's private virtual network (`192.168.64.x`), which other VMs on your Mac using
  that shared network can reach.
- **The Mac, from a container:** a container can reach your Mac's own services at the
  VM's gateway (`192.168.64.1`), as it can a Linux host's.
- **The VM's agent** answers only the Mac (vsock's host, CID 2). A container started
  with `--security-opt seccomp=unconfined` can open vsock sockets (the default profile
  refuses them), and could take the agent's port while the agent is down.

What isn't done yet (tracked in the [roadmap](docs/ROADMAP.md#open-findings)):

- an independent security review, and the OCI runtime conformance suite;
- testing on more distributions and security modules (SELinux; AppArmor beyond
  Ubuntu 24.04);
- the items in the roadmap's open findings, each with its severity and home.

The engineering record of what was defended and how is in
[`docs/stages/`](docs/stages/), in particular
[the hardening notes](docs/stages/07.5-hardening.md).

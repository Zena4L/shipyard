# Shipyard documentation

Shipyard is a lightweight, daemonless container engine for macOS and Linux.

> **Status: early development.** On Linux, `shipyard run` starts a command in
> its own namespaces: its own processes (under a small init), mounts, hostname
> and IPC, and optionally a root filesystem of its own (`--rootfs`), or a whole
> container from an [OCI bundle](bundles.md) (`--bundle`); `shipyard-runtime` runs
> bundles step by step, as an [OCI runtime](runtime.md). The command is confined:
> a minimal set of capabilities, `no_new_privs`, and a seccomp filter. It runs
> **rootless** for anyone without root, in a user namespace whose root is that user
> (root can ask for one with `--uidmap`). It can run a **pulled image**
> (`shipyard run IMAGE`), stacking its layers into a root filesystem with overlayfs, and mount
> **volumes** (`--volume`). Containers are on a **network** of their own with
> a way out, and can **publish ports** (`-p`): a bridge network as root, pasta
> without; on a network you make, they find each other **by name** (`--name`),
> a container can be on **several networks** at once, and with `--ipv6` they have
> IPv6 too. With `-t`, a container gets a **terminal of its own** (`-it` to type into
> it: job control, full-screen programs, password prompts); it never gets yours.
> **Containers** run in the background (`run -d`, `create` and `start`), outlive the
> command that started them, and are found **by name**: listed (`ps`, `inspect`),
> stopped and started again (`stop`, `kill`, `restart`, `wait`), renamed, and removed
> (`rm`); what one writes is kept until it's removed. You can read a container's
> **logs**, **attach** to it, **exec** into it and **cp** files in and out by its name, and
> connect it to a network while it runs. **Restart policies** (`--restart`) bring a
> container back when it ends, and after a reboot with the [systemd
> units](https://github.com/Zena4L/shipyard/tree/main/contrib/systemd); **health
> checks** (`--health-cmd`, or the image's own) show in `ps`; and `shipyard events`
> tells what happened. **Docker's own clients** drive it: Shipyard serves a subset of
> Docker's Engine API ([API](api.md)), so the `docker` CLI, `docker compose`
> ([Compose](compose.md)) and Testcontainers work against it, root's and rootless, and
> `shipyard` takes Docker's everyday commands and flags itself.
> It isn't a hardened boundary yet. It starts the command the way a container
> runtime does (clean environment, chosen user, working directory and
> resource limits), supervises it (passing signals on, stopping it gracefully, cleaning up what it
> leaves behind), and reports exactly how it ended. On an Apple Silicon Mac, commands
> run in a Linux VM of Shipyard's own ([macOS](macos.md)). These docs describe only what works in the current version, and
> they are updated with every release.

## Pages

| Page | What it covers |
|---|---|
| [macOS](macos.md) | Shipyard on a Mac: its Linux VM, your files, input, signals and variables |
| [Running commands](run.md) | `shipyard run`: isolation, environment, users, working directory, resource limits, stopping |
| [Containers](containers.md) | `run -d`, `create`, `start`, `ps`, `inspect`, `rm`: containers that outlive the command, by name, ID or serial; their kept files |
| [Images](images.md) | `shipyard pull`, `run IMAGE`, `images`, `rmi`, `image prune`, `system df`, `system prune`: pulling, private registries and the local store |
| [Networking](networking.md) | `--network`, `-p`, `--name`, `shipyard network …`: networks, addresses, names, published ports, other firewalls, the hostname and the `/etc` files |
| [The Docker API](api.md) | `shipyard system service`: Docker's Engine API (a subset) on a socket, for Docker's own clients (`DOCKER_HOST`); the systemd units that start it |
| [Compose](compose.md) | `shipyard compose`: Docker's own `docker compose` against Shipyard, what works and what doesn't yet |
| [Events](events.md) | `shipyard events`: what happens to containers, images, volumes and networks, as it happens or since a time, filtered |
| [Volumes](volumes.md) | `shipyard volume …` and `--volume`: named and anonymous volumes, bind mounts, tmpfs |
| [OCI bundles](bundles.md) | `shipyard run --bundle`: a container described by a `config.json`, what's supported, refused and ignored |
| [The OCI runtime](runtime.md) | `shipyard-runtime`, as `runc`: a container's life one step at a time (`create`, `start`, `state`, `kill`, `delete`), its terminals, logs, `attach`, `exec`, `cp`, passing descriptors, and its cgroup (`pause`, `update`, `ps`, `events`) |
| [Runtime reference](runtime-cli.md) | Every `shipyard-runtime` command and flag, generated from the binary itself |
| [Exit codes](exit-codes.md) | What each exit code means, for scripts that check them |
| [Command reference](cli.md) | Every command and flag, generated from the CLI itself |

## Requirements

On Linux:

- **Linux 5.11 or later** (6.8 or later for root's `--uidmap` with an image), with
  **cgroup v2** (every current distribution).
- **For rootless containers** (running without root, the default for a normal user):
  - `uidmap` (`newuidmap` and `newgidmap`) and a range of subordinate IDs for you in
    `/etc/subuid` and `/etc/subgid` (most distributions give new users one; see
    [Rootless](run.md#rootless));
  - `passt`, for the container's network (pasta);
  - user namespaces allowed: on Ubuntu 23.10 and later, Shipyard's AppArmor profile
    ([below](#ubuntu-2310-and-later-apparmor));
  - for resource limits (`--memory`, `--cpus`, …), a cgroup subtree delegated to you:
    a desktop session has one; from an SSH login, run Shipyard under your user manager
    (`systemd-run --user --scope shipyard run …`; see [Cgroup](run.md#cgroup)).
- **For root's bridge networks:** nf_tables (every current kernel).

```sh
sudo apt-get install uidmap passt        # Debian, Ubuntu
sudo dnf install shadow-utils passt      # Fedora
```

`shipyard doctor` checks most of this on your machine, by trying it (user namespaces,
mounts, overlayfs where the image store goes, a cgroup), and says what to fix. It finds
pasta, but doesn't start it.

On an Apple Silicon Mac, Shipyard runs containers in a Linux VM of its own: see
[macOS](macos.md).

## Install from source

You need [Rust](https://rustup.rs). The repository pins the exact toolchain
version, and `rustup` installs it automatically on the first build.

```sh
git clone https://github.com/Zena4L/shipyard.git
cd shipyard
cargo build --release
```

The binaries are `target/release/shipyard` and `target/release/shipyard-runtime` (the
[OCI runtime](runtime.md)). Copy them somewhere on your `PATH`, for example:

```sh
sudo install -m 0755 target/release/shipyard /usr/local/bin/shipyard
sudo install -m 0755 target/release/shipyard-runtime /usr/local/bin/shipyard-runtime
```

Install it with mode `0755` as shown. Shipyard refuses to run if it is installed
setuid or setgid.

### Ubuntu 23.10 and later: AppArmor

Ubuntu restricts unprivileged user namespaces with AppArmor
(`kernel.apparmor_restrict_unprivileged_userns=1`), so a rootless container is
refused until Shipyard is allowed them. Its profile allows them through Shipyard's two
binaries, as Podman's and Chrome's profiles do through theirs:

```sh
sudo install -m 0644 contrib/apparmor/shipyard /etc/apparmor.d/shipyard
sudo apparmor_parser -r /etc/apparmor.d/shipyard
```

The profile needs AppArmor 4 (Ubuntu 23.10 and later), and matches `/usr/local/bin`
and `/usr/bin`: installed elsewhere, add your path to it (`shipyard doctor` says when
it doesn't attach).

What it grants: the restriction exists to keep unprivileged users from the kernel code
user namespaces expose. Anyone who can run Shipyard gets user namespaces through it,
which is what Shipyard is for, so for them that protection is lifted, as with Podman's
profile. The alternative, `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0`,
lifts it for every program on the machine.

## Quick start

Check that your machine can run containers:

```sh
shipyard doctor
```

```text
host:     linux / aarch64 (kernel 6.8.0-142-generic)
backend:  native (host kernel)
user:     1000 (rootless)

ok    user namespaces  allowed (probed)
ok    ID mapping       your ID and 65536 more from /etc/subuid and /etc/subgid (newuidmap and newgidmap)
ok    cgroups          v2, delegated at /sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service
ok    images           overlayfs works where the store will be (/home/alice/.local/share/shipyard/images, not made yet: probed in /home/alice/.local/share)
ok    network          pasta (/usr/bin/pasta)
ok    runtime state    /run/user/1000/shipyard
ok    linger           off (no container has a restart policy that would need it)
ok    restart at boot  the boot unit isn't enabled (no container has a restart policy)

containers run here
```

Each line is `ok`, `warn` (something is limited, such as resource limits, but
containers run) or `fail` (a plain `shipyard run --no-image`, or a run with an image, would fail),
with what to do. The
exit code is 1 if anything failed. `doctor` tries what a run needs: it makes a user
namespace, a mount and an overlay next to the image store, and removes them again.
As root, its `firewall` line says whether another program's firewall (firewalld's,
Docker's) would keep the containers from the outside, and how to let them through.

Run a shell in Alpine Linux, pulled from Docker Hub:

```sh
shipyard run -it alpine sh
```

```text
Pulling docker.io/library/alpine:latest
…: downloaded (3.9 MiB)
/ # id
uid=0(root) gid=0(root)
/ # wget -qO- example.com | grep -o '<title>.*</title>'
<title>Example Domain</title>
/ # exit
```

It's **rootless**: root inside is you, in a user namespace (see
[Rootless](run.md#rootless)). It has its own processes, hostname, files and network,
and with `--memory 512m` (or `--cpus`, `--pids-limit`, …) its own limits. See
[Running commands](run.md) for everything `run` accepts, and [Images](images.md)
for pulling and the image store.

`-it`, as with Docker, gives the shell a terminal of the container's own and passes
what you type on to it: full-screen programs, password prompts and job control work
inside (see [A terminal of the container's own](run.md#a-terminal-of-the-containers-own--t)).
A container never gets your terminal itself.

A command, and its exit code:

```sh
shipyard run alpine sh -c 'echo "hello from PID $$"; exit 3'
echo $?
```

```text
hello from PID 2
3
```

PID 2: the command sees only its own processes, under Shipyard's small init (PID 1).

## What it is: `version` and `info`

`shipyard version` says what Shipyard is, as Docker's `version` does: a `Client`
part (its version, the API version it speaks, 1.43, its revision, its OS and
architecture) and a `Server` part (the engine: the same, with the oldest API version
it takes, 1.24). `shipyard info` says what it has and what its host is: its
containers (running, paused, stopped), images, storage and logging drivers, cgroup
driver and version, security options, the kernel, the distribution, CPUs, memory and
the host's name. Both take `--format`: a Go template over Docker's objects
(`version --format '{{.Server.APIVersion}}'`, `info --format '{{.NCPU}}'`), or
`json`. On a Mac both describe the VM's `shipyard` (`shipyard -v` is the Mac's own).

## Limitations

- **No image `history`, `save` or `load` yet.**
- **macOS is new:** commands run in Shipyard's Linux VM (on a terminal there when you're
  on one, published TCP ports on your Mac's `localhost`, the VM stopping by itself when
  idle); the guest is built from the source tree, and UDP ports aren't forwarded yet
  ([macOS](macos.md#limits-this-version)).
- **Rootless containers have no shared networks:** each gets its own (pasta); bridge
  networks, and names between containers, are root's
  (see [Networking](networking.md#rootless-containers-pasta)).
- **Other firewalls are left to you:** firewalld, ufw or Docker's rules on the same
  host can keep root's containers from the outside. Shipyard warns, with the exact
  fix, but never changes another program's firewall
  (see [Other firewalls](networking.md#other-firewalls)).
- **No default limit on memory:** as Docker, a container can use what the host has
  unless it's given `--memory`. Its processes are limited to 2,048 by default (unlike
  Docker), where the `pids` controller reaches it (see [Running
  commands](run.md#memory-cpu-and-io-limits)).
- **The default network reaches the host and the cloud's metadata:** `bridge` can't
  have `--isolate-host`; put containers that mustn't on a network of your own made
  with it (see [Networking](networking.md#networks)).
- **Restart policies have edges:** the boot unit doesn't know stores you set with
  environment variables (`SHIPYARD_IMAGE_STORE`, `XDG_DATA_HOME`) until you give it
  them (`shipyard doctor` says how). On a full disk, a container's end is recorded,
  and its policy applied, once there's room again
  (see [Containers](containers.md#after-a-reboot)).
- **Not every Docker flag yet** (see each page's limits). `inspect` prints Docker's shape, but not all its fields yet: `ShmSize`, the
  image's manifest descriptor and some resource fields are left out. Its `/etc` files'
  paths (`ResolvConfPath`, `HostnamePath`, `HostsPath`) are there while it runs, and
  empty once it has ended (Docker keeps them: Shipyard's go with the run).
- **Not a hardened security boundary yet:** containers are confined (namespaces,
  capabilities, seccomp, cgroups), but don't run code you wouldn't run as your own
  user. See [SECURITY.md](https://github.com/Zena4L/shipyard/blob/main/SECURITY.md).

## Getting more output

Every command accepts these flags:

| Flag | Effect |
|---|---|
| `-D`, `--debug` | Show debug logs |
| `--log-level LEVEL` | `error`, `warn` (default), `info`, `debug` or `trace` |

The `RUST_LOG` environment variable also sets the log level, but a flag wins over
it. Logs go to stderr, so they never mix with a command's output.

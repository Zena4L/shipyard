# Running commands

```sh
shipyard run [OPTIONS] COMMAND [ARGS...]
```

`shipyard run` starts a command the way a container runtime does: with a clean
environment and exactly the user, working directory and limits you ask for.

> **Status:** on Linux, `run` isolates the command in its own process IDs, mounts,
> hostname and IPC (see [Isolation](#isolation)). With `--rootfs` it also gets a
> root filesystem of its own (see [Root filesystem](#root-filesystem)); without it, it
> sees the host's files. It has a network of its own with a way out and
> published ports (`-p`): the default bridge network as root, pasta without (see
> [Network](#network)). On macOS the command isn't isolated yet.

## Arguments

Shipyard's options go **before** the command. Everything from the command onward
belongs to the command:

```sh
shipyard run -e A=1 ls -l --env X     # ls receives: -l --env X
```

A mistyped option before the command is an error, so it is never mistaken for
the command. Use `--` if the command itself starts with `-`:

```sh
shipyard run -- --program-starting-with-dash
```

Arguments are passed through byte for byte, including ones that aren't valid UTF-8.

## Isolation

On Linux, `shipyard run` starts the command in new **namespaces**, so it gets
its own view of:

| What | Inside the container |
|---|---|
| Process IDs | The command sees only its own processes (`ps`, `/proc`). It's PID 2: PID 1 is a small init (see below). |
| Mounts | Mounts made inside stay inside; the host never sees them. Mounts the host makes or removes later do reach the container, so a disk unmounted on the host isn't kept busy by it. |
| Hostname | Its own copy: `--hostname`, else the run's ID (12 hexadecimal digits) with a network of its own, or the host's name with the host's network. |
| IPC | Its own System V IPC objects and POSIX message queues. |
| Keyring | A new, empty session keyring (`man 7 keyrings`), as runc gives each container: the keys your login session holds stay out of its reach. |
| Cgroup | Run as root, a **cgroup** of its own (see [Cgroup](#cgroup) below). |
| Network | Its own: the default bridge network as root, pasta without; or what `--network` says (see [Network](#network)). |

Without `--rootfs`, the command sees the host's files. With `--network host`, it
sees the host's network.

- **Root or not.** Run as root, the container is rootful. Run as anyone else, it's
  **rootless**: it gets a user namespace of its own (see [Rootless](#rootless)).
- **The command is root inside** unless `--user` says otherwise, as with Docker.
  For a rootless container, root inside is you on the host.
- **Refused by the system.** Namespaces can be refused, for example inside a
  container started without privileges, or by a security policy (seccomp,
  AppArmor, SELinux). `run` then exits with 125 and says why; use `--no-isolation`
  there.
- **`--no-isolation`** runs the command without namespaces, as you: it shares the
  host's processes, mounts, hostname, IPC and network.
- **`--hostname NAME`** sets the hostname inside: letters, digits and hyphens in
  dot-separated parts (like `web-1` or `db.internal`), at most 64 characters, no
  part starting or ending with a hyphen. Without it, a container with a network
  of its own is named by the run's ID, 12 hexadecimal digits new for each run,
  as Docker names a container; one with the host's network keeps the host's
  name. Not with `--no-isolation`.
- **`--domainname NAME`** sets the NIS domain name inside (what `domainname`
  prints), with the same rules as the hostname. The host's is unchanged. Not
  with `--no-isolation`.
- **`--sysctl KEY=VALUE`** sets a kernel setting for the container, such as
  `--sysctl kernel.shmmax=1073741824`. Repeatable. Only settings the container
  owns are allowed: the shared-memory, message-queue and semaphore ones
  (`kernel.shmmax`, `kernel.shmall`, `kernel.shmmni`, `kernel.shm_rmid_forced`,
  `kernel.msgmax`, `kernel.msgmnb`, `kernel.msgmni`, `kernel.sem`, and
  `fs.mqueue.*`), and, with a network of its own (any `--network` but `host`), the
  network ones (`net.*`, such as `--sysctl net.ipv4.ip_forward=1`). The kernel
  keeps a separate copy of those for each container, so the host's stay as they
  are. Any other setting would change the whole machine, so it's refused, and so
  are network settings when the container shares the host's network, and
  conntrack's table size and helpers (`net.netfilter.nf_conntrack_max`,
  `nf_conntrack_expect_max`, `nf_conntrack_helper`), which are the whole
  machine's. A network
  setting the whole machine shares (one the kernel keeps no copy of per network)
  fails when the container starts, saying so. Use `--hostname` and
  `--domainname` for those two. Not with `--no-isolation`.
- **A small init is PID 1.** The process with PID 1 in a container is special:
  the system ignores any signal it has no handler for. So Shipyard runs its own
  small init as PID 1, and the command as PID 2, where signals work normally.
  The init passes every signal on to the command, cleans up exited processes
  ("zombies") in the container, and when the command exits, stops everything
  else left in the container and reports the command's exit status exactly.
  It shows up as `shipyard-init` in `ps`. It holds none of Shipyard's command
  line or environment, so `-e` values passed to Shipyard can't be read from it
  inside the container.
- **Terminal:** see [On a terminal, isolated](#on-a-terminal-isolated) below.
- **Cgroup:** see [Cgroup](#cgroup) below.
- **If Shipyard is killed**, the whole container goes with it.
- **Not a hardened boundary yet.** The command runs as root, with only a few of
  root's privileges (see [Capabilities and privileges](#capabilities-and-privileges))
  and the system calls most programs need (see [System calls](#system-calls-seccomp)).
  In a rootful container it's still root on the host's files, unless `--uidmap`
  (see [User namespaces](#user-namespaces)) makes root inside an ordinary user
  outside; in a rootless one it's you (see [Rootless](#rootless)). Without
  `--image` or `--rootfs`, those files are the host's own, its whole `/`: a
  rootful run warns about it ("the container is root on the host's own files"). With
  `--network host`, the container shares the host's network; it uses as much
  memory and CPU as it likes unless you set limits (see
  [Memory, CPU and I/O limits](#memory-cpu-and-io-limits)).
- If `/` isn't a mount point (for example inside a plain `chroot`), isolation
  fails with "making the container's mounts private: Invalid argument"; use
  `--no-isolation` there.
- **macOS:** commands aren't isolated yet (as with `--no-isolation`);
  `--hostname`, `--domainname`, `--sysctl`, `--rootfs`, the network flags
  (`--network`, `--name`, `--network-alias`, `--ip6`, `-p`, `--add-host`, the `--dns`
  flags) and the other isolation flags are errors there.

### Network

`--network MODE` says what network the container gets:

| Mode | The container's network |
|---|---|
| `bridge` | One of its own on the default network: an address on a bridge it shares with other containers and the host, and a way out. Root only, and root's default. |
| `NAME` | The same on a network made with `shipyard network create NAME`. Root only. |
| `pasta` | One of its own, connected through pasta: a way out and published ports. The default without root. |
| `host` | The host's: its interfaces, addresses and ports. |
| `none` | One of its own, with only a loopback interface (`lo`, up, `127.0.0.1`): no way in or out. |

```sh
shipyard run --network none cat /proc/net/dev    # only lo
sudo shipyard run --network bridge --image alpine:3.20 ip addr show eth0
```

On a bridge network the container has `eth0` with an address of its own, and a
default route through the bridge. It reaches the other containers on the network,
the host and the internet; the host reaches it, and so does anyone through the
ports it publishes (`-p 8080:80`, `-P`). `--ip` and `--mac-address` choose its
addresses. On a network made with `shipyard network create`, containers find
each other by name: `--name db` (and each `--network-alias`) is a name the
network's DNS server answers. [Networking](networking.md) covers networks,
addresses, names, published ports, other firewalls, and the `/etc/hosts`,
`/etc/hostname` and `/etc/resolv.conf` a container gets (with the `--add-host`
and `--dns` flags).

With a network of its own (any `--network` but `host`):

- **Its own settings.** `--sysctl net.*` changes the container's copy (see
  `--sysctl` above). As Docker does, any user in the container may listen on a
  port below 1024 (`net.ipv4.ip_unprivileged_port_start=0`) and use `ping` without
  privileges (`net.ipv4.ping_group_range`: every group the container has). It
  routes nothing (`net.ipv4.ip_forward=0` and `net.ipv6.conf.all.forwarding=0`,
  whatever the host's): a container on two networks never joins them. A `--sysctl` of yours for any of these wins.
- **Its own `/sys`** (with `--rootfs` or `--image`): `/sys/class/net` lists only its
  own interfaces, rootless too.
- **Programs that talk to themselves over `127.0.0.1` work.** With `none`,
  nothing else can reach the container, or be reached from it.

Without root, a container gets a network of its own through pasta (`--network
pasta`, the default; see [Networking](networking.md#rootless-containers-pasta)).
A container can be on several bridge networks: `--network` once for each (see
[Several networks](networking.md#several-networks)). On a network made with
`--ipv6` it has an IPv6 address too (`--ip6` chooses it; see
[IPv6](networking.md#ipv6)). Rootless bridge networks (and with them, names
between rootless containers) come in a later version. Not with
`--no-isolation`.

### Cgroup

A **cgroup** (control group, `man 7 cgroups`) is a group of processes the kernel
can count, limit, freeze or kill together. Each isolated container gets one of its
own, named after the `shipyard` process's PID:

- **as root,** `/sys/fs/cgroup/shipyard/run/PID`;
- **rootless,** in the part of the tree delegated to you: `shipyard/run/PID` below
  the highest cgroup above your own that you own. With systemd that's your user
  manager's, `/sys/fs/cgroup/user.slice/user-UID.slice/user@UID.service`, which any
  process it started is in: a desktop terminal, or a command run with
  `systemd-run --user --scope`.

- **Every process of the container is in it,** from its very first instruction:
  the container starts there. Shipyard itself isn't.
- **Inside, it's the whole tree.** `/proc/self/cgroup` says `0::/`, and
  `/sys/fs/cgroup` shows only the container's own cgroup, read-only: the host's
  tree, and other containers', are out of sight.
- **It goes with the container.** When the run ends, anything left in it is
  killed and it's removed. If Shipyard itself was killed (`kill -9`), the empty
  cgroup it leaves is removed by the next isolated `shipyard run` (by the same user).
- **It holds the limits** you set (see
  [Memory, CPU and I/O limits](#memory-cpu-and-io-limits)).
- **Not made** where the system has no cgroup v2 tree at `/sys/fs/cgroup` (cgroup
  v1 only), where it can't be written (a container started without privileges), or,
  rootless, where nothing is delegated to you. An SSH login, for one, puts your shell
  in a session scope that's root's. The run goes on without one (its cgroup
  namespace's root is then Shipyard's own cgroup; `/sys/fs/cgroup` is the host's tree
  without `--rootfs`, and empty with one), unless it asks for limits: then it's refused
  (exit 125), saying why, and for an SSH login how: run it under your user manager,
  `systemd-run --user --scope shipyard run …`.

### Capabilities and privileges

Root's power is split into about forty **capabilities** (`man 7 capabilities`):
`CAP_CHOWN` to change any file's owner, `CAP_NET_ADMIN` to configure the network,
`CAP_SYS_ADMIN` for mounting and much else. An isolated command gets only the 14
Docker gives by default:

`CAP_CHOWN`, `CAP_DAC_OVERRIDE`, `CAP_FOWNER`, `CAP_FSETID`, `CAP_KILL`,
`CAP_SETGID`, `CAP_SETUID`, `CAP_SETPCAP`, `CAP_NET_BIND_SERVICE`, `CAP_NET_RAW`,
`CAP_SYS_CHROOT`, `CAP_MKNOD`, `CAP_AUDIT_WRITE`, `CAP_SETFCAP`.

That's enough for ordinary programs that start as root (changing file owners,
switching users, binding ports below 1024), and not enough to mount file systems,
change the network, load kernel modules or trace other processes. The rest are gone
for good: not even a setuid-root program inside gets them back.

- **`--cap-add CAP`** adds one (`--cap-add NET_ADMIN`, or `CAP_NET_ADMIN`; `ALL` for
  every one). **`--cap-drop CAP`** removes one (`--cap-drop ALL` for none). Both are
  repeatable; naming the same capability in both is an error.
- **A command run as another user** (`--user 1000`) has no capabilities in effect,
  except those you add: `--user 1000 --cap-add NET_BIND_SERVICE` lets a web server
  running as user 1000 listen on port 80 (as Podman does; Docker gives such a user
  nothing).
- **`no_new_privs` is on.** Nothing the command runs can gain privileges: a setuid
  program (`sudo`, `su`, `passwd`) runs with its caller's rights, not root's.
  `--security-opt no-new-privileges=false` turns it off for an image that needs them
  (`no-new-privileges` alone, or `=true`, keeps it on; Docker's older `:` in place of
  `=` works too, for `seccomp:` as well).
  **Docker's default is off**; Shipyard's is on.
- **Programs with file capabilities don't get them either,** for the same reason:
  Debian's `ping`, for example, holds `cap_net_raw` instead of being setuid. A
  non-root `ping` still works in a container with a network of its own (the
  default): it falls back to an ICMP datagram socket, which every group the
  container has may open (`net.ipv4.ping_group_range`). With `--network host`, it
  depends on the host's setting; otherwise give the user the capability itself
  (`--cap-add NET_RAW`, which a non-root user keeps), or turn `no_new_privs` off.
- **`--cap-add ALL`** means every capability this kernel has: an older one lacks the
  newest (Linux 5.9 added `CAP_PERFMON`, `CAP_BPF` and `CAP_CHECKPOINT_RESTORE`).
  Naming one it lacks is an error.
- **The small init** (PID 1) gives up every capability but `CAP_KILL` (to pass
  signals to a command running as another user) before the command goes on, and
  runs under the container's seccomp filter too (see below). Root inside can't
  look into it (`/proc/1/fd`, its memory) without `--cap-add SYS_PTRACE`.

These apply to isolated commands; with `--no-isolation` the command keeps the
rights of whoever runs Shipyard, and the flags are errors.

### System calls (seccomp)

A program asks the kernel for everything it does through **system calls** (`read`,
`openat`, `clone`, …, over 300 of them). Most programs use a few dozen. The rest,
such as keyrings (`keyctl`), BPF programs (`bpf`), new namespaces (`unshare`) and
kernel modules, are where many attacks on the kernel start. An isolated command
runs under a **seccomp filter**: the kernel checks each call against it, and every
program the command starts stays under it.

- **The default is Docker's profile.** It allows what ordinary programs need, and
  refuses the rest with "Operation not permitted" (EPERM). Some calls come with a
  capability, as in Docker: `--cap-add SYS_ADMIN` also allows `mount`, `unshare` and
  `setns`, `--cap-add SYS_PTRACE` allows `process_vm_readv`, and so on.
- **`--security-opt seccomp=FILE`** uses a profile of your own, in Docker's JSON
  format (which includes the OCI format). For example, to refuse `mkdir` with
  "Not a directory":

  ```json
  {
    "defaultAction": "SCMP_ACT_ALLOW",
    "syscalls": [
      {"names": ["mkdir", "mkdirat"], "action": "SCMP_ACT_ERRNO", "errnoRet": 20}
    ]
  }
  ```

  The file is checked before anything starts. A misspelled call name, an unknown
  field or an unsupported action is an error that names the file. A call another
  architecture has (`arch_prctl` on 64-bit Arm) is skipped, so one profile works on
  both.
- **`--security-opt seccomp=unconfined`** runs without a filter.
- **When several rules match a call, the most restrictive one wins** (kill, then
  trap, then errno, then log, then allow), whatever order they're written in.
- **Actions:** `SCMP_ACT_ALLOW`, `SCMP_ACT_ERRNO` (with `errnoRet`; EPERM if it's
  left out), `SCMP_ACT_LOG`, `SCMP_ACT_TRAP`, `SCMP_ACT_KILL` (the thread),
  `SCMP_ACT_KILL_THREAD` and `SCMP_ACT_KILL_PROCESS`. A command killed by the
  filter exits with 159 (SIGSYS). `SCMP_ACT_TRACE` and `SCMP_ACT_NOTIFY` aren't
  supported.
- **32-bit programs are killed.** The filter allows only the machine's own 64-bit
  calls: a 32-bit program, or the x32 ABI on x86-64, would reach the kernel through
  other call numbers. Run them with `seccomp=unconfined`.
- **Under x86-64 emulation** (Rosetta, as in Docker or OrbStack on an Apple-silicon
  Mac), the kernel refuses every seccomp filter. An isolated run fails there with
  exit code 125 and a message saying so; add `--security-opt seccomp=unconfined` to
  run without one. Shipyard never drops the filter on its own.
- **Kernels before 4.14** refuse `SCMP_ACT_LOG` and `SCMP_ACT_KILL_PROCESS`: a
  profile that uses them fails the same way ("the kernel refused it").
- **Arguments are compared as 64-bit numbers.** A negative `int` argument arrives
  sign-extended: `AT_FDCWD` (-100) is `18446744073709551516`
  (`0xffffffffffffff9c`), not `4294967196`. libseccomp and Docker behave the same.
- **`"errnoRet": 0` makes a call look successful without running it:** an `rm`
  refused this way prints nothing and leaves the file.
- **The init runs under it too,** as Docker's init does: whatever reaches PID 1
  from inside the container meets the same rules.
- **What the profile must allow:**
  - what the init does: `rt_sigtimedwait`, `wait4` and `kill` among others (Docker's
    profile allows them). If one is refused, the init can't supervise the command,
    and the container ends with exit code 125: "the container's init couldn't
    supervise its command" (refused with an error, at once or at the first signal
    to pass on), or "the container's init was killed by SIGSYS" (refused with a
    kill or a trap).
  - `execve`, since the command is started under the filter. If it's refused,
    Shipyard says so ("cannot execute: Operation not permitted"), unless `write` is
    refused too: then the command just exits with 125 and no message.
  - With `no-new-privileges=false`, the filter goes on before the switch of user
    and capabilities, so the profile must also allow `prctl`, `capset`, `setgroups`,
    `setresuid`, `setresgid` and `rt_sigprocmask` (Docker's profile does). A step it
    refuses fails with "Operation not permitted", followed by "(the seccomp profile,
    in force by then, may have refused it)".
- **The file** must be a regular file of at most 1 MiB: a FIFO or a device is
  refused, not read.

### User namespaces

A **user namespace** gives the container user and group IDs of its own. Inside, the
command can be root (uid 0) while on the host it's an ordinary user with no
privileges, so a command that breaks out of the container is nobody special.
Capabilities inside apply only to what the container owns.

```sh
sudo shipyard run --uidmap 0:100000:65536 sh -c 'id; cat /proc/self/uid_map'
# uid=0(root) gid=0(root) groups=0(root)
#          0     100000      65536
```

- **`--uidmap CONTAINER:HOST:SIZE`** maps `SIZE` user IDs from `CONTAINER` inside to
  `HOST` outside: here root inside is host user 100000, user 1000 is 101000, and so
  on up to 65535. It's repeatable, for several ranges.
- **`--gidmap`** does the same for group IDs. Without it, the group IDs are mapped
  like the user IDs.
- **Container ID 0 must be mapped,** for both users and groups: the container is set
  up as root inside. Ranges can't overlap, inside or outside.
- **An ID outside the maps doesn't exist inside.** `--user`, its group and every
  `--group-add` must be mapped, or the run fails with "user 70000 isn't mapped in the
  container's user namespace" (exit 125). Files on the host owned by unmapped IDs
  show up as `nobody` (65534) inside.
- **What the command creates on the host belongs to the mapped IDs:** a file root
  inside writes into a bind-mounted directory is owned by 100000 on the host.
- **Root inside can't read or change the host's root-owned files,** even with its
  capabilities: they only reach files whose owner is mapped. `cat /etc/shadow` fails
  with "Permission denied" (without a root filesystem, the container sees the host's
  files).
- **The root filesystem must belong to the mapped IDs**, or root inside can read it
  (where its permissions let anyone) but not change it. Give it to the range before
  running:

  ```sh
  sudo chown -R 100000:100000 rootfs
  sudo shipyard run --uidmap 0:100000:65536 --rootfs rootfs sh
  ```

  With `--image`, that's done for you: the image's layers are extracted again for
  that map, as a copy whose files belong to the mapped IDs (one copy per distinct
  map, removed with the image), and the container's writable layer belongs to the
  mapped root. Shipyard (as root) mounts the image itself, since the mapped root
  can't reach the store; that needs Linux 6.8 or later.

- **`/dev` is the host's, bound in, and so is `/sys` without a network of its
  own.** A user namespace can't create
  device nodes, so `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`,
  `/dev/urandom` and `/dev/tty` are binds of the host's own. `/sys` is the host's,
  read-only, unless the container has a network of its own (any `--network` but
  `host`, and the default is one): a user namespace can only mount a `/sys` of its
  own with its own network, and only
  where the host's is fully visible (inside another container it often isn't, and
  the host's is bound instead). What the
  host mounted inside it (its cgroup tree, for one) is covered by an empty
  directory, as in a container without a user namespace; `/sys/fs/cgroup` then
  shows the container's own cgroup (see [Cgroup](#cgroup)). `/dev/pts` belongs to the `tty`
  group (5) only if group 5 is mapped. With `--cap-add SYS_ADMIN`, root inside can
  unmount those covers, and then reads (never writes) what they hid, such as the
  host's cgroup tree.
- **Don't map host ID 0.** `--uidmap 0:0:65536` works, but root inside is then root
  on the host's files, which is what a user namespace is for preventing: Shipyard
  warns.
- **The mapped users need the host's permissions:** to run Shipyard's own binary
  (the container's setup starts from it) and to reach the root filesystem (every
  directory above it). A root filesystem under `/root` (mode 700), for example, fails
  with "Permission denied (in its user namespace, the container's root is host user
  100000, …)".
- **Shipyard itself in a user namespace** (inside another container, say) can only
  map host IDs that its own namespace has (`/proc/self/uid_map`); others fail with
  "writing the container's uid_map … Operation not permitted".
- **`/proc` must be Shipyard's own PID namespace's** (after `unshare -p -f`, mount a
  new one with `--mount-proc`): the maps are written through it.
- **`--uidmap` and `--gidmap` are root's.** Without root, the maps are chosen for
  you (see [Rootless](#rootless)).
- **macOS:** like the other isolation flags, `--uidmap` is an error there.

### Rootless

Run `shipyard run` without root and the container is **rootless**: it gets a user
namespace (see [User namespaces](#user-namespaces)) whose root is you. Inside, the
command is root over the container: its processes, mounts, hostname and files you
own. On the host it's still you, with no more rights than you have.

```sh
shipyard run sh -c 'id; cat /proc/self/uid_map'
# uid=0(root) gid=0(root) groups=0(root)
#          0       1000          1
#          1     100000      65536
```

- **Your subordinate IDs, from 1 up.** Root inside is your own ID; IDs from 1 up
  inside are your **subordinate IDs**, which `/etc/subuid` and `/etc/subgid` give
  you (`subuid(5)`): usually 65536 of them, so IDs 0 to 65536 exist inside. Mapping them takes the setuid helpers `newuidmap` and
  `newgidmap` (the `uidmap` package), found on `PATH`. Most distributions give new
  users a range; otherwise an administrator can:

  ```sh
  sudo apt-get install uidmap                                     # or your distribution's
  sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 alice
  ```

  A file `--user 1000` creates is then owned by host user 100999 (the range's
  1000th ID): yours to remove, not root's.
- **Otherwise, only root inside.** Without the helpers or a range, or with ranges
  the kernel would refuse (one overlapping your own ID, say), only your own ID is
  mapped (as root inside), and `run` warns, saying what's missing. Then:
  - `--user` other than 0 fails ("user 1000 isn't mapped…");
  - `--group-add` fails ("supplementary group 0 can't be set…"), and programs that
    change their groups can't: the kernel denies `setgroups` there;
  - the command keeps your supplementary groups on the host. Inside they show as
    `nogroup` (65534), but the host still counts them: a file only your `docker`
    group may open stays open to it. With the full map, the command has no
    supplementary groups.
- **When `newuidmap` or `newgidmap` fails,** `run` shows what it said. They must be
  setuid root (a copy, or one on a `nosuid` mount, isn't), and `/etc/subuid` and
  `/etc/subgid` must give you the IDs. A helper that hangs (`/etc` on a hung network
  file system) is stopped after 10 seconds.
- **Your root filesystem is root's inside:** a `--rootfs` directory you own is
  writable by root inside, with no `chown` needed. What `--user 1000` creates there
  belongs to a subordinate ID (100999) on the host, which you can't remove directly.
  Remove it as root inside instead:
  `shipyard run --rootfs rootfs rm -rf /path/inside`.
- **The maps are chosen for you:** `--uidmap` and `--gidmap` are for root.
- **Limits can only go down:** `--ulimit` can't raise a hard limit above yours
  (that needs root on the host, which root inside isn't); it fails with
  "Operation not permitted (raising a hard limit needs root)".
- **Inside another user namespace** that denies `setgroups` (Shipyard started under
  `unshare -U -r`, say), Shipyard is root there and runs containers as root does,
  but `--group-add` fails the same way.
- **When user namespaces are off,** `run` says which setting stops them:
  - Ubuntu (23.10 and later) restricts them with AppArmor:
    `sudo sysctl kernel.apparmor_restrict_unprivileged_userns=0`, or an AppArmor
    profile for shipyard, allows them;
  - older Debian kernels have `kernel.unprivileged_userns_clone=0`;
  - `user.max_user_namespaces=0` disables them entirely.

  `--no-isolation` runs without namespaces there.
- **Bundles too:** `shipyard run --bundle` and `shipyard-runtime` run rootless as well
  (see [Rootless bundles](bundles.md#rootless-bundles)).
- **A cgroup and limits** where a cgroup subtree is delegated to you (see
  [Cgroup](#cgroup)).
- **A network of its own,** through pasta, by default: a way out and published
  ports (`-p`), as for root, but no networks shared between containers (see
  [Networking](networking.md#rootless-containers-pasta)). It needs pasta (the
  `passt` package); without it, a rootless run is refused, saying how to do
  without (`--network host` or `--network none`).

### On a terminal, isolated

An isolated command runs in a session of its own. It's never in the terminal's
foreground group and has no *controlling terminal*, though its input and output
are still the terminal. So:

- **Ctrl-C and Ctrl-\\** reach it through Shipyard, exactly once. Ctrl-C stops
  a `sleep` as usual, and Shipyard then ends as if interrupted, so a shell loop
  around it stops too.
- **Resizing the window** is passed on (SIGWINCH), and the command sees the new
  size.
- **Ctrl-Z stops only Shipyard.** The command keeps running, and its output keeps
  appearing. `fg` resumes Shipyard. If the command finishes meanwhile, Shipyard
  reports it once resumed.
- **Closing the terminal** ends the command (exit code 129), unless the command
  ignores hangups. Then it keeps running, and Shipyard with it.
- **A command that stops itself** (`kill -STOP $$`, or a debugger) doesn't stop
  Shipyard, so the terminal seems to hang. Press Ctrl-Z (Shipyard stops and
  your shell comes back), then `fg`: that resumes both. Ctrl-Z typed into a
  program like `vim` or `less` inside doesn't do this; the system ignores that
  request for an isolated command.
- **Programs that open `/dev/tty` fail.** That's how `sudo`, `ssh` and others ask
  for a password, so they can't prompt. Use `--no-isolation` for those, until
  full terminal support comes in a later version.
- **With `--rootfs`, the terminal has no name inside.** The container's `/dev/pts`
  is its own, so your terminal isn't in it: `tty` prints "not a tty", and programs
  that look the terminal up by name can't find it. Reading and writing it, Ctrl-C
  and resizing work as above.
- **Don't run an isolated command that reads the terminal in the background.**
  Nothing stops it from reading (as a background job normally would be), so it
  takes keys you type into your shell.

Use `--no-isolation` for full job control.

## Root filesystem

```sh
shipyard run --rootfs ./rootfs sh
```

(To describe a whole container in one file instead, use an OCI bundle: see
[OCI bundles](bundles.md).)

With `--rootfs DIR`, the directory DIR becomes the container's `/`. The command
sees only what's in it: nothing of the host's files is reachable from inside. DIR
must hold the programs you run and everything they need (their libraries, for
example). Any directory works, such as an unpacked image or a `debootstrap`. To
run a pulled image instead of a prepared directory, use `--image` (below).

The program and the working directory (`-w`) are looked up inside DIR, with the
container's `PATH`. A working directory DIR doesn't have is made there (root's,
`rwxr-xr-x`), as Docker does.

Shipyard adds what every container needs:

| Path | What it is |
|---|---|
| `/proc` | The container's own processes only. |
| `/sys` | Read-only. Its network interfaces are the container's own with a network of its own (any `--network` but `host`), else the host's. |
| `/dev` | A fresh, small directory with only `null`, `zero`, `full`, `random`, `urandom` and `tty`, the links `fd`, `stdin`, `stdout`, `stderr` and `ptmx`, and its own `pts` (terminals), `shm` (shared memory) and `mqueue` (message queues). The host's devices aren't there. |

Shipyard creates `/proc`, `/sys` and `/dev` in DIR if they're missing. DIR itself
is never changed otherwise, and what's mounted inside never shows on the host. If
something is mounted on DIR's `proc`, `sys` or `dev` on the host (as in a chroot
set up by hand), the container gets its own in their place and can't reach the
host's.

**Hidden and read-only paths.** Some kernel files would tell the container about
the host, or let it change the host's settings, so they're hidden or read-only (the
same ones as Docker):

- **Hidden** (they read as empty): `/proc/acpi`, `/proc/asound`,
  `/proc/interrupts`, `/proc/kcore`, `/proc/keys`, `/proc/latency_stats`,
  `/proc/timer_list`, `/proc/timer_stats`, `/proc/sched_debug`, `/proc/scsi`,
  `/sys/firmware`, `/sys/devices/virtual/powercap`.
- **Read-only:** `/proc/bus`, `/proc/fs`, `/proc/irq`, `/proc/sys`,
  `/proc/sysrq-trigger`.

**`--read-only`** makes the root filesystem itself read-only inside: the command
can't change anything in DIR. `/dev` (including `/dev/shm`) and `/proc` stay as
they are. The mount's other options, such as `nosuid` or `noexec`, are kept.

**Limits for now:**

- `/sys` shows the host's network devices unless the container has a network of
  its own (any `--network` but `host`).
- Directories mounted *inside* DIR on the host come along into the container, but
  `--read-only` doesn't make them read-only.
- Shipyard replaces DIR's `/etc/hosts`, `/etc/hostname` and `/etc/resolv.conf`
  for the run only where DIR has them as regular files, never writing into DIR (see
  [Networking](networking.md#the-hostname-and-etc-files)).

**Errors** (exit code 125, before the command starts):

- DIR doesn't exist, or isn't a directory.
- DIR is `/`, the host's own root. Leave out `--rootfs` to share the host's files.
- DIR's `proc`, `sys` or `dev` is a symbolic link. Shipyard refuses to mount on
  it, because it could point out of DIR.
- They're missing and DIR is on a read-only file system, so they can't be
  created. Create them in DIR.

**If the program doesn't start:**

- **"found, but its interpreter or loader is missing"** (exit code 127): the
  program is in DIR, but a library it needs, or its `#!` interpreter, isn't. Or the
  program's path is a symbolic link to an absolute path that's missing inside DIR
  (Debian's `/usr/bin/awk` → `/etc/alternatives/awk`, say): Shipyard looks the
  program up on the host to explain the error, where such a link may resolve.
- **"mounted noexec"** (exit code 126): DIR is on a file system that forbids
  running programs (on some systems, `/tmp`). Put it somewhere else.

## Image (`--image`)

Instead of preparing a root filesystem by hand, run a pulled image:

```sh
shipyard run --image alpine:3.20 cat /etc/alpine-release
```

Shipyard pulls the image (if the store doesn't have it), stacks its layers into a
root filesystem with overlayfs, and runs it. Every flag is checked first, so a
mistake costs no download. The image's baked-in environment, working directory,
user and command are the defaults; any flag you give wins over them:

- **The command** is made as Docker makes it: the image's entrypoint, then the
  command you give, or else the image's default command (`Cmd`).
  `--entrypoint PROGRAM` runs PROGRAM instead of the image's entrypoint (and drops
  its default command too); `--entrypoint ""` runs your command alone (or, with none, the image's default
  command, as Docker does).
- **The environment** is the image's, then each `--env` (which wins). An image's
  variable is taken as the image sets it, never from your shell.
- **The working directory** is `--workdir`, else the image's, else `/`. If the
  image doesn't have it, it's made (root's, `rwxr-xr-x`), as Docker does.
- **The user** is `--user`, else the image's, else root inside the container.

```sh
shipyard run --image nginx:alpine nginx -v            # its entrypoint, your arguments
shipyard run --image alpine:3.20 --entrypoint /bin/echo hi
shipyard run --image node:20 -w / pwd                 # / (not the image's workdir)
```

`--pull WHEN` says when to pull: `missing` (the default: only if the store
doesn't have the image), `always` (ask the registry each time, as a tag may have
moved), or `never` (the store's copy, or an error). A stored image runs without a
network.

`--image` can't be combined with `--rootfs` (they're two ways to give the root).
It works with the rest of `run` — `--read-only`, limits, `--volume`, capabilities.
`--read-only` needs one of the two: without a root filesystem of the container's
own, there's nothing of its own to make read-only.
Running an image needs Linux (overlayfs); see [Images](images.md) for references,
the store, and the `images`/`rmi`/`prune` commands.

## Volumes (`--volume`)

Mount persistent storage or a host path into the container:

```sh
shipyard run --image alpine --volume data:/var/lib/data …     # a named volume
shipyard run --image alpine --volume /host/dir:/mnt:ro …      # a host path, read-only
```

`--volume SOURCE:DEST[:ro]`: a bare `SOURCE` (no slash) is a **named volume**,
created on first use and kept from removal while the container runs; an absolute
host path (it must exist; symlinks are resolved) is **bind-mounted**. `DEST` is
where it appears inside (absolute; not `/`, `/dev`, or in `/proc` or `/sys`).
`:ro` mounts it read-only. Repeatable. A volume needs a root filesystem, so use
`--volume` with `--image` or `--rootfs`. Every `--volume` is checked before an
image is pulled. See [Volumes](volumes.md).

## Environment

The command does **not** inherit your shell's environment. It gets only:

1. the variables you set with `-e/--env`, and
2. defaults for anything you didn't set:

| Variable | Default |
|---|---|
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin` |
| `HOME` | The user's home directory from `/etc/passwd` (the container's; see [User and groups](#user-and-groups)). If the user has no entry: `/root` for user 0, otherwise `/`. |

Your values always win over the defaults.

| Form | Effect |
|---|---|
| `-e KEY=VALUE` | Sets `KEY`. Only the first `=` splits, so `-e A==b` sets `A` to `=b`. |
| `-e KEY=` | Sets `KEY` to an empty string. |
| `-e KEY` | Copies `KEY` from your shell. If it isn't set there, it is skipped. A name with spaces is rejected, since it's almost certainly a mistyped `KEY=VALUE`. |

If you set the same variable twice, the last value wins.

These are rejected:

- an empty name (`-e =x`)
- a NUL byte in a name or value

**Values are never shown**: not in logs (even with `--debug`) and not in error
messages. Shipyard shows variable names only, so secrets such as `-e TOKEN=…`
stay out of your terminal history and CI logs.

## Working directory

| Option | Default |
|---|---|
| `-w, --workdir DIR` | `/` |

The path must be absolute.

## User and groups

| Option | Meaning |
|---|---|
| *(none)* | Isolated: root (`0:0`) inside the container, as Docker (in a rootless container, that's you). With `--no-isolation`, and on macOS: the user who ran `shipyard`. |
| `-u USER` | Run as `USER`: a name such as `postgres`, or a number. |
| `-u USER:GROUP` | Run as `USER`, with primary group `GROUP` (a name or a number). |
| `--group-add GROUP` | Add a supplementary group (a name or a number). Repeatable; duplicates are ignored. |

**Names are looked up in the container's own files**, `/etc/passwd` and
`/etc/group`: its root filesystem's with `--rootfs`, otherwise the host's. Like
Docker:

- **A user name** must be in `/etc/passwd`. It gives the user ID, the primary group
  and the home directory.
- **A user number** is used as it is. If `/etc/passwd` has an entry for it, that
  entry's group and home directory are used; otherwise the group is `0`.
- **A group name** must be in `/etc/group`.
- **The user's groups:** with `-u`, in a container, the groups that list the user
  in `/etc/group` are added too, plus any `--group-add`. Not with `--no-isolation`,
  where `/etc/group` is the host's.
- **`HOME`** is the user's home directory, unless you set it with `-e HOME=…`.

A name that isn't there stops the run (exit code 125), saying which name and which
file. So does a name whose ID in the file is `4294967295`, which the kernel would
read as "leave unchanged". If the files can't be read, only names fail: numbers
still work, `HOME` falls back to its default, and no groups are added. The files are read directly, not through the system's user database. So on a
host whose users come from a directory service (LDAP, or macOS's own), only the
users listed in `/etc/passwd` can be named; numbers always work.

These are rejected:

- **ID `4294967295`.** The kernel reads it as "leave unchanged", so allowing it
  could silently leave the command running as root.
- **Names with spaces, `:`, `,` or `/`**, or that start with `-` or `+`.
- **Too many supplementary groups.** The limit is 15 on macOS (the system's 16
  slots include the primary group) and 65536 on Linux.

In a container, any user can switch: inside a rootless container, the user
namespace makes that possible (see [Rootless](#rootless)). With `--no-isolation`,
switching to another user or group, or adding groups, requires running `shipyard`
as root. Otherwise it fails before starting, and says what differs: for example
`-u 501` alone may ask for group `0`, so use `-u 501:20` to keep your own group.

When running as root, the command gets **only** the groups you ask for; root's
own supplementary groups are always dropped.

## The OOM score (`--oom-score-adj`)

```sh
shipyard run --oom-score-adj 500 my-batch-job
```

`--oom-score-adj N` (-1000 to 1000) sets how willing the kernel's OOM killer is to
pick the command when memory runs out: higher is likelier, -1000 never. It's the
command's `/proc/self/oom_score_adj` (and, isolated, its init's), with or without
isolation, set by Shipyard before the command starts. Lowering it needs root
(`CAP_SYS_RESOURCE`): run as root it works, a user namespace (`--uidmap`) included;
rootless, it can only go up, and a lower value is refused, saying so. Linux only: on
macOS it's an error.

## Process limits (`--ulimit`)

```sh
shipyard run --ulimit nofile=1024:2048 --ulimit core=0 my-server
```

`--ulimit NAME=SOFT[:HARD]` sets a limit:

- **Soft limit**: the limit the kernel enforces.
- **Hard limit**: the ceiling the command may raise its soft limit to. If you give
  only a soft value, the hard limit is set to the same value.
- **Values:** a number, `unlimited`, or `-1` (also unlimited).
- **The soft limit can't be above the hard limit.**
- **Repeats:** if you set the same limit twice, the last one wins.

Names are case-insensitive, and accept both Docker style (`nofile`) and OCI style
(`RLIMIT_NOFILE`):

| Name | Limits | Platforms |
|---|---|---|
| `as` | address space (virtual memory), bytes | all |
| `core` | core dump size, bytes (`0` disables core dumps) | all |
| `cpu` | CPU time, seconds | all |
| `data` | data segment (heap), bytes | all |
| `fsize` | largest file created, bytes | all |
| `memlock` | memory locked into RAM, bytes | all |
| `nofile` | open file descriptors | all |
| `nproc` | processes for the user | all |
| `stack` | main thread stack, bytes | all |
| `locks` | file locks | Linux |
| `msgqueue` | POSIX message queue bytes | Linux |
| `nice` | ceiling for raising priority | Linux |
| `rss` | resident memory, bytes (advisory) | Linux |
| `rtprio` | real-time priority ceiling | Linux |
| `rttime` | real-time CPU time, microseconds | Linux |
| `sigpending` | queued signals | Linux |

On macOS, the largest finite value is `9223372036854775806` (2⁶³ − 2). `rss` is
Linux-only because macOS treats it as another name for `as`.

`nofile=unlimited` on macOS: recent versions accept it as is. Older ones refuse
an unlimited soft limit, and the command then gets the most the system allows
per process (`sysctl kern.maxfilesperproc`). On Linux, `unlimited` is refused before
anything starts (exit 125), because the kernel caps `nofile` at `fs.nr_open`;
use a number instead.

## Memory, CPU and I/O limits

```sh
sudo shipyard run --memory 512m --cpus 1.5 --pids-limit 200 my-server
```

Docker's flags, with the same meaning. They're written into the container's own
cgroup (see [Cgroup](#cgroup)) before anything runs there, so the whole container, its
init included, stays within them. Linux only, as root or rootless where a cgroup
subtree is delegated to you (see [Cgroup](#cgroup)): a run that asks for a limit and
can't have a cgroup of its own is refused (exit 125), never run without it. Rootless,
only the controllers delegated to you can be used: systemd delegates `cpu`, `memory`
and `pids` by default, so `--cpuset-*` and the I/O flags are refused ("it wasn't
delegated to this user") unless `systemctl edit user@.service` adds them
(`Delegate=cpu cpuset io memory pids`).

| Flag | Limits | cgroup v2 file |
|---|---|---|
| `-m`, `--memory SIZE` | The most memory (`512m`, `2g`; at least 6 MiB). Past it, the kernel reclaims, then its OOM killer kills a process. | `memory.max` |
| `--memory-reservation SIZE` | Memory kept for the container when the host runs short. | `memory.low` |
| `--memory-swap SIZE` | Memory **plus** swap (needs `--memory`, except `-1`); `-1` for unlimited swap. | `memory.swap.max` |
| `--cpus N` | N CPUs' worth of time (`1.5`) in every 100 ms. | `cpu.max` |
| `--cpu-period µs`, `--cpu-quota µs` | The same in raw terms (a quota of `-1` or `0`: none). Not with `--cpus`. A quota alone keeps the period the cgroup has (100 ms unless `--cpu-period` set another). | `cpu.max` |
| `-c`, `--cpu-shares N` | A share of the CPU against other containers (1024 is the default share). | `cpu.weight` |
| `--cpuset-cpus LIST`, `--cpuset-mems LIST` | Which CPUs and memory nodes (`0-3`, `0,2`). | `cpuset.cpus`, `cpuset.mems` |
| `--pids-limit N` | The most processes and threads, the init included (`-1` or `0`: none). | `pids.max` |
| `--blkio-weight N` | A share of disk time (10–1000). | `io.weight` |
| `--device-read-bps`, `--device-write-bps DEVICE:RATE` | Bytes per second from or to a block device (`/dev/sda:10mb`). Repeatable. | `io.max` |
| `--device-read-iops`, `--device-write-iops DEVICE:RATE` | Operations per second. Repeatable. | `io.max` |

- **Swap, as Docker:** with `--memory` and no `--memory-swap`, the container may swap as
  much as its memory (512 MiB of memory, 512 MiB of swap). `--memory-swap` equal to
  `--memory` means no swap. Where the kernel doesn't account swap, that default is
  skipped with a warning, and an explicit `--memory-swap` is refused.
- **Out of memory:** the kernel's OOM killer kills one process, as with Docker. If it's
  the command, the run ends with 137 and the warning
  "the command was killed: out of memory". If it's another process,
  the command goes on, and Shipyard says one was killed. (A command Shipyard killed
  itself, after `--stop-timeout`, isn't blamed on the OOM killer.) When the container
  never reached its `--memory` limit, memory ran short outside it (the whole machine,
  or a limit on a cgroup above it, such as your user's slice), and the warning says
  so: "… the host or a parent cgroup ran out of memory". A container that hit its
  limit earlier and was then killed for the host's sake is still reported as "out of
  memory": the kernel's counters don't tell the two apart.
- **0 means "not set"** for every flag, as with Docker: `--memory 0`, `--cpus 0`,
  `--memory-swap 0` (Docker's default swap) and so on.
- **The init counts:** it's one of the container's processes. With `--pids-limit 1` it
  can't start the command, and the run fails, saying the limit is 1.
- **Controllers:** a limit needs its cgroup controller (memory, cpu, cpuset, pids, io).
  Shipyard turns on the ones it needs, from the cgroup root down, as runc does; one the
  kernel doesn't have is refused, naming it. Some files exist only on some kernels:
  `io.weight` comes from the kernel's I/O cost controller (`CONFIG_BLK_CGROUP_IOCOST`),
  so `--blkio-weight` is refused
  ("the container's cgroup has no io.weight: this kernel doesn't have it, …") without
  them. Where the file exists but nothing acts on it, Shipyard warns: a weight needs the
  I/O cost controller turned on for the disk (`io.cost.qos` at the cgroup root, off by
  default). The BFQ scheduler's own weight (`io.bfq.weight`) isn't set.
  A cgroup that holds processes never hands a controller down (the kernel would then
  refuse to place anything below it): inside a container whose processes sit in its
  cgroup namespace's root (a plain `docker run`), limits are refused, saying so. Move
  those processes into a child cgroup first, as docker-in-docker setups do.
- **CPUs and memory nodes** must exist and be allowed to the cgroup tree; the kernel
  refuses others ("Invalid argument").
- **I/O limits take whole disks** (`/dev/sda`, not `/dev/sda1`); a partition is refused,
  saying so.
- **Checked before anything starts:** a value the kernel wouldn't take is an error
  (exit 125).
- **Not with `--no-isolation`** (there's no cgroup) **or `--bundle`** (its
  `linux.resources` say). On macOS, they're errors.

## Devices

```sh
shipyard run --device /dev/fuse --rootfs ./root my-fuse-tool
shipyard run --device /dev/sdb:/dev/xvdc:r --rootfs ./root sh
shipyard run --device-cgroup-rule 'c 189:* rwm' --rootfs ./root lsusb
```

An isolated command may use only certain devices, whatever its capabilities and
permissions: Shipyard attaches a small program to its cgroup (see
[Cgroup](#cgroup)), and the kernel asks that program about every device node the
container opens or makes (`mknod`). A device it refuses fails with "Operation not
permitted".

**By default** (Docker's list, which is runc's), the container may use `/dev/null`,
`zero`, `full`, `random`, `urandom`, `tty`, `ptmx`, its own terminals (`/dev/pts/*`)
and `/dev/net/tun`. It may make a node of any device with `mknod` (root has the
capability by default), but can't open one it made. Without `--rootfs` the container sees the host's `/dev`, and
the same rules apply: `/dev/sda` is there, but it can't be opened.

Where Shipyard can't give the container a cgroup (cgroup v1, or a read-only
`/sys/fs/cgroup`, as inside some containers), there's no program to ask. A rootful
container then goes without `CAP_MKNOD`, even if `--cap-add` gave it, so it can't
make device nodes; the ones Shipyard makes for it (the defaults and `--device`) still
work. Shipyard warns when this happens. Without `--rootfs`, it still sees the host's
`/dev`, and every device there opens.

- **`--device HOST[:CONTAINER][:PERMS]`** gives the container a device of the host's,
  at `CONTAINER` in it (its own path if you leave it out), for `PERMS`: any of `r`
  (read), `w` (write) and `m` (make a node of it), `rwm` if you leave them out.
  - The host path is followed through symbolic links (`/dev/disk/by-id/…`).
  - A directory gives every device below it (`--device /dev/dri`), at the same places
    below `CONTAINER`.
  - The node keeps the host's permissions and owner.
  - A `CONTAINER` path outside `/dev` (a fresh, empty file system for each container)
    is made in the root filesystem's own directory, and stays there after the
    container, as a mount's missing destination does.
  - Without `--rootfs`, the device is used where it is on the host: giving it another
    path is an error (exit 125).
- **`--device-cgroup-rule 'TYPE MAJOR:MINOR ACCESS'`** lets the container use a whole
  kind of device, without making a node: `TYPE` is `c` (character), `b` (block) or `a`
  (both), `MAJOR` and `MINOR` are numbers or `*` (any), and `ACCESS` is from `rwm`. For
  a device whose nodes come and go (USB, say), with its nodes bound in another way.

Both are repeatable, need Linux, and can't be used with `--no-isolation` or
`--bundle` (a bundle has its own fields: see [OCI bundles](bundles.md)). Later
`--device-cgroup-rule`s and `--device`s add to what's allowed; nothing they say can
take a default away. A container has at most 256 rules, the dozen defaults and one per
`--device` node included.

**Rootless:** loading the program needs a capability (`CAP_BPF`) a rootless user
doesn't have on most kernels. Where it's refused, or where the container has no cgroup
of its own, a rootless container runs without it. It still can't make device nodes
(its user namespace can't), and the host's nodes it's given keep the host's
permissions, so it can use only what your user can. If you asked for `--device` or
`--device-cgroup-rule` there, a warning says the rules can't be enforced.
`--device` itself still works rootless: the host's node is bound into the container.

**Only with a cgroup:** the program is attached to the container's cgroup, so a
container without one (on a cgroup v1 host, say: see [Cgroup](#cgroup)) has no
device control at all, as it has no limits. Where you asked for `--device` or
`--device-cgroup-rule`, a warning says so.

As root, a kernel that refuses the program (one without `CONFIG_BPF_SYSCALL` or
`CONFIG_CGROUP_BPF`, which the message names) makes the run fail (exit 125) rather than
run without it. On kernels before 5.11, where a program's memory counts against the
locked-memory limit (`ulimit -l`), root lifts that limit for the load, and puts it back:
the container gets yours.

## Inherited state

The command starts clean, whatever state Shipyard was started in:

- **Signals:** every signal is back to its default action and none are blocked.
  (Shipyard itself ignores `SIGPIPE`, as Rust programs do; the command doesn't.)
  The one exception: if Shipyard was started ignoring `SIGHUP`, `SIGINT` or
  `SIGQUIT`, the command keeps ignoring it. That's how `nohup shipyard run …`
  and `shipyard run … &` in a script keep protecting the command from hangups
  and Ctrl-C.
- **File descriptors:** only stdin, stdout and stderr are passed on. Any other
  open descriptor is closed when the command starts.
- **umask:** `022`.
- **`TERM`:** when Shipyard runs in a terminal, your `TERM` is passed on (unless
  you set one with `-e`), so full-screen programs like `top` and `vim` work.
- **On Linux:** if Shipyard is killed with `SIGKILL`, the command is killed too.
  (On macOS it can be left running in that one case.) Isolated, the whole
  container goes. With `--no-isolation`, one exception: a command run as
  another user (`--user`) that then starts a setuid program loses that
  protection (the system clears it for such programs), and the program can be
  left running.

## Signals and stopping

Shipyard stays with the command until it has exited, and passes signals on.

Isolated commands on a terminal behave differently: see
[On a terminal, isolated](#on-a-terminal-isolated). This section describes
`--no-isolation` (and macOS) on a terminal, and every run without one.

**From the terminal.** Ctrl-C, Ctrl-\\ and Ctrl-Z reach the command directly,
just as without Shipyard, and `fg`/`bg` work as usual. Shipyard doesn't pass
them on a second time. If Ctrl-C ends the command, Shipyard also stops as if
interrupted, so a shell loop around `shipyard run` stops too (unless Shipyard
itself was started ignoring Ctrl-C).

**Stopped jobs.** If the command stops itself (for example with
`kill -STOP $$`, or under a debugger) rather than through Ctrl-Z, Shipyard stops
too, so your shell shows the job as stopped instead of the terminal seeming to
hang. Some shells then say "stopped (signal)" rather than "suspended". `fg`
resumes both. If only Shipyard is resumed (`kill -CONT <shipyard>`), it resumes
the command as well.

Shipyard doesn't stop itself when nothing could resume it:

- **while it's stopping the command** (after `SIGTERM`): it has to stay awake
  to kill the command when `--stop-timeout` runs out;
- **when no job-control shell is above it** (the shell that started it has
  exited, or under `ssh -t host cmd`, `script`, or an IDE's run pane).

A job stopped with Ctrl-Z ignores `kill <shipyard>` until it's resumed, like
any stopped program. The `--stop-timeout` clock keeps running while it's
stopped.

**Sent to Shipyard.** A signal sent to Shipyard's process ID (`kill`, `timeout`,
a CI runner, a service manager) is passed on to the command, with these
exceptions:

- **`SIGTERM` stops the command gracefully, like `docker stop`:** the command
  gets the `--stop-signal`, and if it hasn't exited after `--stop-timeout`, it's
  killed with `SIGKILL`.
- **Not passed on:**
  - job-control signals (`SIGTSTP`, `SIGCONT`, …), which the shell handles for
    both;
  - signals about Shipyard's own state (crashes, `SIGPIPE`, `SIGCHLD`).
- **Signals Shipyard was started ignoring** (for example under `nohup`) stay
  ignored and aren't passed on.
- **Signals 32 and 33** (Linux): the C library keeps them for itself, so
  Shipyard can't catch them, and either one ends Shipyard at once (and an
  isolated command with it). Don't send them to Shipyard.
- **Signals the command sends to Shipyard** aren't bounced back to it. (On
  macOS this is best effort: if two signals arrive at the same instant, the
  system can report the wrong sender for one of them.)
- **On macOS, in a terminal:** while Shipyard runs in the foreground, the
  signals a terminal can send (`SIGINT`, `SIGQUIT`, `SIGHUP`, `SIGWINCH`,
  `SIGINFO`) aren't passed on even when sent with `kill`. macOS reports a
  terminal's Ctrl-C exactly like a `kill`, and the terminal has already
  delivered it to the command. Send them to the command itself instead, or use
  `SIGTERM`. Linux tells the two apart, so this doesn't apply there.

Shipyard catches signals even if the program that started it had blocked
them, and catches `SIGTERM` even if it was started ignoring it. So `SIGTERM`
always stops it, and it always notices when the command exits.

A signal sent to Shipyard's whole *process group* (as `timeout` does, and
`kill -SIG -<pgid>`) reaches the command twice when Shipyard runs in a terminal
without isolation: once directly and once passed on. (An isolated command isn't
in Shipyard's group, so it gets it once.) A signal that arrives before the command has
finished starting gets the command's default reaction (for most signals, it
exits).

| Option | Default | Meaning |
|---|---|---|
| `--stop-signal SIGNAL` | `SIGTERM` | Signal sent to ask the command to exit |
| `--stop-timeout SECONDS` | `10` | How long to wait before forcing it with `SIGKILL` |

The stop signal can't be `SIGSTOP`, `SIGTSTP`, `SIGTTIN`, `SIGTTOU` or `SIGCONT`:
those pause or resume the command instead of ending it. Nor can it be `SIGCHLD`
or signals 32 and 33, which the container's init can't pass on.

Signals can be given as:

- a number: `15`
- a name, with or without the `SIG` prefix and in any case: `TERM`, `SIGTERM`, `sigterm`
- on Linux, a real-time signal: `RTMIN` (34), `RTMIN+3`, `RTMAX-2`, `RTMAX` (64).
  These use the kernel's numbers, the same as Docker, so `RTMIN+3` is always
  signal 37.

## Cleanup

When the command exits, Shipyard cleans up what it left behind:

- **Isolated** (Linux): when the command exits, the container's init exits and
  the system kills everything else left in the container. Its cgroup, if it has
  one, is removed (see [Cgroup](#cgroup)).
- **Without a terminal** (CI, services, scripts), and always when isolated: the
  command runs in its own process group, and anything still running in that
  group is killed.
- **On Linux:** every leftover process is killed, even ones that left the
  command's group (for example with `setsid`). Shipyard registers as their parent
  (a "child subreaper") to find them. Processes that were already Shipyard's
  children when it started (as in `helper & exec shipyard run app`) aren't the
  command's, and are left running.
- **On macOS, from a terminal:** background processes the command started can be
  left running. Full isolation (a later version) removes this gap.
- **Laptop sleep:** if the computer sleeps during `--stop-timeout`, the sleep
  doesn't count toward the timeout.

## Exit codes

See [Exit codes](exit-codes.md). In short: `run` returns the command's own exit
code, `128 + N` if signal N killed it, and `125`–`127` if it never ran.

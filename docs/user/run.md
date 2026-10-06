# Running commands

```sh
shipyard run [OPTIONS] COMMAND [ARGS...]
```

`shipyard run` starts a command the way a container runtime does: with a clean
environment and exactly the user, working directory and limits you ask for.

> **Status:** on Linux, `run` isolates the command in its own process IDs, mounts,
> hostname and IPC (see [Isolation](#isolation)). With `--rootfs` it also gets a
> root filesystem of its own (see [Root filesystem](#root-filesystem)); without it, it
> sees the host's files. It always shares the host's network: networking comes in a
> later version. On macOS the command isn't isolated yet.

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
| Hostname | Its own copy; set it with `--hostname`. |
| IPC | Its own System V IPC objects and POSIX message queues. |
| Keyring | A new, empty session keyring (`man 7 keyrings`), as runc gives each container: the keys your login session holds stay out of its reach. |

Without `--rootfs`, the command sees the host's files. It always sees the host's
network: networking comes in a later version.

- **Root or not.** Run as root, the container is rootful. Run as anyone else, it's
  **rootless**: it gets a user namespace of its own (see [Rootless](#rootless)).
- **The command is root inside** unless `--user` says otherwise, as with Docker.
  For a rootless container, root inside is you on the host.
- **Refused by the system.** Namespaces can be refused, for example inside a
  container started without privileges, or by a security policy (seccomp,
  AppArmor, SELinux). `run` then exits with 125 and says why; use `--no-isolation`
  there.
- **`--no-isolation`** runs the command without namespaces, as you: it shares the
  host's processes, mounts, hostname and IPC.
- **`--hostname NAME`** sets the hostname inside: letters, digits and hyphens in
  dot-separated parts (like `web-1` or `db.internal`), at most 64 characters, no
  part starting or ending with a hyphen. Not with `--no-isolation`.
- **`--domainname NAME`** sets the NIS domain name inside (what `domainname`
  prints), with the same rules as the hostname. The host's is unchanged. Not
  with `--no-isolation`.
- **`--sysctl KEY=VALUE`** sets a kernel setting for the container, such as
  `--sysctl kernel.shmmax=1073741824`. Repeatable. Only settings the container
  owns are allowed: the shared-memory, message-queue and semaphore ones
  (`kernel.shmmax`, `kernel.shmall`, `kernel.shmmni`, `kernel.shm_rmid_forced`,
  `kernel.msgmax`, `kernel.msgmnb`, `kernel.msgmni`, `kernel.sem`, and
  `fs.mqueue.*`). The kernel keeps a separate copy of those for each
  container, so the host's stay as they are. Any other setting would change
  the whole machine, so it's refused, including network settings (`net.*`),
  until containers get networking of their own. Use `--hostname` and
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
- **If Shipyard is killed**, the whole container goes with it.
- **Not a hardened boundary yet.** The command runs as root, with only a few of
  root's privileges (see [Capabilities and privileges](#capabilities-and-privileges))
  and the system calls most programs need (see [System calls](#system-calls-seccomp)).
  In a rootful container it's still root on the host's files, unless `--uidmap`
  (see [User namespaces](#user-namespaces)) makes root inside an ordinary user
  outside; in a rootless one it's you (see [Rootless](#rootless)). The container
  shares the host's network and has no resource limits of its own yet.
- If `/` isn't a mount point (for example inside a plain `chroot`), isolation
  fails with "making the container's mounts private: Invalid argument"; use
  `--no-isolation` there.
- **macOS:** commands aren't isolated yet (as with `--no-isolation`);
  `--hostname`, `--domainname`, `--sysctl` and `--rootfs` are errors there.

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
  Debian's `ping`, for example, holds `cap_net_raw` instead of being setuid, so a
  non-root `ping` fails. Give the user the capability itself (`--cap-add NET_RAW`,
  which a non-root user keeps), or turn `no_new_privs` off.
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

- **`/dev` and `/sys` are the host's, bound in.** A user namespace can't create
  device nodes, so `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`,
  `/dev/urandom` and `/dev/tty` are binds of the host's own. `/sys` is the host's,
  read-only, until containers get their own network (a later version). What the host
  mounted inside it (its cgroup tree, for one) is covered by an empty directory, as
  in a container without a user namespace. `/dev/pts` belongs to the `tty` group (5)
  only if group 5 is mapped. With `--cap-add SYS_ADMIN`, root inside can unmount
  those covers, and then reads (never writes) what they hid, such as the host's
  cgroup tree.
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
- **Not rootless yet:** networking of its own and resource limits come in later
  versions.

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
example). Any directory works, such as an unpacked image or a `debootstrap`. Pulling
images comes in a later version.

The program and the working directory (`-w`) are looked up inside DIR, with the
container's `PATH`.

Shipyard adds what every container needs:

| Path | What it is |
|---|---|
| `/proc` | The container's own processes only. |
| `/sys` | Read-only. |
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

- `/sys` shows the host's network devices, until containers get networking of their
  own.
- Directories mounted *inside* DIR on the host come along into the container, but
  `--read-only` doesn't make them read-only.
- There's no `/etc/hosts`, `/etc/resolv.conf` or `/etc/hostname` from Shipyard
  yet, and no volumes: only what DIR has.

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

## Resource limits

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
  the system kills everything else left in the container.
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

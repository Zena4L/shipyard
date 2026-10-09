# OCI bundles

```sh
shipyard run --bundle ./mybundle
```

An **OCI bundle** is the standard way to describe one container: a directory with a
`config.json` that says what to run and how, and a root filesystem. It's the format
of the [OCI runtime specification](https://github.com/opencontainers/runtime-spec),
which `runc`, Podman and other tools read too.

`--bundle DIR` runs the bundle in DIR. The config is the whole description, so
`--bundle` takes no command and none of the options that describe the container
(`-e`, `-w`, `-u`, `--group-add`, `--ulimit`, `--hostname`, `--domainname`,
`--sysctl`, `--rootfs`, `--read-only`, `--no-isolation`). `--stop-signal` and `--stop-timeout` still work: a
config has no such settings. `-i` works too: as for any container, the bundle's
process never gets your terminal, and gets your input only with `-i` (see [Input,
output and terminals](run.md#input-output-and-terminals)). `-t` doesn't: a bundle
asks for a terminal itself (`process.terminal`), which isn't supported yet. It needs Linux. Run as root, the container is rootful;
run as anyone else, it's rootless (see [Rootless bundles](#rootless-bundles)). A run is
the container's whole life, as with `runc run`: all of its [hooks](#hooks) run, from
`prestart` to `poststop`.

## An example

```json
{
  "ociVersion": "1.2.0",
  "root": { "path": "rootfs", "readonly": true },
  "hostname": "web",
  "process": {
    "user": { "uid": 1000, "gid": 1000 },
    "args": ["/bin/server", "--port", "8080"],
    "env": ["PATH=/bin:/usr/bin", "MODE=production"],
    "cwd": "/srv",
    "rlimits": [{ "type": "RLIMIT_NOFILE", "soft": 1024, "hard": 4096 }]
  },
  "mounts": [
    { "destination": "/srv/data", "type": "bind", "source": "/var/lib/web", "options": ["rbind", "ro"] },
    { "destination": "/tmp", "type": "tmpfs", "options": ["nosuid", "nodev", "size=64m"] }
  ],
  "linux": {
    "namespaces": [{ "type": "pid" }, { "type": "mount" }, { "type": "uts" }, { "type": "ipc" }],
    "maskedPaths": ["/proc/kcore", "/proc/keys"],
    "readonlyPaths": ["/proc/sys", "/proc/sysrq-trigger"]
  }
}
```

## What Shipyard uses

| Field | What it does |
|---|---|
| `root.path` | The root filesystem. A relative path is relative to the bundle. |
| `root.readonly` | Makes the root filesystem read-only, as `--read-only` does. |
| `hostname` | The hostname inside. |
| `domainname` | The NIS domain name inside. |
| `process.args` | The program and its arguments. Required. |
| `process.env` | The environment. Only `HOME` is added, when it's missing: the user's home directory from the root filesystem's `/etc/passwd` (`/root` for user 0, otherwise `/`, if the user has no entry). `PATH` isn't added: without one, the program itself is still found in the standard directories (`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`), but the variable isn't set for it. |
| `process.cwd` | The working directory (absolute). |
| `process.user` | `uid`, `gid`, `umask` and `additionalGids`, exactly: no groups are added from `/etc/group`. |
| `process.rlimits` | Resource limits, such as `RLIMIT_NOFILE`. `18446744073709551615` (the largest number) means unlimited. |
| `process.oomScoreAdj` | The program's OOM score adjustment, -1000 to 1000 (see [The OOM score](run.md#the-oom-score---oom-score-adj)); out of range is an error. Rootless, it can only be raised. |
| `process.capabilities` | The five capability sets (`bounding`, `effective`, `permitted`, `inheritable`, `ambient`), exactly, by name (`CAP_KILL`). An unknown name is an error. **Leaving it out means none**, with a warning (`runc` would leave the process every capability root has). For a process running as root, the bounding set is what really limits it: unless `noNewPrivileges` is set, running a program gives root its whole bounding set back (the kernel's rule). |
| `process.noNewPrivileges` | When `true`, nothing the process runs can gain privileges: setuid programs and file capabilities give nothing. Leaving it out means `false`, as the specification says (unlike `shipyard run`, where it's on by default). |
| `linux.seccomp` | A seccomp profile, which the process runs under (see [System calls](run.md#system-calls-seccomp) for what's supported). It's checked before anything starts: a misspelled call name or an unsupported action is an error. Fields Shipyard doesn't know are ignored, with a warning for each, as the specification requires. **Leaving it out means no filter**, as with `runc` (unlike `shipyard run`, which applies Docker's profile). |
| `linux.namespaces` | Which of `pid`, `mount`, `uts`, `ipc`, `network`, `user` and `cgroup` the container gets. `mount` is required (for the root filesystem). A `network` namespace has only a loopback interface, brought up; with a `path` (such as `/proc/PID/ns/net`, or a bind mount of one made by `ip netns add`), the container joins that network namespace instead, as it is (joining needs root, and only a `network` namespace can be joined by its path for now; one joined with a `user` namespace keeps its settings out of the container's reach, so `net.*` settings are refused there, and joining the host's own gets no settings or devices); without a `network` entry, the container shares the host's network. A `/sys` of the container's own shows its network's interfaces (rootless too, with a `network` namespace it made). Without `pid`, there's no small init, and the command shares the host's process IDs (`shipyard-runtime` then needs a cgroup: see [Cgroups](#cgroups)). A `cgroup` namespace makes the container's own cgroup its root (`/proc/self/cgroup` says `0::/`). A `user` namespace needs both mappings below; see [Rootless bundles](#rootless-bundles) for running without root. |
| `linux.uidMappings`, `linux.gidMappings` | The user namespace's ID maps: `{"containerID": 0, "hostID": 100000, "size": 65536}` entries, checked as `--uidmap` is (ID 0 mapped, no overlaps; see [User namespaces](run.md#user-namespaces)). They need a `user` namespace, and it needs them. Other fields in an entry are ignored. Mappings that include host ID 0 work (as root), with a warning: a user inside is then root on the host's files. |
| `linux.maskedPaths`, `linux.readonlyPaths` | Exactly the paths hidden or made read-only (see [Root filesystem](run.md#root-filesystem)). **Leaving them out means none**, not Shipyard's defaults. |
| `linux.sysctl` | Kernel settings the container owns: the IPC ones (`kernel.shm*`, `kernel.msg*`, `kernel.sem`, `fs.mqueue.*`), which need an `ipc` namespace, and the network ones (`net.*`), which need a `network` namespace (but not conntrack's `net.netfilter.nf_conntrack_max`, `nf_conntrack_expect_max` and `nf_conntrack_helper`, which are the whole machine's). Any other one would change the host, so it's an error (exit 125), not ignored. Values must be strings. |
| `linux.netDevices` | The host's network devices to move into the container's `network` namespace: `{"eth1": {"name": "eth0"}}` moves `eth1` in, named `eth0` there (without `name`, it keeps its own). It needs root, and a `network` namespace. When the container ends, the kernel gives a physical device back to the host's first network namespace **under its name in the container** (or `devN` if that's taken), without its addresses, and destroys a virtual one (a veth, say) with the container's namespace. If one of the devices can't be moved, the ones already moved are moved back, under their own names, before the bundle fails. Moving the device the host reaches its network through cuts the host off until the container ends. |
| `linux.resources` | The container's limits, as `runc` reads them: `memory` (`limit`, `reservation`, `swap` as memory **plus** swap; `-1` is no limit, `0` not set), `cpu` (`shares`, `quota`, `period`, `cpus`, `mems`), `pids.limit`, `blockIO` (`weight`, and the four `throttle…Device` lists), and `unified` (other files of the controllers, such as `"memory.high": "400M"`, written last). cgroup v1's units are converted to v2's (shares to `cpu.weight`, block I/O weights to `io.weight`). It needs a cgroup of the container's own: root's, or rootless in your delegated subtree (see [Cgroups](#cgroups)); what cgroup v2 can't apply is refused (see [What's refused](#whats-refused)). |
| `linux.cgroupsPath` | Where [`shipyard-runtime`](runtime.md) puts the container's cgroup: an absolute path is below `/sys/fs/cgroup` as root (below your delegated subtree, rootless), a relative one below `/sys/fs/cgroup/shipyard/runtime/`. It must not exist yet (no joining another container's cgroup), nor be below a cgroup that holds processes (another container's, say: it would share that one's limits, freezing and kill), and `.` and `..` aren't allowed, nor a place inside `shipyard/run/` (`shipyard run`'s). Its parents get only the controllers the limits need. systemd's `slice:prefix:name` form is refused. `shipyard run --bundle` ignores it, with a warning: each run's cgroup is its own. |
| `linux.resources.devices` | Device rules, as `runc` reads them: `{"allow": false, "access": "rwm"}` entries with an optional `type` (`a` for both, the default, `c` or `b`), `major` and `minor` (missing or `-1`: any) and `access` (any of `rwm`; all three if missing). Later rules win, for each of read, write and mknod. Shipyard adds `runc`'s list after them: `mknod` of any device, and `null`, `zero`, `full`, `random`, `urandom`, `tty`, `ptmx`, `/dev/pts/*` and `/dev/net/tun`. **A bundle may use only those, and what its rules allow**, as with `runc`: a `linux.devices` node needs a rule of its own here to be opened. At most 256 rules, runc's list included. See [Devices](run.md#devices). |
| `linux.devices` | Device nodes made in the container (outside `/dev`, in the root filesystem's own directory, where they stay): `path`, `type` (`c`, `u` (the same), or `b`), `major`, `minor`, `fileMode` (`0666` if missing), `uid` and `gid` (0 if missing). In a user namespace, where nodes can't be made, the host's node at the same path is bound in instead: it must be there, and be the same device. FIFOs (`p`) are refused. |
| `mounts` | See below. |
| `hooks` | Programs run at points of the container's life. See [Hooks](#hooks). |

Every container also gets a new, empty **session keyring** (`man 7 keyrings`), as runc
gives one by default: the keys the caller's login session holds stay out of its reach,
even without a seccomp profile. There's no `--no-new-keyring` to share the caller's.
Too many containers for one user can run into the kernel's key quota
(`kernel.keys.maxkeys`), which `run` and `create` report (exit code 125).

## Mounts

- **`bind`:** a host file or directory, seen inside too. The `source` is the host path:
  a relative path is relative to the bundle, and it must exist. Use the option `rbind`
  to include the mounts below the source. Otherwise (the option `bind`, or none) only
  the source's own mount is bound, as with `runc`.
- **`tmpfs`:** a fresh, empty file system in memory. The options `size=`, `mode=`,
  `nr_inodes=`, `nr_blocks=`, `uid=` and `gid=` are passed on to it (one value each:
  no commas).
- **Options for both:** `ro`/`rw`, `nosuid`/`suid`, `nodev`/`dev`, `noexec`/`exec`.
  - `ro` with `rbind` makes the mounts below read-only too.
  - A bind keeps the restrictions of the host mounts it comes from: `exec` doesn't
    undo the host's `noexec`, and a host mount that's read-only stays read-only, for
    example.
  - The propagation options `private`, `rprivate`, `slave` and `rslave` are accepted:
    the container's mounts never reach the host anyway.
  - Access-time options (`noatime`, `relatime`, `strictatime`, `nodiratime`,
    `lazytime` and their opposites) are ignored, with a warning: a bind keeps its
    source's, a tmpfs the kernel's default.
  - Any other option is an error.
- **The destination** is the path inside the container. It's created if it's missing:
  a directory, or an empty file when a file is bound. What's created stays in the root
  filesystem afterwards, as with `runc`. A symbolic link on the way is
  followed *inside* the root filesystem, never out to the host's files.
- **Not allowed:** destinations in `/proc` or `/sys`, at `/dev` itself, or `/`.
  `/dev/...` below it is fine. A destination that a symbolic link in the root
  filesystem leads into a kernel interface (`/proc`, `/sys`, the cgroup tree,
  `/dev/pts` or `/dev/mqueue`) is refused too (exit 125). Naming `/dev/pts` or
  `/dev/mqueue` directly is fine.
- **The standard mounts** (`/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`,
  `/dev/mqueue`), which most configs list, are accepted. Shipyard always mounts them
  itself, with its own options.
- **The cgroup mount,** type `cgroup` or `cgroup2` at `/sys/fs/cgroup` (as `runc spec`
  writes it), mounts the container's own cgroup there, read-only if its options
  include `ro`, and always `nosuid`, `nodev` and `noexec`; its other options are
  ignored. It's cgroup v2 either way. Without `ro`, root in the container can change
  its cgroup's settings. It's left out, with a warning:
  - without a `cgroup` namespace, where it would show the host's whole cgroup tree;
  - when the container has no cgroup of its own (rootless without delegation, or no cgroup v2; see
    [Cgroups](#cgroups)).

## Cgroups

A container gets a **cgroup** of its own, as with `shipyard run` (see
[Cgroup](run.md#cgroup)): `shipyard/run/PID` for `shipyard run --bundle`, and
`shipyard/runtime/ID` for [`shipyard-runtime`](runtime.md), below `/sys/fs/cgroup` as
root, or below the cgroup subtree delegated to you, rootless. Every process of the container is in it, and it's
removed when the container ends; `shipyard-runtime` puts it where `linux.cgroupsPath`
says, if the config gives one. The `linux.resources` limits are written into it before
anything runs there. `linux.cgroupsPath` counts from the same base: rootless, an
absolute path is below your delegated subtree. Where the container can't have a cgroup
(nothing delegated, cgroup v1), a bundle that asks for limits is refused, and its
`cgroupsPath` is ignored, with a warning.

A root filesystem in a user namespace needs a `pid` namespace too: its `/proc` can only
be mounted by the root of a PID namespace's own user namespace, so a bundle without one
is an error (exit 125), naming that.

## Hooks

`hooks` lists programs to run at fixed points of the container's life, phase by
phase, as the OCI runtime specification describes them. Tools use them to prepare
what the container needs from the outside: devices (GPU toolkits), network setup,
cleanup.

```json
"hooks": {
  "createRuntime": [
    {"path": "/usr/local/bin/setup-net", "args": ["setup-net", "--quiet"],
     "env": ["PATH=/usr/bin:/bin"], "timeout": 10}
  ],
  "poststop": [{"path": "/usr/local/bin/cleanup"}]
}
```

| Phase | When | Run where | `status` |
|---|---|---|---|
| `prestart` | During create, after the container's namespaces and mounts are made, before its root filesystem becomes `/`. Deprecated in the specification; runs just before `createRuntime`. | Outside, in Shipyard's namespaces. | `creating` |
| `createRuntime` | Right after `prestart`. | Outside, in Shipyard's namespaces. | `creating` |
| `createContainer` | Right after `createRuntime`, still before the root filesystem becomes `/`. | In the container's namespaces; its `path` is a path on the host. | `creating` |
| `startContainer` | At start, before the program. | Inside the container: its `path` is inside the root filesystem. | `created` |
| `poststart` | Once the program has been started. | Outside. | `running` |
| `poststop` | Once the container is gone. | Outside. | `stopped` |

Each hook gets:

- **its own `args` and `env`, exactly.** `args` works like `execv`'s arguments: the
  first is the program's own name (`argv[0]`); none means just the path. `env` is
  the hook's whole environment (`KEY=VALUE` entries): nothing is inherited, not even
  `PATH`. (The specification doesn't say; `runc` passes its own environment when `env`
  is missing.)
- **the container's state on its standard input,** the JSON `shipyard-runtime state`
  prints, with that phase's `status`. `pid` is the container's init's process ID as
  the hook sees it, as the specification says: from outside for the hooks that run
  outside (from `prestart` on), and the container's own (1, with a `pid` namespace) for
  `createContainer` and `startContainer`; and `id` is the container's ID
  (`run-PID` for `shipyard run --bundle`, after shipyard's own process ID).
- **Shipyard's standard output and error,** or the container's for `createContainer`
  and `startContainer`, and no other open file.
- **For `createContainer` and `startContainer`, the container's device rules:** they
  run in its cgroup. A hook that opens a device (a GPU's, say) needs it allowed in
  `linux.resources.devices`, as the toolkits that write such hooks add it.
- **The bundle as its working directory,** so relative paths in its arguments mean the
  bundle's files. (The specification doesn't say; `runc` leaves each phase wherever its
  own process happens to be.) `startContainer` runs at `/` inside the container
  (and any hook runs at `/` if the bundle directory is gone by then).

The hooks of a phase run one after the other, in order. A hook fails if it exits with
anything but 0, is killed by a signal, or can't be started. **`timeout`** is in
seconds: past it, the hook is killed (SIGKILL) and fails. There's no default: a hook
without a `timeout` that never ends stops its phase for good, as with `runc`.

A failure in a phase up to `startContainer` means the container doesn't run:

- **`prestart`, `createRuntime`, `createContainer`:** `shipyard run --bundle` fails
  with exit code 125 (`shipyard-runtime create` with 1), naming the hook (`the createRuntime hook /usr/local/bin/setup-net
  (hooks.createRuntime[0]) exited with code 1`). The rest of that phase doesn't run,
  nor do later phases, except `poststop`: the container is destroyed, and its
  `poststop` hooks run, as the specification says.
- **`startContainer`:** `shipyard run --bundle` fails the same way. With
  `shipyard-runtime`, the hook runs after `start` returns. The container ends without
  running its program, the hook's failure goes to the container's standard error, and
  the container's exit is recorded as code 125.

A failing `poststart` or `poststop` hook is only a warning, as specification 1.2 says
(1.3 makes a failing `poststart` stop the container): the rest of its phase
runs, and so does the container.

**While a `poststart` hook runs** under `shipyard run --bundle`, the container's output
(on a terminal) and your input wait for it: Shipyard passes them on between its own
steps. A container with a lot to say meanwhile waits once its pipe is full, so a
`poststart` hook that waits for the container's output should have a `timeout`.

**Hooks run with Shipyard's privileges:** as root, a bundle's hooks run as root on the
host, outside the container. A bundle with hooks is code you run: only use bundles you
trust. Rootless, they run as you.

**Except `startContainer`,** whose program comes from the container's root
filesystem, so the image decides what it is. It runs as the container's root, but with
no capability other than `CAP_KILL`, with `no_new_privs`, and under the container's
seccomp profile: never with Shipyard's power, which an image could otherwise take
over. (`runc` runs it as `process.user`, with the program's capabilities; a hook that
needs a capability belongs in `createContainer`, whose program is the host's.) Since
they run in processes of their own inside the container, its program isn't PID 2 then,
but comes after them.

With hooks, the bundle's path must be UTF-8: the state the hooks get is JSON.

## Rootless bundles

Without root, a bundle runs **rootless**, as `shipyard run` does (see
[Rootless](run.md#rootless)):

- **No `user` namespace in the config:** one is added, with your automatic maps (you
  as root inside, and your `/etc/subuid` range when `newuidmap` is there), and a
  warning says so. A user namespace only adds confinement. (`runc` refuses such a
  bundle instead.)
- **The bundle's own mappings:** your own ID alone (`"hostID"` your uid, `"size": 1`)
  is written directly. Anything more goes to `newuidmap`/`newgidmap`, which allow only
  your ranges in `/etc/subuid` and `/etc/subgid`: other IDs fail with what the helper
  said.
- **`process.user`** must be mapped: with your own ID alone, only `0`.
- **With your own ID alone,** the namespace denies `setgroups`, as for `shipyard run`
  (see [Rootless](run.md#rootless)): `process.user.additionalGids` is refused
  ("supplementary group … can't be set"), and the process keeps your supplementary
  groups.
- **The root filesystem** you own is root's inside.

## What's refused

Some fields ask for confinement Shipyard can't provide yet. Running without it would
leave the container **less confined than the config asks**, so the bundle is refused
(exit code 125; `shipyard-runtime`: 1). Every such field is listed at once, so you can fix them all:

- `process.apparmorProfile`, `process.selinuxLabel`, `linux.mountLabel`;
- `time` namespaces, and joining an existing namespace other than a `network` one
  (a namespace with a `path`, `user` and `cgroup` included);
- in `linux.resources`, what cgroup v2 can't apply: a kernel memory limit (`memory.kernel`, `kernelTCP`;
  `-1` is accepted, as "unlimited"), `memory.swappiness` (`-1` is accepted, as "the
  default"), `memory.disableOOMKiller: true`, real-time CPU time
  (`cpu.realtimeRuntime`, `realtimePeriod`), `cpu.burst` and `cpu.idle` (for now),
  `blockIO.leafWeight` and `weightDevice`, `hugepageLimits`, `network` and `rdma`;
- FIFOs in `linux.devices` (`"type": "p"`);
- `linux.cgroupsPath` in systemd's `slice:prefix:name` form;
- `process.terminal: true`;
- `process.scheduler`, `process.ioPriority` and `process.execCPUAffinity`;
- `linux.intelRdt`, `linux.memoryPolicy`, `linux.personality` and
  `linux.timeOffsets`;
- `shared` propagation (`linux.rootfsPropagation`, or a mount option), which would
  let the container's mounts reach the host, and `unbindable` propagation, which
  comes later;
- mounts of any type but `bind` and `tmpfs`.

A config made by `runc spec` asks for a terminal, so it's refused until that arrives.
Set `terminal` to `false` to run it with what Shipyard has now. Its capabilities,
`noNewPrivileges`, device rule and network namespace are applied.

## What's ignored

These don't confine the container, so the bundle runs without them, with a warning
(`shipyard: warning: …`, or `shipyard-runtime: warning: …` from the
[runtime](runtime.md)):

- the cgroup mount, without a `cgroup` namespace or a cgroup of the container's own
  (see [Mounts](#mounts));
- access-time mount options (see [Mounts](#mounts));
- fields inside `linux.seccomp` that Shipyard doesn't know (each is named).

Other warnings say what the bundle gets that it didn't ask for, or asked for
unsafely: no `process.capabilities` (none given), a `user` namespace added for a
rootless run, only your own ID mapped (no `newuidmap` or range), and mappings that
include host ID 0.

`process.consoleSize` is ignored silently: it only applies with a terminal, which is
refused. Fields the specification doesn't define are ignored silently, as it requires.

## Errors

All of these exit with code 125 (`shipyard-runtime`: 1) before anything starts, naming
the problem:

- **`config.json`:** missing, not a regular file, larger than 4 MiB, or not valid JSON
  (with the line and column).
- **The version:** an `ociVersion` other than 1.x.
- **The process:** none, or `args` empty; a `uid`, `gid` or `additionalGids` entry of
  4294967295, which the kernel reads as "leave unchanged".
- **Capabilities:** an unknown name, or sets the kernel wouldn't accept: an effective
  capability that isn't permitted, an inheritable one outside the bounding set, an
  ambient one not both permitted and inheritable. The message names the rule and the
  capability.
- **The root filesystem:** `root.path` missing.
- **Namespaces:** an unknown type.
- **Mounts:** a mount source that doesn't exist, or a mount option Shipyard doesn't
  know.
- **Hooks:** a `path` that isn't absolute, a `timeout` of 0 or less, or an `env`
  entry without `=` (named as `hooks.PHASE[N]`).

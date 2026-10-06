# OCI bundles

```sh
sudo shipyard run --bundle ./mybundle
```

An **OCI bundle** is the standard way to describe one container: a directory with a
`config.json` that says what to run and how, and a root filesystem. It's the format
of the [OCI runtime specification](https://github.com/opencontainers/runtime-spec),
which `runc`, Podman and other tools read too.

`--bundle DIR` runs the bundle in DIR. The config is the whole description, so
`--bundle` takes no command and none of the options that describe the container
(`-e`, `-w`, `-u`, `--group-add`, `--ulimit`, `--hostname`, `--domainname`,
`--sysctl`, `--rootfs`, `--read-only`, `--no-isolation`). `--stop-signal` and `--stop-timeout` still work: a
config has no such settings. It needs root, as any isolated run does, and Linux.

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
| `linux.namespaces` | Which of `pid`, `mount`, `uts` and `ipc` the container gets. `mount` is required (for the root filesystem). Without `pid`, there's no small init, and the command shares the host's process IDs (`shipyard-runtime` requires `pid`). |
| `linux.maskedPaths`, `linux.readonlyPaths` | Exactly the paths hidden or made read-only (see [Root filesystem](run.md#root-filesystem)). **Leaving them out means none**, not Shipyard's defaults. |
| `linux.sysctl` | Kernel settings the container owns: the IPC ones (`kernel.shm*`, `kernel.msg*`, `kernel.sem`, `fs.mqueue.*`), which need an `ipc` namespace. Any other one would change the host, so it's an error (exit 125), not ignored. Values must be strings. |
| `mounts` | See below. |

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
  filesystem leads into `/proc` or `/sys` is refused too (exit 125).
- **The standard mounts** (`/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`,
  `/dev/mqueue`), which most configs list, are accepted. Shipyard always mounts them
  itself, with its own options.

## What's refused

Some fields ask for confinement Shipyard can't provide yet. Running without it would
leave the container **less confined than the config asks**, so the bundle is refused
(exit code 125). Every such field is listed at once, so you can fix them all:

- `process.capabilities` and `process.noNewPrivileges: true`;
- `linux.seccomp`, `process.apparmorProfile`, `process.selinuxLabel`,
  `linux.mountLabel`;
- `network`, `user`, `cgroup` and `time` namespaces, and joining an existing one (a
  namespace with a `path`);
- `linux.uidMappings` and `linux.gidMappings`;
- `linux.resources` that asks for anything (empty lists and objects are fine), and
  `linux.devices`;
- `process.terminal: true`;
- `process.scheduler`, `process.ioPriority` and `process.execCPUAffinity`;
- `linux.intelRdt`, `linux.memoryPolicy`, `linux.personality`, `linux.netDevices`
  and `linux.timeOffsets`;
- `shared` propagation (`linux.rootfsPropagation`, or a mount option), which would
  let the container's mounts reach the host, and `unbindable` propagation, which
  comes later;
- mounts of any type but `bind` and `tmpfs`.

A config made by `runc spec` asks for a terminal, a few capabilities,
`noNewPrivileges`, a device rule under `linux.resources` and a network namespace, so
it's refused until those arrive. Remove those fields (and set `terminal` to `false`) to
run it with what Shipyard has now: the process then runs as root with root's full
privileges, which is what Shipyard does today.

## What's ignored

These don't confine the container, so the bundle runs without them, with a warning
(`shipyard: warning: …`):

- `hooks`;
- `process.oomScoreAdj`;
- `linux.cgroupsPath`;
- the `cgroup` or `cgroup2` mount at `/sys/fs/cgroup`;
- access-time mount options (see [Mounts](#mounts)).

`process.consoleSize` is ignored silently: it only applies with a terminal, which is
refused. Fields the specification doesn't define are ignored silently, as it requires.

## Errors

All of these exit with code 125 before anything starts, naming the problem:

- **`config.json`:** missing, not a regular file, larger than 4 MiB, or not valid JSON
  (with the line and column).
- **The version:** an `ociVersion` other than 1.x.
- **The process:** none, or `args` empty; a `uid`, `gid` or `additionalGids` entry of
  4294967295, which the kernel reads as "leave unchanged".
- **The root filesystem:** `root.path` missing.
- **Namespaces:** an unknown type.
- **Mounts:** a mount source that doesn't exist, or a mount option Shipyard doesn't
  know.

# Exit codes

Shipyard's exit codes follow Docker's, so existing scripts keep working.


When the command runs and exits, its exit code passes through unchanged,
whatever it is (0–255). The codes below have a special meaning:

| Code | Meaning |
|---|---|
| `125` | **Shipyard failed.** The command never ran. Causes include an unknown option, an invalid value (signal, limit, user, group, environment variable, hostname, sysctl), `-it` when your input isn't a terminal ("the input device is not a TTY"), a relative working directory, isolation refused by the system (user namespaces disabled, a container without privileges, or a security policy), a user namespace that can't be set up (`newuidmap` failing, a user or group the maps leave out), a seccomp profile that can't be read or installed (or that refuses what the container's init needs), supplementary groups asked for where the user namespace denies `setgroups`, a root filesystem that can't be used (`--rootfs`), a bundle that can't be run (a bad `config.json`, or fields that are refused), a user or group name the container's `/etc` doesn't have, a limit that can't be applied (no cgroup of the container's own, a controller not delegated, a value the kernel refuses), a device given with `--device` that isn't one, a kernel that refuses the device program as root, a bundle's hook that failed before the program (`prestart`, `createRuntime`, `createContainer`, `startContainer`), or a host problem. The message on stderr says which. |
| `126` | **The command could not be executed.** It isn't executable, is a directory, isn't a program this system can run (a binary built for another CPU, or a script with no `#!` line), or the user hit its process limit (`nproc`) as it started. If Shipyard itself can't create the process (the whole system is out of processes), that's `125`. |
| `127` | **The command was not found.** Also when the file exists but its `#!` interpreter (or, for a binary, its loader) is missing; the message says so. |
| `128 + N` | **The command was killed by signal N.** |

A command can also exit with 125–127, or above 128, by itself. If you need to
tell the cases apart, check stderr: when the code comes from Shipyard, the
message starts with `shipyard:` or `error:`. (One exception: `shipyard` with
no command at all prints its help and exits with `125`.)

## Common signal codes

| Code | Signal | Usually means |
|---|---|---|
| `130` | `SIGINT` (2) | Interrupted with Ctrl-C |
| `134` | `SIGABRT` (6) | The program aborted itself |
| `137` | `SIGKILL` (9) | Killed forcibly, for example after `--stop-timeout` expired, or by the kernel's OOM killer past `--memory` (Shipyard then says "out of memory") |
| `139` | `SIGSEGV` (11) | The program crashed (segmentation fault) |
| `143` | `SIGTERM` (15) | Asked to stop, and it did |
| `159` | `SIGSYS` (31) | A system call the seccomp profile kills for (`SCMP_ACT_KILL_PROCESS`, `SCMP_ACT_KILL`) |

## In scripts

```sh
shipyard run my-job
case $? in
  0)   echo "succeeded" ;;
  125) echo "shipyard rejected the job; see the message above" ;;
  126) echo "my-job is not executable" ;;
  127) echo "my-job was not found" ;;
  137) echo "my-job was killed" ;;
  *)   echo "my-job failed" ;;
esac
```

`--help` and `--version` exit with `0`.

## `shipyard-runtime`

The [OCI runtime](runtime.md) runs no command itself, so its codes are simpler: `0`
when the call did what was asked, `1` when it failed (the message says why), and `2`
for a command line it can't parse. How a container's program ended is in its state
directory, not in these codes; it ends in "(out of memory)" when the kernel's OOM
killer ended it, or "(the host or a parent cgroup ran out of memory)" when the
container hadn't reached its own memory limit.

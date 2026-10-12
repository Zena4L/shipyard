# Changelog

All notable changes to Shipyard. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) (before 1.0, a minor version may change
behaviour; each change is listed here).

## Unreleased

### Changed

- **`shipyard-runtime attach` exits 255 when it fails itself** (no such container,
  the monitor gone, …), as `exec` does: any other code is the container's. It exited 1,
  which a container's own exit code 1 looked the same as.
- **Other firewalls** (stage 8): a run on a bridge network recognises more of the
  firewalls that would drop its containers' traffic, and says what each would keep
  from them, with the fix in that firewall's own terms:
  - **firewalld**, which rejects by a rule rather than its policy and went
    unnoticed: on Fedora, a network's DNS server gets no questions until its bridge
    is in firewalld's trusted zone;
  - **ufw**, with its own `ufw route allow` rules;
  - a chain that ends in a reject (RHEL's `iptables-services`).

  The fix for Docker's firewall is now complete. Containers reach each other and
  their published ports too, not just the outside, and a network with IPv6 gets
  the `ip6tables` rules. A fix you've applied is recognised and not warned about
  again: the rules have to be the ones printed, so a rule for a former way out
  doesn't count. `shipyard doctor` has a `firewall` line. Shipyard still never
  changes another program's firewall.
- **A container never gets your terminal** (stage 8), from `shipyard run` or
  `shipyard-runtime exec`: when Shipyard's stdin, stdout or stderr is a terminal, the
  command gets a pipe instead, which Shipyard relays (`/dev/null` for `exec -d`), so
  it can't type into your shell or change the terminal's settings, nor keep your
  terminal through a process it leaves behind. (A container `shipyard-runtime create`
  makes from a terminal, without `--log-dir`, keeps it, as with runc.)
  Programs inside see no terminal (`tty` says "not a tty"); `--no-isolation` keeps
  it, and without `--image` or `--rootfs` the container's `/dev/pts` is its own. A
  `shipyard run -i … &` in the background now waits for `fg` instead of taking your
  shell's keys.
- **Shipyard's binary file never runs where the container could write it**
  (stage 8): on a host that won't make or run a sealed copy (`vm.memfd_noexec` at 2,
  or a security module's policy), the file itself ran in a container's init with a
  warning, and in a rootless `exec` too. It now runs only where nobody in the container
  could write it; otherwise the run, `create` or `exec` is refused. A root container
  without a user namespace is always refused there, and so is a rootless one when
  Shipyard is installed in your home: install it where root owns it. A hard file-size
  limit (`ulimit -f`) smaller than the binary also leaves no copy, and the error says
  so; a soft one (`ulimit -S -f`) is lifted while the copy is made.
- **Input only with `-i`** (stage 8): an isolated command's stdin is empty
  (`/dev/null`) unless `-i` (`--interactive`) is given, as with Docker. Scripts that
  pipe input into `shipyard run` need `-i`.

### Added

- **Passing descriptors** (stage 8): `--preserve-fds N` for `shipyard-runtime create`
  and `exec` (runc's) and `shipyard run` (Podman's) passes descriptors 3 and up on to
  the container's process; socket activation (`LISTEN_FDS`, `LISTEN_PID` naming
  Shipyard's process) passes a service manager's listening sockets on, with
  `LISTEN_PID` set to the process's own PID, as `sd_listen_fds` expects. Only the
  process holds them once it runs.
- **cp** (stage 8): `shipyard-runtime cp ID:SRC DEST` and `cp SRC ID:DEST` copy files
  and directories out of and into a created, running or paused container, with
  Docker's path rules, `-` for a tar stream on stdout or stdin, `-a` to keep owners and
  `-L` to follow a link. The container side is done inside the container by a helper
  of Shipyard's own: a link the container plants (or swaps in while the copy runs)
  never leads to the host's files. Files copied out are yours, without setuid bits;
  device nodes aren't copied.
- **Exec** (stage 8): `shipyard-runtime exec ID COMMAND` runs a command inside a created
  or running container, runc's way: in all its namespaces (rootless too) and its
  cgroup, confined as its own process is (from a snapshot taken when it started, not
  the bundle again), with `--env`, `--cwd`, `--user`, `--cap`, `--no-new-privs` and
  `--process` able to narrow that, never widen it; `-d` to leave it running; `-t` for a
  terminal made in the container (relayed to yours, or sent to `--console-socket`,
  runc's protocol). It exits with the command's code, or 255 if it couldn't run it.
  Nothing in the container can reach it before it runs.
- **`-i`/`--interactive`** passes your input on to the command; `-it` when your
  input isn't a terminal is refused as Docker refuses it.
- **`-t`/`--tty`: a terminal of the container's own** (stage 8): a pseudo-terminal
  made inside the container, sized as your window and resized with it, the
  command's controlling terminal. With `-it` your terminal is raw while it runs,
  so `vi`, `top`, password prompts (`su`, `sudo`) and job control inside work;
  it's put back when the run ends or Shipyard is stopped. `-t` works without
  isolation (and on macOS) too.
- **Attach** (stage 8): `shipyard-runtime attach ID` shows a container's output as it
  comes and passes your typing on (`create --stdin`; `--stdin-once` closes its input
  when the first client to type goes); for a container with a terminal, your terminal
  goes raw and follows its size. Ctrl-P Ctrl-Q (`--detach-keys`) detaches; otherwise `attach`
  ends with the container's exit code. Up to 16 clients; one that falls behind is let
  go, never slowing the container; only the container's owner (or root) may attach.
- **Container logs** (stage 8): `shipyard-runtime create --log-dir DIR` keeps a
  container's output in CRI-format log files (Kubernetes', as containerd and Podman
  write them), rotated (10 MiB × 5 by default; `--log-max-size`, `--log-max-files`),
  versioned by a `log.json` beside them; the monitor reads the output, and the logs
  outlive the container. `shipyard-runtime logs [--follow] [--tail N] [--since TIME]
  [--timestamps] ID` reads them back. The monitor's own memory stays under 1 MiB
  idle (up to 4 MiB more with 16 attached clients that read slowly).
- **OCI terminals** (stage 8): bundles that ask for a terminal
  (`process.terminal`, as every `runc spec` config does) run. `shipyard run
  --bundle` gives them one as `-t` does, sized by `process.consoleSize` when there's
  no window to copy; `shipyard-runtime create --console-socket` sends it to the
  caller's socket as runc does, so containerd's and Podman's shims can use it.

### Fixed

- **Killing `shipyard-runtime attach`** (`kill`, a hang-up) left your terminal raw
  when the container has a terminal: it's now put back first, however attach ends
  but `kill -9`.
- **A full disk no longer costs a container's oldest logs**: a line that couldn't be
  written made the logs rotate, dropping the oldest file for an almost empty one. Only
  a file-size limit rotates early now. A write cut short is cut back by the file's own
  length, so a log file someone truncated by hand gets no hole of NUL bytes.
- **`logs` reads only the directory a container was created with**, by identity, not
  path: one put at its path since is refused, `logs --follow` ends when the ID and the
  path are both used again, and a failed `create --pid-file` removes only the log
  directory it made (it removed whatever was at the path, by path). `logs --follow`
  waits for a `container.log` deleted by hand to come back, rather than ending.
- **Mounts are made in the order a bundle lists them** (stage 8, found by the OCI
  conformance suite): Shipyard made `/proc`, `/sys` and `/dev` first, whatever the
  order, as the specification forbids. An order that can't work (a mount below `/dev`
  listed before `/dev`, which would hide it) is now refused, saying what to move.
- **`poststart` hooks run once the program has started** (stage 8): `start` ran them as
  soon as the container was let go, before its program's `execve`.
- **A tmpfs's options on a bind mount** (`mode=`, `size=`) are ignored with a warning,
  as `runc` does, instead of refusing the bundle.
- **`runc spec --rootless` configs run:** their `/sys`, a bind of the host's, was
  refused as a mount into `/sys`; it's now the standard `/sys`.
- **A failed `create --log-dir` removes its logs safely:** it removed the directory by
  its path, which someone able to write in its parent could have pointed elsewhere
  meanwhile (a symlink); it now removes it through descriptors, only if it's still the
  one it made.
- **Logs deleted while a container runs:** a deleted `container.log` is replaced at the
  next rotation (writing went on to the deleted file, out of sight, with no limit), and
  logs whose directory is gone are no longer written. A rotation that fails no longer
  cuts the current file short.
- **`logs --follow ID`** ends when the container is deleted even if a new one takes its
  ID at once.
- **Detaching from a container without a terminal works from a terminal:** the
  terminal's flow control took Ctrl-Q for itself, so Ctrl-P Ctrl-Q (and Enter) never
  detached; it's off while attached, and put back when attach ends.
- **`exec` of a program the container doesn't have** says "command not found": it
  looked for the program among the host's files, and named one the host had
  "found, but its interpreter or loader is missing".
- **A container's monitor runs in `/`**, not in the directory `create` was run from,
  whose mount it kept busy as long as the container ran.
- **Removing an image layer with a directory its owner could read but not search**
  (mode `0600`, say: an image can hold one): it failed, and the layer stayed on disk;
  such a directory is now made searchable first, as an unreadable one already was
  (stage 8, found by fuzzing `cp`).
- **`shipyard-runtime create` as root, for a bundle with a user namespace:** the
  container stopped at its start gate (exit 125) before `start`, as its root couldn't
  open the gate; the gate is now that user's (stage 8).
- **Rootless networking works under SELinux** (Fedora; stage 8): Fedora confines
  pasta, and its policy refused how Shipyard gave pasta the container (by process
  ID), so every rootless run with its default network failed there.
- **`--network pasta` works for root** (stage 8), as the docs said: pasta dropped to
  `nobody` before joining the container's namespace, which `nobody` can't, so root's
  pasta runs always failed. pasta now
  gets the container's network namespace as a file, as with Podman.
- **Shipyard's firewall table can be saved and restored with `nft`** (stage 8): one
  of its sets was named `masquerade`, a keyword of nft's language, so `nft -f` refused
  a listing of the table (`nft list ruleset > saved` then `nft -f saved` failed on
  it). It's now `masquerade4`; the table is rebuilt on the next root run.

## 0.1.0 - unreleased

The first milestone: `shipyard run --image alpine sh` rootless on Linux, on your own
terminal, pulled from Docker Hub, isolated, resource-limited and networked. Built in
stages 0 to 7 of the [roadmap](docs/ROADMAP.md), then hardened (stages 7.5 and 7.6).

### Added

- **Running commands** (stage 1): `shipyard run` starts a command with a clean
  environment, a chosen user, working directory and limits; passes signals on, stops it
  gracefully (`--stop-signal`, `--stop-timeout`), cleans up everything it started, and
  reports exactly how it ended (exit codes as Docker's).
- **Isolation** (stages 2 and 3): its own processes (under a small init), mounts,
  hostname and IPC; a root filesystem of its own (`--rootfs`), with `/proc`, `/sys`,
  `/dev`, masked and read-only paths; OCI bundles (`--bundle`); an OCI runtime,
  `shipyard-runtime` (`create`, `start`, `state`, `kill`, `delete`, `pause`, `resume`,
  `update`, `ps`, `events`), as runc.
- **Users and security** (stage 4): rootless containers (user namespaces, with your
  subordinate IDs); a minimal set of capabilities, `no_new_privs`, and a seccomp filter
  (Docker's profile by default, compiled by Shipyard itself).
- **Resource control** (stage 5): cgroup v2 limits (`--memory`, `--cpus`,
  `--pids-limit`, I/O, …), rootless where a subtree is delegated; OOM reporting;
  device access control (eBPF); OCI hooks.
- **Images and storage** (stage 6): `shipyard pull`, `images`, `rmi`, `image prune`,
  `system df`; private registries (Docker's credentials and helpers); resumable
  downloads; safe, verified layer unpacking (fuzzed); a content-addressed store shared
  by concurrent runs; overlayfs, rootless too; named volumes and bind mounts.
- **Networking** (stage 7): a network of the container's own, with a way out and
  published ports (`-p`, `-P`): bridge networks as root, over Shipyard's own netlink and
  nf_tables, and pasta without root; `shipyard network` commands; names and aliases on
  your networks (a DNS server per network); several networks per container; IPv6.
- **`shipyard doctor`** checks whether the host can run containers by trying what a
  run needs (a user namespace, a mount, an overlay next to the image store, a cgroup),
  and says what to fix; exit code 1 if something fails.
- **An AppArmor profile** (`contrib/apparmor/shipyard`) for Ubuntu 23.10 and later,
  which restrict unprivileged user namespaces.

### Security

Stages 7.5 and 7.6 fixed what a review of the whole codebase found
([notes](docs/stages/07.5-hardening.md)). Among them:

- the init runs from a sealed in-memory copy of the binary (CVE-2019-5736's class);
  every entry point refuses a setuid or file-capability start;
- a container can't send as another (a bridge anti-spoofing table), nor poison a
  shared image layer for other images;
- registry credentials never follow a redirect to another host;
- a symlink in an image can't steer a mount onto a kernel interface (CVE-2019-19921's
  class).

### Known limitations

- No terminal of the container's own (`-it`), no detached containers, `exec` or logs
  (v0.2).
- macOS doesn't isolate commands yet (v0.4).
- Rootless containers can't share a network.
- Not a hardened security boundary yet: see [SECURITY.md](SECURITY.md).
- Open findings are tracked in the [roadmap](docs/ROADMAP.md#open-findings).

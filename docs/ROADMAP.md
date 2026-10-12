# Roadmap

Each stage builds directly on the previous one and ends with something runnable and tested.
What each stage contains in detail is in [FEATURES.md](FEATURES.md).

Chapter numbers refer to *The Linux Programming Interface* (TLPI). TLPI predates mature
namespaces and cgroups, so those stages also use the man pages (`namespaces(7)`,
`cgroups(7)`, …) and Michael Kerrisk's LWN series
["Namespaces in operation"](https://lwn.net/Articles/531114/).

## Stages

| # | Stage | Outcome | Reference | Status |
|---|-------|---------|-----------|--------|
| 0 | **Foundation** | Workspace, lints, CI, docs, UI concept | — | done |
| 1 | **Process lifecycle** *(POSIX: runs natively on macOS)* | `shipyard run -- /bin/sh` spawns, waits, forwards signals, stops gracefully, returns the exit code; reaps and cleans up everything the command started | TLPI 6, 20–22, 24–27, 34 | [done](stages/01-process-lifecycle.md) |
| 2 | **Namespaces** | PID, mount, UTS and IPC namespaces (`clone3`); own session; a minimal PID-1 init; `setns` to join a running container. The other namespaces moved to stages 4, 5 and 7 | TLPI 26, 28, 34; `namespaces(7)`, `pid_namespaces(7)`, `setns(2)` | [done](stages/02-namespaces.md) |
| 3 | **Filesystem isolation & OCI bundles** | `pivot_root`, mount propagation, `/proc` `/sys` `/dev`, masked and read-only paths; OCI `config.json` and `create / start / kill / delete / state`; user/group names; domain name and namespaced sysctls | TLPI 8, 14, 18, 44, 55; `mount_namespaces(7)`, `pivot_root(2)`, `mount_setattr(2)`; OCI runtime spec | [done](stages/03-filesystem.md) |
| 4 | **Users & security** | Rootless (user namespaces, UID/GID maps), capabilities, `no_new_privs`, seccomp. Device access control moved to stage 5, with cgroups | TLPI 8, 9, 39; `user_namespaces(7)`, `capabilities(7)`, `seccomp(2)` | [done](stages/04-security.md) |
| 5 | **Resource control** | cgroups v2 limits and cgroup namespaces, OOM detection, live stats, pause/resume, device access control (eBPF), OCI hooks; `shipyard`'s own `stats`/`top`/`pause`/`update` come with stage 9's named containers | TLPI 36; `cgroups(7)`; OCI runtime spec (hooks) | [done](stages/05-resources.md) |
| 6 | **Images & storage** | Registry pulls (private registries, resumable), OCI/Docker manifests, safe layer unpacking (fuzzed), content-addressed store shared by concurrent runs, overlayfs (rootless too), volumes. `tag`/`inspect`/`save`… and `--mount` come with stage 9; the `fuse-overlayfs` fallback later | OCI image & distribution specs; `overlayfs` docs; TLPI 14, 18, 55 | [done](stages/06-images.md) |
| 7 | **Networking** | Network namespaces, bridge + veth over Shipyard's own netlink, IPAM, NAT and published ports with its own nf_tables client, rootless networking with pasta, a DNS server per network, several networks per container, IPv6, reading a container's sockets. `network connect`/`disconnect` and the sockets command come with stage 9 | TLPI 56–61; `netlink(7)`, `rtnetlink(7)`, `network_namespaces(7)` | [done](stages/07-networking.md) |
| 8 | **Terminals, I/O & logs** | Per-container monitor, PTYs, attach/detach, `exec`, log files with rotation, `cp`; also `--preserve-fds` and socket activation, other firewalls (F14), and the OCI conformance suite (H5) | TLPI 44, 62–64 | done ([notes](stages/08-terminals.md)) |
| 9 | **Engine state** | On-disk state, locking, crash recovery, restart policies, health checks, events | TLPI 37, 55 | |
| 10 | **Compose** | `compose.yaml` subset; `up / down / ps / logs`; dependency-ordered start | Compose spec | |
| 11 | **API** | Versioned REST + event stream over a UNIX socket, socket-activated (still daemonless) | TLPI 57 | |
| 12 | **macOS VM backend** | Shipyard boots and manages its own minimal Linux VM; the Mac CLI drives the runtime inside it | Virtualization.framework, vsock, virtiofs | |
| 13 | **Desktop UI** | The [design](design/README.md), built on the stage 11 API | Tauri (likely) | |
| 14 | **Hardening & release** | OCI conformance suite passing (it runs since stage 8), fuzzing, benchmarks, threat model, signed reproducible releases, packaging | [FEATURES.md §15](FEATURES.md#15-production-quality--ongoing-from-stage-1-finalized-in-stage-14) | |

Each finished stage gets **stage notes** in [`stages/`](stages/): what was built and why,
macOS and Linux differences, the failure modes and how they're handled, known limitations,
and what was carried forward to later stages.

**Ongoing from stage 1:**

- tests for every stage
- the CLI growing with each stage
- the production-quality items in FEATURES.md §15, adopted as soon as they apply
- the [hardening track](#hardening-track), each item from its home stage

Stages 2–9 are Linux-only. Until stage 12 lands, develop them on macOS inside a Lima VM,
and run their tests in a Linux container with `scripts/test-linux.sh` (see
[DEVELOPMENT.md](DEVELOPMENT.md)).

## Hardening track

What looking back at stages 0–7 says to do differently (decided 2026-10-09). Each item
has a home stage; none waits for stage 14. The first two get more expensive with every
stage, so they come first.

| # | Item | Why | Home |
|---|---|---|---|
| H1 | **An engine crate** (`shipyard-engine`): the logic now in the CLI (`run.rs`, `network.rs`, …) behind a typed API; the CLI a thin client of it, with `--json` output | stage 11's API and stage 13's UI need it as a library; the move grows with every stage | stage 9 (start in stage 8 for what it adds) |
| H2 | **One state model, versioned:** one container record and state machine, one liveness rule and one recovery path in place of each stage's own lock-and-sweep; a schema version on every on-disk format from its first write, and a test reading the previous version (the first: stage 8's `log.json`, its version refused when unknown) | today's records (run areas, leases, run directories, volume holds, `dns.lock`, cgroup records) each work alone; upgrades rest on `serde` defaults | stage 9 |
| H3 | **A threat model in every plan:** who is hostile (the container, a local user, the network, a registry, the image) and what each may never do, written as invariants with a test each; the Murphy review run against the plan too | stage 6's tar escapes and stage 7's router advertisements, open router and lockable leases were all found after building | from stage 8 |
| H4 | **Real hosts in CI:** VM jobs on the distributions users run (Ubuntu with AppArmor, Fedora with SELinux, an older LTS kernel), the probes on each | the test container is one privileged Debian on one kernel; stage 7's last failures appeared only on Ubuntu | stage 8 (one VM job), widened by stage 12; Ubuntu 24.04 checked by hand for v0.1 (rootless with the AppArmor restriction on, root bridge networks). **Built in 8.1b:** CI's `apparmor` job (Ubuntu, the restriction on, the installed binaries and profile, a control without it) and `fedora` job (a QEMU/KVM VM, SELinux enforcing, both passes, the terminal probe). *Not yet run on stage 8's code:* CI is manual-only (billing off) and its Ubuntu `check`/`root` jobs are off; stage 8 was checked by `test-linux.sh` and by hand on Ubuntu and Fedora VMs, arm64 only |
| H5 | **Quality tools in CI:** `cargo-mutants` on changed files, the fuzz targets on a schedule, the OCI runtime conformance suite, differential tests of Shipyard's own netlink and nf_tables against `ip` and `nft` | the method works but runs by hand; conformance was planned for stage 14 though the runtime exists since stage 3 | conformance and differential tests in stage 8 (differential tests built in 8.1b: see [Owned components](#owned-components); the conformance suite in 8.7a: `scripts/test-linux.sh --conformance`, gated by `scripts/conformance/expected.txt`, and a CI job, not yet run: amd64 unchecked); mutants and scheduled fuzzing in stage 9 |
| H6 | **Errors as a designed surface:** stable error kinds (and codes) in one module per crate; tests assert the kind, not the wording; messages stay free to improve | wording drifted across steps, and tests matching on wording broke CI twice | with H1 (stage 9), and each new error from stage 8 |
| H7 | **Performance budgets as tests:** run start, run exit, a cached pull, a network attach; a regression fails | pasta's one-second exit delay was found by accident | stage 8 (built in 8.1b: `tests/budgets.rs`, whose documentation lists every budget) |
| H8 | **Owned components decided explicitly:** for each piece Shipyard writes itself instead of using a crate or a tool (netlink, nf_tables, seccomp, eBPF, tar, DNS), the trade-off recorded here, and a differential test against a reference | each is a large, security-relevant surface Shipyard maintains alone | recorded in stage 8 ([below](#owned-components)); tests with H5 |
| H9 | **Build hygiene:** small debug info (done: `line-tables-only`), CI caches, documented clean-up of build volumes | builds reached 30 GB on disk and 65 GB in Docker before anyone looked | done in stage 7; kept |

## Owned components

What Shipyard writes itself instead of using a crate or a tool (H8), why, and what it's
checked against (8.1b). Each differential test runs a second, independent
implementation on the same input; where they differ, one of them is wrong.

| Component | Why it's Shipyard's own | Checked against | Tests |
|---|---|---|---|
| **netlink** (rtnetlink: links, addresses, routes) | No `ip` binary run as root, no C library, and no netlink crate whose dumps and errors would be another surface to audit; it's small and its parser is bounded and fuzzed | `ip -j`: what Shipyard makes, `ip` reads the same, and the reverse (links, addresses, routes and their types; a route's scope is checked by a unit test of what Shipyard sends) | `route/linux_tests.rs`: `ip_reads_what_shipyard_makes`, `shipyard_reads_what_ip_makes` (root) |
| **nf_tables** (`inet shipyard`) | The same: no `nft` run as root; one batch, atomic, from Rust | `nft`: Shipyard's table lists with nothing unknown, its key rules read as meant, nft rebuilds it from its own listing to the same (a fixed point, the same rules per chain and the same elements), and `nft -j` gives each chain the type, hook, priority and policy, and each set the types and flags, Shipyard made them with. Not byte for byte: nft encodes some rules differently but equivalently | `nft/linux_tests.rs`: `nft_reads_and_rebuilds_shipyards_table`, `nft_json_describes_shipyards_chains_and_sets` (root) |
| **seccomp** (a BPF compiler for Docker's profile) | No libseccomp (a C library): the binary stays static, and the filter's every instruction is Shipyard's | The kernel: the filter loads, and the e2e tests see what it allows and refuses; an interpreter runs it as the kernel would | `security/seccomp/`, `tests/security.rs`. A libseccomp differential: stage 9, with H5's tooling |
| **eBPF** (device programs) | No libbpf or clang: a small assembler for one kind of program | The kernel's verifier (it loads them), the e2e device tests, and an interpreter as strict as the verifier | `devices/ebpf.rs`, `tests/devices.rs`. A bpftool differential: stage 9 |
| **tar** (reading layers) | A layer is untrusted: the reader bounds what an archive can make it hold, and nothing is unpacked by path (the `tar` crate's `unpack` isn't used) | The `tar` crate: generated GNU and ustar archives, and PAX ones, read the same (for a PAX `mtime`, owner or extended attribute, which the crate hands over without applying, the reference is the test's own reading). Shipyard reads an empty number field as 0, as GNU tar does; the crate refuses it | `tar_reader/differential.rs` (proptest); the `tar` fuzz target fuzzes Shipyard's reader alone |
| **tar** (`cp`'s: walking a tree, and extracting a copy) | The tree may be a hostile container's: walked by descriptor (`O_PATH`, never followed; only files and directories opened, by reopening what was checked), each file written at its header's size; the `tar` crate only writes the headers (no `append_path`, which follows paths). Extraction is the layer unpacker's, with a copy's rules (no whiteouts, nothing implied, one top-level name) | GNU `tar`: it extracts `cp`'s stream to the same tree `cp` makes (contents, modes, links, hard links), and `cp` extracts GNU tar's to the same | `tests/runtime.rs`: `cp_streams_agree_with_gnu_tar` (root); `cp/pack.rs`; fuzzed (`cp_unpack`) |
| **Timestamps** (RFC 3339, for CRI logs and `--since`) | A few dozen lines of calendar arithmetic (Howard Hinnant's days-to-date) instead of a time crate: UTC only, no time zones to get wrong | GNU `date`: 200 instants from year 0000 to 9999, the same text | `logs/time.rs`: `the_same_as_date_says` (Linux), plus round trips and known instants |
| **CRI logs** (the writer and reader) | The format is a line per record; writing it is the monitor's job (conmon's), and reading it the engine's: no Go tools, no daemon | kubelet's own test lines (read the same), and the round trip of any text; `crictl` or fluent-bit by hand | `logs/cri.rs`, `logs/writer.rs`, the engine's `logs.rs`; fuzzed (`cri_record`, `logs_dir`) |
| **DNS** (a server per network) | No daemon beside the container (as Podman's aardvark-dns or dnsmasq): one small server, started per network, as nobody | `dig` (BIND's library): names, aliases, case, other types, reverse, TCP, upstream, refusal | `tests/dns.rs`: `dig_reads_the_servers_answers_as_meant` (root) |

What the tests found when first run (8.1b): the set `masquerade` was a keyword of nft's
language, so `nft -f` couldn't read back a listing of Shipyard's table (renamed
`masquerade4`, ruleset 6); and `Rtnetlink::add_route` wrote every route as unicast (it
now writes the route's type).

## Open findings

A review of the whole codebase after stage 7 (2026-10-09; five reviewers, one per area)
found these; each was then re-checked against the code by independent agents, which
corrected several (the severities and findings below are the corrected ones). Stage 7.5
(hardening) fixed the first group, and 7.6 the stage 1–7 findings the review had left (F18's pins, F19–F23, F25–F28); the rest have their stage. Each fix
comes with a test that reproduces the problem first. Remove a row when its fix lands.

| ID | Severity | Finding | Fix | Home |
|---|---|---|---|---|
| F6 | high | A joined process runs unconfined (no capability drop, no seccomp); not reachable yet (no `exec` command); `/proc/<joiner>` and `exe` patterns (CVE-2016-9962, CVE-2019-5736, CVE-2024-21626) | confine it, non-dumpable before `setns`, no host procfs descriptors | **closed in 8.4a**: a joiner process from the sealed copy, non-dumpable before any of the container's namespaces, descriptors really closed, the container's confinement from a snapshot; the terminal in 8.4b, made by the joiner through the init's root, a pty master or refused |
| F10 | low | No per-source conntrack limit: one container can fill the host's connection table (the `ct state invalid drop` landed in 7.5) | `ct count` per source, as an option (D10) | 9 |
| F14 | medium | firewalld rejects by rule, not policy: containers have no way out there and no warning; the Docker fix leaves published ports broken | detect firewalld's table (suggest `--zone=trusted`); complete the Docker fix | **closed in 8.6**: what firewalld rejects is read from its rules (a network's DNS server; the way out where its zone doesn't forward), its trusted zone suggested; Docker's fix complete (same bridge, way out, replies and published ports; `ip6tables` with IPv6); ufw's own rules; chains ending in a reject; a fix is recognised only rule by rule, in the right direction; `doctor`'s `firewall` line; the printed iptables and nft fixes run in tests, firewalld's against its 2.4 layout and a real Fedora VM; the Fedora VM job keeps firewalld running |
| F18 | low | CI: one distribution; no fuzzing or mutation in CI (actions are pinned by commit since 7.6) | H4, H5 | 8–9 |
| F24 | low | An OCI bind source is checked at load but resolved again by path at start: swapped for a symlink in between, another host path is bound (runc shares it) | open it with `O_PATH`/`O_NOFOLLOW` at load and carry the descriptor through the init's re-exec; it needs write access above another bundle's source, so it waits | later |
| F29 | low | For a program embedding the runtime library: the subreaper reaps and kills its own children and stays set; SIGPIPE can kill it if the child dies before go; the parent-death signal is tied to the thread that forked | scope reaping to the command's tree and reset the subreaper; `MSG_NOSIGNAL`/blocked SIGPIPE; document "the main thread" | 11 (the API) |
| F30 | low | DNS TCP slots still per client; no opt-in to keep containers from the host's own services and the cloud metadata address (169.254.169.254); internal networks reach host services bound to every address | per-lease TCP slots; an input chain option | 9 |
| F31 | low | Stages 7.5 and 7.6's residuals: link-local IPv6 isn't tied to its container; a store root that's a symlink is only warned about; a root filesystem's own device nodes open where no device program guards; a symlink can steer a volume onto Shipyard's tmpfs covers in `/sys`, or onto securityfs, debugfs or the like below a bundle's own recursive `/sys` bind; a container that hit its memory limit, then lost a process to the host's OOM killer, reads "out of memory"; `--blkio-weight` doesn't set BFQ's `io.bfq.weight` (runc does); `update --cpu-period` alone clears the quota (runc keeps it); a `delete` of a killed `create` can remove another `--root`'s same-ID cgroup in the moment before its init joins it (that `create` fails) | each as noted in `stages/07.5-hardening.md` | later |
| F32 | medium | The OCI conformance suite (8.7a): mounts aren't made in the config's order: Shipyard mounts the standard ones (`/proc`, `/dev`, `/sys` …) itself first, so `/dev/mqueue` comes after `/sys` where the config lists it before (the spec: "MUST mount entries in the listed order"); 11 of the suite's tests fail on it alone, and one more on it among other reasons | mount every entry in the listed order, the standard ones with Shipyard's own options | **closed in 8.7b**: one ordered plan: the standard mounts made where the config lists them (those it doesn't, first, or after their listed parent), and an order that would hide a mount refused; the 11 tests pass |
| F33 | low | The conformance suite's `mounts` test puts tmpfs data options (`mode=`, `size=`) on bind mounts: Shipyard refuses them as unknown, runc passes them to `mount(2)`, which ignores them for a bind | accept and ignore them on a bind, with a warning (or keep refusing: decided there) | **closed in 8.7b**: accepted on a bind and ignored, with a warning |
| F34 | low | Joining an existing `pid`, `ipc`, `uts` or `mount` namespace by its path is refused (only `network` can be joined); the suite's `linux_ns_path` fails on it | join them, safely (the joined namespace's owner, and what joining a `mount` namespace means for the root filesystem); Compose's `pid`/`ipc: service:…` need it | 10 |
| F35 | medium | `start` runs `poststart` hooks as soon as the init is let go, before the command's `execve` (the init, PID 1, forks it after): the spec says after the process is executed; the conformance suite's `poststart` sees the hook's write first | report the command's `execve` to `start` (a close-on-exec pipe: end of file once it has exec'd) and run the hooks after | **closed in 8.7b**: a started FIFO the init holds open (close-on-exec) until its command has exec'd; `start` waits for no writer left, up to its usual wait; only with `poststart` hooks |
| F36 | low | 8.7b's Murphy review: with mounts in a config's order, a user mount listed before `/proc`, `/sys` or `/dev` that a symlink in the image leads into that directory (`/s -> /sys`, a mount at `/s/x`) is made there and then hidden by the standard mount, its directory left in the image; rootless, the host `/sys` fallback then also tries to cover it (`mounts_below`), failing `create` or covering an unrelated directory of the container's own view. The config's own mounts and symlinks: nothing reaches outside | refuse a user mount whose resolved place is in a standard mount's directory before it's made (the kernel-interface check by place, not only by type) | 9 |
| F37 | low | Stage 8's whole-stage clean-code review (8.8a), left for the stage 9 rework: passed descriptors are taken and reserved three ways (the monitor's `keep_only`, the init's `Held`, the joiner's `reserve`); the init's inherited descriptors are listed twice (`plan.rs`, `init.rs`), so a new one could leak through the other list; "a console socket, no acknowledgement" is encoded two ways (`gate.is_none()`, `ExecSpawn.console`); the engine's attach keeps a `SIGWINCH` self-pipe of its own (`Winch`) beside the runtime's signals; small duplications (close-on-exec helpers, close-all-but sweeps, `/proc/self/fd/N/NAME` socket paths, three shapes of a terminal size); naming (`passed` in three senses, two `read_record`s, `NotAttachable`/`NotCaptured`) | one `passed::Held::take`, one `Setup::inherited()`, a `ChildTerminal::Console` variant, a runtime resize watch, the helpers shared; renames | 9 |
| F38 | low | H1 drift (8.8a review): `cp` is addressed by root and ID in the engine, but `logs`' and `attach`'s container-level logic is in the CLI (`LogsSource`, follow's end check, attach's end-race fallback), and `exec` is the runtime's alone: stage 11's API would have to copy it | `logs`/`attach` entry points by root and ID in the engine, or a recorded rule for what goes where | 11 |
| F39 | low | Stage 8's whole-stage Murphy review (8.8a), left for stage 9's state model: in the seconds after the init exits and before the monitor has cleaned up, `exec`, `cp` and `kill` say misleading things (the status is still `running`); the monitor gives up the lock after 15 s and then skips the exit record and the cgroup's removal (a CLI call stopped while holding it); `state.json`, `exit` and `killed` aren't versioned, and `exec.json`/`log.json` accept only the current version (an upgrade with containers running would need readers of the older one) | a `stopping` state the monitor sets; the liveness locks unified (stage 9's plan); versions on every record, and readers that keep the previous one | 9 |
| F40 | low | Stage 8's whole-stage review (8.8c): attach's frame decoder shifts its buffer once per frame, so a same-user client flooding small frames costs the monitor quadratic work; each `Resize` is applied, none coalesced (a SIGWINCH storm for the container) | decode from an offset, compacting once per read; keep a pass's last resize only | 9 |
| F41 | low | Stage 8's whole-stage review (8.8c): at the container's end the monitor waits for the lock (up to 15 s, twice) without serving attach clients or new hellos | take the lock without blocking, servicing the capture between tries | 9 |

**Stage 8 decisions (2026-10-09):**
- **D13:** stage 8 builds the pieces (terminals, a confined `exec`, a monitor that
  captures output and serves attach, `logs`/`attach`/`cp` by runtime ID); the
  user-facing commands by name come with stage 9's state model.
- **D14:** real-host CI: a rootless Ubuntu job with AppArmor's restriction on and
  the shipped profile, and a Fedora VM job (SELinux, firewalld).
- **D15:** logs in the CRI format, rotated by size and count, versioned in a
  `log.json` beside them (the log files stay pure CRI).
- **D16:** an isolated command never holds the user's terminal: where Shipyard's
  stdio is a terminal, it gets a pipe Shipyard relays (closes F17: a container
  can't change the terminal's settings). `--no-isolation` keeps the terminal.
- **D17:** an isolated command's stdin is `/dev/null` without `-i`, as Docker's.

**Stage 7.5 decisions (2026-10-09):**
- **D1:** no device program for a rootful run → drop `CAP_MKNOD` (in every set) and warn; `/dev` is unchanged (a `nodev` `/dev` with made nodes wouldn't open them: probe 10); refusing would break cgroup-v1 hosts.
- **D2:** a memfd that can't be exec'd → today's descriptor, with a warning.
- **D3:** host-`/` rootful runs warn; a required flag waits for a major version.
- **D4:** inherited metadata kept; chain-ID names only for extractions that inherited.
- **D5:** 128 layers per image.
- **D6:** fix the built-in profile's data (bundles mean exactly what they say).
- **D7:** anti-spoofing in a `bridge`-family table.
- **D8, D9:** warn, don't refuse.
- **D10:** a per-source conntrack limit becomes a stage 9 option.
- **D11:** F16 closed with a bound test.
- **D12:** F12 closed without deadlines: each wait is bounded by its other end (shipyard
  itself, end-of-file if it dies; each hook's OCI `timeout`, and a hook without one may
  run as long as it likes, as runc's), or behaves as runc's (`create` waits as long as a
  setup hung in the kernel, ended by `delete --force`). A deadline would end hooks the
  spec lets run. Each wait says so in a comment.

## Milestones

| Version | After stage | What works | Status |
|---|---|---|---|
| **v0.1** | 7 | `shipyard run --image alpine sh` rootless on Linux, on the caller's own terminal: pulled from Docker Hub, isolated, resource-limited, networked. Docker's spelling, `run -it alpine sh`, comes with a pseudo-terminal of the container's own (stage 8: `run -it --image alpine sh` works since step 8.2) and the image as the first argument (stage 9) | reached; release-ready after the readiness pass (doctor, the AppArmor profile, release files; checked by hand on Ubuntu 24.04) |
| **v0.2** | 9 | Containers run detached and survive the CLI exiting; logs, `exec`, restart policies | the pieces are built in stage 8 (by runtime ID: `shipyard-runtime create --log-dir`, `logs`, `attach`, `exec`, `cp`; checked by hand, rootless, on Ubuntu 24.04 and Fedora); the commands by name come with stage 9 |
| **v0.3** | 11 | Compose stacks and the API | |
| **v0.4** | 12 | Works on macOS | |
| **v1.0** | 14 | Desktop UI, conformance-tested, hardened, packaged | |

## Decisions

- **Daemonless** (Podman-style). There's no long-running daemon. Each container gets a
  tiny monitor process instead (built in stage 3 for `shipyard-runtime`), so an idle
  Shipyard costs nothing.
- **macOS-first, UNIX-only.**
  - The CLI, engine and UI are native on macOS and Linux.
  - Kernel isolation runs on Linux: natively there, and inside a Shipyard-managed VM on
    macOS.
- **The runtime is an OCI runtime.**
  - `shipyard-runtime` implements the OCI runtime spec (`config.json`;
    `create / start / kill / delete / state`).
  - The engine calls it like Podman calls runc. That gives us an official conformance
    suite and clean layering.
- **Syscalls via the conventional Rust route.** Use `std` where it covers the call, then
  `nix`, and raw `libc` only where neither does (stage 1, step 1).
  - `nix` over `rustix`: rustix offers no safe `fork`, `execve` or credential-changing
    API for this use, and nix covers the later stages' `clone`, `mount` and `setns`.
- **The container's PID 1 is Shipyard's own init** (stage 2). It re-execs the running
  binary to start clean, so every binary that runs containers calls
  `process::init_entry_point()` first in `main`. Its status pipe is never trusted beyond
  what its own exit code confirms.
- **After a raw `clone`, the child uses raw system calls** where glibc would keep
  process-wide state (the `setuid` family). They rely on the 64-bit-only build (stage 1).
- **Container mounts are recursively slave** of the host's (stage 2): nothing leaks out,
  and host unmounts reach in.
- **`serde` and `serde_json` for JSON** (stage 3): `config.json` is JSON, and `std` has
  none. The de-facto standard crates.
- **The init re-execs first** (stage 3): the cloned child re-executes the binary at once,
  while it still sees the host's files, and the clean image does all container setup
  (mounts, `pivot_root`, names) in ordinary Rust. A minimal root filesystem has no
  loader or libc for the host's binary.
- **Refuse what would confine less** (stage 3): a bundle field Shipyard can't apply yet,
  whose absence would leave the container less confined than asked, is refused, never
  silently ignored. Fields that don't confine are ignored with a warning.
- **Never reach a container's files, or its state, by path** where the path's meaning can
  change (stage 3): mount targets are resolved inside the root and mounted through
  descriptors, mount flags are only ever added (`mount_setattr`), and the runtime's state
  directory is written through a descriptor.
- **`shipyard-runtime` requires a `pid` namespace, or a cgroup** (stage 5): one of the two
  must find every process of a container.
- **Rootless is automatic** (stage 4): a non-root caller's isolated run, bundle or
  `shipyard-runtime` container gets a user namespace whose root is the caller: the full
  map through `newuidmap`/`newgidmap` and `/etc/subuid`, else the caller's own ID with a
  warning. Root keeps rootful containers, confined the same way.
- **seccomp is compiled by Shipyard itself, in Rust** (stage 4): profiles become classic
  BPF with no C library; Docker's profile is the default for `shipyard run`.
- **The init confines itself before its command goes on** (stage 4): only `CAP_KILL`,
  `no_new_privs` and the container's own seccomp filter, as Docker's init runs under it;
  every way a profile can break the init ends the container visibly.
- **Read the kernel's state; don't infer it** (stage 4): what can't be applied as asked
  (supplementary groups where `setgroups` is denied) is refused, never silently dropped.
- **Containers' cgroups are written directly** (stage 5): cgroupfs, v2 only, below
  `shipyard/` for root and inside systemd's delegated subtree for anyone else; a limit
  that can't be applied is refused, never dropped (except Docker's implicit swap
  default, skipped with a warning where swap isn't accounted).
- **A cgroup, like a PID, is pinned before it's trusted** (stage 5): a lock for a live
  run, the recorded inode for a container, pidfds for its processes.
- **Device control is Shipyard's own eBPF** (stage 5): runc's default list, compiled by
  Shipyard; real root fails closed where it has a cgroup, rootless goes without where
  the kernel refuses.
- **Netlink is Shipyard's own** (stage 7): rtnetlink, nf_tables and conntrack messages are
  built and read by its own bounded, fuzzed code; no `ip` or `nft` binary and no netlink
  crate, as with the seccomp and eBPF compilers.
- **Networking follows Docker's defaults** (stage 7): root's runs join the `bridge` network
  (NAT, no names), rootless runs get pasta (one per container; given the container's
  network namespace as a file since 8.1c, not attached by PID), and
  rootless without pasta is refused with a hint, never quietly given the host's network.
  OCI bundles keep the spec's meaning.
- **The firewall is one table of Shipyard's own** (stage 7): `inet shipyard`, with static
  rules and per-network and per-container sets and maps. Other firewalls are detected
  and warned about with the exact fix, never changed (stage 8.6: firewalld's trusted
  zone, Docker's `DOCKER-USER`; a fix applied isn't warned about again).
- **Turning a host into a router is undone for everyone else** (stage 7): when Shipyard
  turns forwarding on, it routes only containers' traffic, keeps the host learning its
  IPv6 routes (`accept_ra=2`), and never lets a container's router advertisements in.
- **One DNS server per network, rootful, not a daemon** (stage 7): started by the first
  run that needs it, outside the run's supervisor, as `nobody`; it exits when idle.
- **A container never gets the user's terminal** (stage 8, D16, D17): pipes relayed by
  Shipyard, or with `-t` a pseudo-terminal made inside the container whose master comes
  back over a socket; stdin is `/dev/null` without `-i`.
- **One monitor per container, the same binary** (stage 8, D13, D15): it holds the
  container's output, writes CRI logs rotated by size and count, and serves attach on a
  UNIX socket; no daemon.
- **What runs inside a container on its behalf is a short-lived process of Shipyard's
  own** (stage 8): `exec`'s joiner and `cp`'s helper, started from a sealed copy of the
  binary, non-dumpable before they join, holding nothing of the host's.
- **The OCI conformance suite gates the runtime** (stage 8, H5): every test passes, or
  fails for a stated reason matched in its output (`scripts/conformance/expected.txt`).
- **No image building before stage 11.** It's out of scope until then; see
  [FEATURES.md](FEATURES.md#out-of-scope-on-purpose).

## Open questions

Decide these when we reach them:

- Whether to ship a Docker-compatible API subset (stage 11).
- UI framework: Tauri or alternatives (stage 13).

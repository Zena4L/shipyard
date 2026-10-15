# Briefs: the five-reviewer readiness review

Launch all five in one message, each a fresh background agent. Every brief starts with
the common header; then its area. Ask each for a score out of 10 with a one-paragraph
justification, and a report under 1,200 words.

## Common header (paste into each)

> You are doing a fresh, skeptical production-readiness review of [AREA] in Shipyard
> (/Users/clementbogyah/Desktop/shipyard), a daemonless container engine in Rust, at
> [version / milestone]. Read `AGENTS.md` first. The project claims [what the milestone
> claims] is production ready.
>
> **Rules:** read-only. Don't edit, create, stage or commit any file. Don't run cargo
> builds or tests (another build is using the target dir), `docker` or `limactl`. Use
> reading, grep and git only.
>
> Review the uncommitted or unreleased changes first (`git diff [BASE] -- [PATHS]`, plus
> untracked files), every hunk, then the area broadly. Verify each issue by reading the
> real code paths before reporting it; no style nits. Report: severity
> (critical/high/medium/low), file:line, what's wrong, a concrete failure scenario, your
> confidence. Note briefly what's notably well done. End with your score out of 10 for
> this area's production readiness, and why.

## 1. Runtime (`crates/shipyard-runtime/`)

Post-fork code (async-signal-safety, allocation, locks); every `unsafe` block and its
SAFETY claim; namespaces and mounts (`pivot_root`, propagation, `/proc`/`/sys` masking,
symlink races on mount targets); seccomp, capabilities, `no_new_privs`, user namespaces
and ID maps; cgroup v2 (delegation, leaf rules, removal); the monitor and its control
protocol; `SCM_RIGHTS` (leaks, `MSG_CTRUNC`, close-on-exec); signals; PID reuse; log
growth; terminals.

## 2. Engine (`crates/shipyard-engine/`)

Correctness under concurrency and crashes: records (atomic writes, fsync of file and
directory), the lock order (deadlocks, inversions, locks held across waits, TOCTOU between
read and act, two CLIs racing); each SIGKILL point of `run -d`, `rm`, `restart`, `network
connect` (leaked cgroups, netns, leases, ports, mounts, half-written records); restart
policy and backoff; recovery and the host-stopping marker; health checks; the events
journal (rotation, concurrent appenders, torn lines); name vs ID-prefix resolution;
volumes in use; on-disk input treated as possibly corrupt; error-kind mapping.

## 3. Network, image, storage

Netlink building and parsing (bounds, alignment, attribute lengths, acks, sequence
numbers, partial reads); firewall rules (isolation between networks, published ports,
`--isolate-host`, conntrack limits, IPv6); lease allocation races; pasta launch and death;
the DNS server (untrusted packets, amplification, TCP, resource limits); the registry
client (TLS, tokens, redirects, digest of every blob and manifest, size and time limits);
the content-addressed store (concurrent pulls, partial downloads, GC); layer unpacking
(traversal, symlink and hard-link escapes, whiteouts, devices, setuid, xattrs, bombs);
named volumes.

## 4. CLI, tests, release

The CLI: error handling, exit codes, broken pipes, escape injection from
container-controlled strings, confirmation for destructive commands, Docker-compatibility
surprises. Tests: vacuous or flaky ones (sleeps, timing, ports), untested milestone
behaviour, how many are root-only and whether CI runs them. CI and release: workflows,
`deny.toml`, artifacts, static builds, install instructions, LICENSE, SECURITY, CHANGELOG
against the code, `contrib/`. Docs: check 10+ concrete claims (flags, exit codes,
behaviours) against the code; are the known limitations told honestly? Is this
**releasable** (tags, binaries, CI green), not only good code?

## 5. Security (adversarial)

Trace real code paths for: a malicious container (root inside) escaping or affecting the
host or other containers (`/proc`, mounts, devices, capabilities, seccomp gaps, the
monitor's socket, `exec`/`cp` helpers and symlinks, health checks, attach and log streams,
DNS, signals, filling storage, fork bombs and memory without default limits); a malicious
image (layers, config, HEALTHCHECK, SHELL, user, labels); an unprivileged local user
against a root-run Shipyard (state-dir permissions, sockets, records, locks, markers,
cgroups, `/tmp`, environment variables kept by `sudo`, the systemd units); rootless
exceeding its ID range or delegated cgroup; terminal-escape injection into the operator's
terminal. Mark anything unverified as such; name the defences you confirmed.

# Stage 15 notes: hardening and v1.0

The engineering record of stage 15: what was built, why it's built that way, what the
kernel and the release tools were checked to do, what can go wrong and how that's
handled, and what was left for after v1.0. For how to *use* it, see the docs site
(<https://zena4l.github.io/shipyard/>), and in it [Containers](../user/containers.md)
(`--pid`, `exec`'s set-up), [The Docker API](../user/api.md) (1.44),
[Shipyard on a Mac](../user/macos.md) (Docker's socket in a container, the guest's
firewall) and [The threat model](../user/threat-model.md). How a release is made, checked
and rebuilt is in [`DEVELOPMENT.md`](../DEVELOPMENT.md)'s Releases.

## Outcome

Stage 15 asked for v1.0 as D30 defines it ([ROADMAP](../ROADMAP.md),
[FEATURES §16](../FEATURES.md#16-production-quality--ongoing-from-stage-1-finalized-in-stage-15)):
green CI, OCI conformance as expected, fuzzing and mutation checks, signed reproducible
releases, packages, a docs site, and every open finding fixed or explicitly accepted. Its
outline was decided on 2026-10-29 (D15-1 to D15-4, D15-5 the next day) and built in steps,
each merged as a pull request (#25 to #50):

- **15.0** the repository public, CI on every pull request, its first failures fixed;
- **15.1** the triage of all 82 open findings (D15-3);
- **15.2** the fixes (a: security and integrity; b: `--pid container:` and `host`, F134;
  c1: Ubuntu 26.04; c2: joiners told, DNS and lease bounds; d: build and load; e1/e2:
  volumes, the API service, a pull's bytes; f1/f2a/f2b: the Mac host, the guest's
  firewall, Docker's socket in a container; g1/g2/g3: API 1.44, a pull's silences, the
  guest's VM tests run at last);
- **15.3** the tests (a: the Fedora job, Rosetta's glibc; b: F193; c: the 15.2 reviews'
  leftovers);
- **15.4** the docs site (D15-4);
- **15.5** releases (a: reproducible, attested builds; b: `.deb`, `.rpm`, Homebrew);
- **15.6** the close (a: v1.0's gaps; b1–b4: the release reviews' fixes; c: these notes,
  fuzzing, the full verification, the milestone; d: v1.0.0-rc.1, then v1.0.0).

From the hardening track the stage took on H2 (`created.json` v7, `joined` v1,
`recovered-boot.json` v1, the ruleset 10 then 11, the forwarded protocol 7, each written
at its version from the first write), H3 (a threat model in every plan; a Murphy review of
15.6b2's plan before it was built), H5 (Docker's CLI 29 and the devcontainer CLI driven
against the API) and H6 (`VmError::Newer`, `ClientError::nothing_serves`).

It closed F123, F126, F130, F134, F139, F162, F171, F174, F175, F182, F192, F194, F195,
F196, F198, F199, F200, F201, F202, F204, F205, F206, F207, F208, F209 and F211 outright;
fixed parts of F31, F111, F136, F150, F164, F165, F166, F170, F178, F180, F183, F185,
F186, F187 and F193, the rest of each accepted with its reason; and accepted F203, F210
and F212–F221. Every row that was homed in stage 15 is closed or in the ROADMAP's
"Accepted at v1.0" table; four rows are after v1.0 (below). A user can now:

- drive Shipyard with Docker's CLI 29 (Ubuntu 26.04's own `docker-cli` 29.1) with no
  `DOCKER_API_VERSION`: the API is 1.44, `docker run --network a --network b` included;
- `shipyard run --pid container:db …` (and compose's `pid: service:`), or as root
  `--pid host`, to see and signal another container's processes;
- on a Mac, bind `/var/run/docker.sock` into a container and get the VM's Docker API, so
  Testcontainers' Ryuk works without `TESTCONTAINERS_RYUK_DISABLED=true`;
- watch `docker pull` through the API draw each layer's bytes, its unpacking and its
  waits, as Docker's daemon says them;
- install from a release (once v1.0 is published): static musl tarballs for x86-64 and
  arm64, `.deb` and `.rpm` packages, a Homebrew formula for Apple Silicon, the Mac's
  `shipyard` with its guest beside it, and `Shipyard.app`; check any file with `gh
  attestation verify`, and rebuild the Linux binaries and the guest to the same bytes;
- read the licences of everything built in (`THIRD-PARTY-LICENSES.html`, the app's own
  page) and fetch the guest's GPL sources (`shipyard-VERSION-guest-sources.tar`);
- read the docs as a site, searchable, its links checked on every pull request, and a
  written threat model;
- contribute: `CONTRIBUTING.md`, a code of conduct, issue forms, a PR template, and CI on
  every pull request.

| Requirement | Where | Tested by |
|---|---|---|
| Platforms: Linux x86-64 and arm64, macOS on Apple Silicon (D30) | `release.yml`: musl builds on both Linux architectures, the Mac on `macos-15` | the release's dry runs (45157813900, 45244265886, 45438587986, 45485076316): every job green |
| `alias docker=shipyard`; Docker's own CLI against the API, at 1.44 (F207) | `shipyard-api`'s `routes.rs` (`NEWEST` held to the engine's `API_VERSION`), `create/` (`EndpointsConfig`), `images.rs` (`VirtualSize` by version) | `docker_s_cli_29_is_served_at_1_44` (Docker's CLI 29.4.2 in the test image), `the_versions_agree`, `several_networks_are_joined_at_create`, `virtual_size_is_kept_for_older_clients_alone`, `info_names_its_runtime_as_1_44_does` |
| Testcontainers and devcontainers against the API | `crates/shipyard-acceptance` (`tests/common`, `tests/devcontainers.rs`) | `a_devcontainer_comes_up_against_shipyard` (devcontainer CLI 0.90, root and rootless; required by `test-linux.sh`), the Testcontainers tests (stage 11) |
| Docker's socket in a container on a Mac (F171, D30's Ryuk) | the guest's PID 1 binds it at boot, hands it at `forward::DOCKER_FD`; the agent starts the service on it; `guest/shares.rs` maps the bind | VM `docker_s_socket_is_given_to_a_container` (`/_ping`, again after the idle end, `--user 1000` refused); `docker_s_own_socket_is_given_the_guest_s`, `docker_s_socket_is_left_to_the_vm` |
| x86 images through Rosetta, glibc's too | stage 13's Rosetta | VM `a_glibc_amd64_image_runs_through_rosetta` (Debian: its loader, `apt-get`, `clone3`'s threads, NSS, DNS), required by `verify.sh --vm` where Rosetta is installed |
| Joining another's PID namespace, safely (F134) | runtime `process/joins.rs` (`tracer_in`), `process/init.rs` (`join_then_clone`); engine `created.json` v7, `pid-join.lock` | `a_container_joins_another_s_pid_namespace` (runtime and CLI), `a_pid_join_waits_for_another_s_set_up`, `a_pid_joined_containers_cgroup_ends_with_its_monitor`, `another_s_pid_namespace_needs_its_user_namespace`, `a_pid_namespace_is_v7` |
| `exec`'s command confined before anything can see it (15.6b2) | runtime `process/exec.rs`, `child.rs`, `spawn.rs` (`own_child`), `security/confinement.rs` (`joiner_lowering`) | `exec_s_command_is_born_with_only_its_own_powers` (root), `exec_as_root_without_capabilities_starts_in_its_directory`, `exec_s_joiner_keeps_only_what_the_command_needs`, `only_our_own_child_is_taken_for_exec_s_command` |
| Green CI (D15-1) | `.github/workflows/ci.yml` (macOS, rootless Ubuntu 24.04 and 26.04 gating, conformance, docs; Ubuntu's root pass on `main`), `quality.yml` (fuzzing, mutants, Fedora nightly: D15-5) | the workflows' runs on each PR from #25; `scripts/ci/pasta-control.sh`, `apparmor-control.sh` |
| OCI conformance as expected | `scripts/conformance/expected.txt` | x86-64: 28 pass, 14 fail each by design or the suite's bug, 16 skip (cgroup v1); arm64: `process_rlimits` a skip (FEATURES §16) |
| Fuzzing and mutation checks | `fuzz/` (19 targets, none new in stage 15); `mutation-runner.py` per step | the close's fuzz run (below); 124 of 126 hand-picked mutants caught, the 2 others equivalent |
| Signed reproducible releases (F186) | `scripts/release/` (`build.env` pinned by digest, compiler `1.98.1`, `compare.sh`), `release.yml` (`reproduce`, `compare`, `attest`) | dry runs: Linux and the guest the same bytes on a second runner; the arm64 tarball rebuilt on a Mac identical to CI's (`6e22662f…`); attestations refused against another tag (control) |
| A release's inputs kept (F211) | `snapshot-apt.sh` (`DEBIAN_SNAPSHOT`), the guest's kept `.apk` set (`SHIPYARD_GUEST_APKS`) | the guest built twice fetched and once from its kept set, all identical (run 45438587986); a set missing `zstd-libs`, an empty set, a lock rewrite from a set each refused |
| Licence notices and the guest's sources | `notices.sh` (cargo-about 0.9.3, `about/`), `guest-sources.sh` (`abuild fetch verify`) | every crate named, an unaccepted licence fails (control); 23 origins verified (266 MB), a changed byte refused (control); run 45485076316 |
| Packages: `.deb`, `.rpm`, Homebrew (F178's install place) | `packages.sh`, `package/`, `homebrew/`, `homebrew.yml`; `shipyard-vm`'s `paths.rs` (`guest_beside`) | `test-packages.sh` in Ubuntu 24.04 and Fedora 42, both architectures (install, upgrade, remove); a local tap: `brew install`, `brew test`, `brew audit --strict`; `a_guest_beside_is_the_user_s_or_root_s` |
| A docs site (D15-4) | `docs/site/book.toml`, `docs/user/SUMMARY.md`, `scripts/docs-site.sh`, `pages.yml` | `docs-site.sh` in `verify.sh --mac` and CI (1,988 links and anchors at 15.4); controls: a page left out, a broken anchor, a raw `<NAME>`, a link leaving the site |
| A threat model (FEATURES §16's security process) | `docs/user/threat-model.md` | each claim checked against the code (15.6a's review corrected four) |
| Open source: licence, CONTRIBUTING, code of conduct, templates | `LICENSE` (Apache-2.0 alone), `NOTICE`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `.github/ISSUE_TEMPLATE/`, `pull_request_template.md` | `cargo deny check licenses` in both workspaces |
| Performance targets measured, not blocking (D30) | `scripts/bench/report.sh` | 15.6a's run (below); the milestone hosts' at the close |
| Every open finding fixed or accepted (D15-3) | ROADMAP's open findings and "Accepted at v1.0" | each fix's test, listed in the failure-mode register |

**The performance report** (15.6a, a release build in OrbStack's VM, arm64):

| Target | Measured | |
|---|---|---|
| A start under 100 ms | `run --rm --network none`: 122 ms root, 168 ms rootless; on the default network 158 / 177 ms; `start` alone 30 / 67 ms | over the target, reported |
| No idle process but the monitors | none of Shipyard's with nothing running; beside a container, its monitor alone (and pasta's launcher, rootless on a network) | met |
| A monitor under 1 MiB | about 520 KiB of its own (its RSS is mostly the binary's text) | met |

**Moved out:**

| Item | Now in | Why |
|---|---|---|
| Notarization (FEATURES §16's "notarized macOS app") | after v1.0 (D15-2) | no Apple Developer ID; `scripts/macos/notarize.sh` stays for then |
| F125 (`update --pids-limit -1`), F156 (`docker build`/`buildx` through the API), F157 (dangling images), F159 (build's other inputs) | after v1.0 | Docker features missing, each refused cleanly and said |
| The Mac's binary and app reproduced in CI (F219) | accepted | Apple's linker and `codesign` aren't promised deterministic; a clean rebuild on a Mac was identical |
| Each container's rx/tx rate on the map; a Linux installer for the app | not built (from stage 14's moved-out table) | no row holds them yet: see Carried forward |

## Architecture

### Where checks run

```
pull request ── ci.yml: macOS (fmt, clippy, tests, deny, signing, desktop)
                         rootless Ubuntu 24.04 and 26.04 (AppArmor on, pasta control)
                         conformance, docs (docs-site.sh), root: macOS only
push to main ── ci.yml: the same, plus Ubuntu's root pass (90 min, --no-fail-fast)
nightly      ── quality.yml: fuzzing, mutants, Fedora 44 in a nested VM (SELinux)
v* tag       ── release.yml (below); a run by hand is a dry run
local        ── verify.sh: macOS, both Linux passes, lints, the Mac's VM (the record)
```

CI has no VM, no Mac guest and no real distribution beside Fedora: local verification
stays the check of record (AGENTS.md §7). The Docker Hub images CI's build tests need come
from Google's mirror (`SHIPYARD_TEST_MIRROR=mirror.gcr.io`), each pulled once a run.

### The release

```
check (tag = workspace version, CHANGELOG dated, tag on main)
├─ notices, notices-app (cargo-about over the lockfiles; the page's npm from sourcemaps)
├─ linux x86-64, arm64 ─ reproduce (second runners) ─ compare (byte for byte)
│  └─ packages (.deb, .rpm by nfpm) ─ test-packages (Ubuntu 24.04, Fedora 42)
├─ guest: twice fetched, once from its kept .apk set, compared; its SOURCES; its sources
├─ macos: shipyard (bin/ + guest/), Shipyard.app, signed ad hoc (D15-2)
├─ sbom (CycloneDX)
└─ attest (SHA256SUMS; each file attested and verified to workflow, ref, commit)
   └─ release: a draft (rc tags as prereleases); Linux alone if the Mac's part failed
homebrew.yml (on publish): the Mac tarball's attestation checked, the formula pushed
```

Every input is pinned: the build image by digest, the compiler to its patch, `musl-tools`
from snapshot.debian.org at `DEBIAN_SNAPSHOT`, the guest's packages in `alpine.lock` and
kept with the release. A later rebuild needs nothing the mirrors may drop.

### A joined PID namespace (15.2b, 15.3c)

```
__shipyard-monitor
└─ a sealed, non-dumpable copy of shipyard: enters the joined namespaces, clones
   └─ init (not PID 1: a subreaper) ── re-execs the clean image inside the joined user
      │                                namespace, before its pivot (its memory theirs)
      └─ the command
```

The init ends its processes with it, and when its monitor dies (a catchable parent-death
signal; shipyard's end told by its status pipe losing its reader), then its cgroup whole
by the `cgroup.kill` it opened before its pivot (F204). A namespace where any process may
hold `CAP_SYS_PTRACE` is refused, every process there looked at; a chained join is
refused by `NSpid`; two joins' set-ups never overlap (`pid-join.lock`, lock order 2a).

### `exec`'s joiner (15.6b2)

The joiner sits in the host's PID namespace, where no process of the container's sees it.
Before it clones the command it moves itself into the container's cgroup, takes a session
keyring of its own, joins every namespace (mount last; no host path resolved after),
enters the working directory, closes every descriptor of the host's but the command's,
and lowers its capabilities to the command's own sets plus `SETUID`, `SETGID` and
`SETPCAP` (and `SYS_ADMIN` only with `no-new-privileges=false`). Shipyard takes the PID it
reports only if it's its own child (`waitid` with `WNOWAIT`).

### Docker's socket in the Mac's guest (15.2f2b)

```
guest PID 1 ── binds /run/shipyard-docker/docker.sock at boot, keeps it for the VM's life
└─ agent (fd 4: forward::DOCKER_FD) ── a connection waits? ── sh -c 'export LISTEN_PID…;
                                                               exec shipyard system service'
   (another process serving it, found by the socket's inode, is waited for)
container: -v /var/run/docker.sock ── mapped to that socket before the share check
```

### Files and wires

| | Version | Step |
|---|---|---|
| `created.json` (`pid_from`, `pid_host`) | v7, written only when set | 15.2b |
| `joined` (the run a joiner joined), in its log directory | v1 | 15.2c2 |
| `recovered-boot.json` (the boot whose pass has run) | v1 | 15.2a |
| The bridge ruleset (`same_uplink`); the guest's (nothing new from outside) | 10; 11 | 15.2c2; 15.2f2a |
| The forwarded protocol (a forwarded port's welcome carries its session) | 7 | 15.2f2a |
| `/shipyard/containers/{id}/isolation` (`joined`) | v1, a key added | 15.3c |
| The API | 1.24–1.44 | 15.2g1 |

## Decisions and why

- **D15-1 the repository public early, for CI** (2026-10-29, supersedes D31): GitHub's
  standard runners are free for public repositories. The history was scanned first; the
  maintainer's checklist was removed from it (the history rewritten). Nothing released
  until 15.6d: D21 is about users, not visibility.
- **D15-2 no Apple Developer ID:** the Mac's files ship signed ad hoc (or with the local
  identity), not notarized; the docs say how to allow them. Its cost: Gatekeeper's prompt.
- **D15-3 findings triaged before any fixing** (approved 2026-10-31): 32 to fix, 44
  accepted with reasons, 4 later, F95 already fixed (its rest F201). An accepted row is a
  statement: no hostile container, image, registry or local user gets a wrong result, a
  leak or a hang they couldn't already cause.
- **D15-4 mdBook on GitHub Pages,** `docs/user/` as it is (asked: from `main` now,
  mdBook's own theme); links leaving the site refused, as they 404 published.
- **D15-5 Fedora nightly, not on PRs:** a nested VM takes one to two and a half hours.
- **Apache-2.0 alone** (15.0), was MIT or Apache-2.0.
- **15.2b, asked:** two parts (runtime, surface); `container:` and `host` (D15.2b-2); a
  target that may hold `CAP_SYS_PTRACE` refused (D15.2b-10); the init re-execs inside the
  joined namespaces (D15.2b-12), replacing a user-switch design that couldn't serve
  rootless pasta.
- **15.2c1 (D15.2c-3):** the rootless job on 24.04 and 26.04, the new one
  `continue-on-error` until its first green run, then gating (both gate now).
- **15.2d, asked:** a rootless build without a delegated cgroup warns, and still builds
  (a refusal broke `RUN`-less builds that worked).
- **15.2e1, asked:** the API service started under the user's systemd manager
  (`systemd-run --user`), the caller's child where that can't be trusted.
- **15.2f, asked:** F180(f) moved to f2 with a real hang-up by token, not a longer bound;
  F171 by socket activation in the guest (the socket never changes, so a container holding
  it keeps reaching the API).
- **15.2g1, asked:** serve 1.44, honoured fully (not 1.52, the newest).
- **15.2g2, asked:** F210 accepted rather than refusing read-only binds over covered
  mounts before Linux 5.12 (it turned away `-v /:/host:ro` and guarded nothing).
- **15.3c, asked:** F205's rootless `--oom-score-adj` with `--pid container:` refused at
  the create; PID joins serialised; F203 accepted.
- **15.5, asked:** split in a (builds) and b (packages); GitHub's artifact attestations
  as the signature; nfpm; the boot units installed and never enabled (D20); the tap
  updated by a workflow on publish, its repository made at v1.0.
- **15.6, asked:** four steps; the runtime review's high fixed in a step of its own.
- **15.6b2, asked:** confine `exec`'s command before the clone, rather than refuse such an
  exec or accept the window (runc's exec has the same window).
- **15.6b3, asked:** keep the release's inputs ourselves (a snapshot for `musl-tools`, the
  guest's `.apk` set as an asset); release Linux without the Mac's part when that fails;
  notices at release time, the app included; every guest package's source published.
- **Mine, 15.6b1:** rootful binds stay without `nosuid`/`nodev`, as Docker's; said in the
  threat model. The packages keep loading the AppArmor profile, said plainly.
- **15.6b4, asked:** CI through Google's mirror of Docker Hub; Ubuntu's root tests on
  `main` and by hand only, local `verify.sh`'s root pass the check before a PR.

## Kernel checks

- `setns` into a PID namespace changes only the caller's children, never the caller: the
  joiner clones the init there. `NSpid` in `/proc/PID/status` gives a
  process's depth, by which a chained join is refused (15.2b).
- A non-dumpable process's `/proc` belongs to the root of the user namespace its memory
  was made in (`mm->user_ns`, set at `exec`), and a change of effective user clears the
  parent-death signal (`commit_creds`): found in 15.2b1, the reason for D15.2b-12.
- `CAP_SYS_PTRACE` in a process's user namespace overrides non-dumpable: 15.6b2's window,
  rootful only (rootless, the command's memory is the host's user namespace's).
- `cgroup.kill` ends a cgroup whole, Linux 5.14+ (F204); `mount_setattr` with
  `AT_RECURSIVE` makes a bind read-only all the way down, Linux 5.12+ (F210).
- `waitid(…, WNOWAIT)` sees a child without reaping it, where `/proc` under `hidepid`
  hides a non-dumpable one from a rootless caller (15.6b2).
- Ubuntu 26.04 (kernel 7.0, AppArmor's restriction on): pasta's own profile takes signals
  only from `unconfined`, and the exec into it clears its parent-death signal; a launcher
  that `changeprofile`s to `unconfined` can stop it (15.2c1, in a Lima VM: 7,080 leftover
  pastas before, none after, 0 denials).
- `/proc/uptime` doesn't move with an NTP step or a Mac's sleep (15.2d's builder wait).
- Linux's libc reads a directory removed under `getdents64` as empty; macOS says `ENOENT`
  (15.2e1's concurrent removal restarts).
- A TCP socket's receive window is set by `SO_RCVBUF` before `connect`; set after, the
  advertised window holds the whole reply (15.2c2's DNS test, AGENTS.md §10).
- The guest's `linux-virt` was re-pinned four times as Alpine dropped the one before:
  to 6.18.60, 6.18.62, 6.18.64 and 6.18.65 (now kept with the release: F211).

## macOS and Linux differences

| | macOS | Linux |
|---|---|---|
| `--pid host` | root's in the VM | root only, said at the create |
| Docker's socket in a container | the VM's API (root's containers only), by socket activation in the guest | the user's own socket, bound as any file |
| The firewall on the outside link | the guest takes nothing new on its own address but DHCP, neighbour discovery and replies (ruleset 11) | the host's own: `same_uplink` exempts bridges alone (15.6b1) |
| A release's binary | ad hoc signed, not reproduced in CI (F219) | static musl, reproduced on two runners, `.deb`/`.rpm` |
| Install | Homebrew's keg or the tarball: `bin/shipyard` with `guest/` beside it | packages in `/usr/bin`, units off, the AppArmor profile placed where AppArmor 4 reads it |
| A removed directory while walked | `ENOENT`: the walk restarts | read as empty |
| The API service, rootless | — (the VM's root) | `systemd-run --user` under a manager, else the caller's child |

## Failure-mode register

| Case | Handling | Test |
|---|---|---|
| As root, a credentials config swapped for a link to another's file, or its directory after the check | the directory opened once without following, its owner checked on that descriptor, read and written through it (F162) | `root_refuses_a_user_s_linked_config_directory`, `a_config_directory_swapped_after_its_check_is_never_written_through` |
| A pause interrupted between asking the freeze and its taking hold | read as paused; Ctrl-C held off until the freeze holds or is undone (F126) | `a_freeze_asked_is_a_pause_reached_or_not` |
| A volume's lock file removed while another holder opens it | re-checked once held, taken again on a fresh open (F150) | `a_removed_lock_file_is_no_longer_the_one_at_its_path`, `a_hold_on_a_removed_lock_file_holds_the_new_one` |
| A container the boot will restart, its networks removed first | kept until the boot's pass has run (`recovered-boot.json`) (F130) | `what_the_boot_will_start_keeps_its_networks_until_its_pass` |
| An API stop whose client leaves | stops waiting, also for the operation lock within half a second; the mark set only under the lock or with the monitor asked (F123, F202) | `a_stop_whose_client_goes_lets_the_service_end`, `a_lock_wait_ends_when_its_caller_goes`, `a_stop_given_up_unasked_leaves_no_mark` |
| A stop of a container waiting to restart, while another command holds it | the restart cancelled before the lock is waited for (15.6b1) | `a_stop_cancels_a_waiting_restart_before_the_lock` |
| A PID namespace where a process may hold `CAP_SYS_PTRACE` | refused, at the runtime (every process looked at) and the engine (target and its joiners) | `a_container_joins_another_s_pid_namespace` |
| Two PID joins' set-ups at once (a debugger's beside another's) | `pid-join.lock` from the checks to the go (F205) | `a_pid_join_waits_for_another_s_set_up` |
| A joined init's monitor killed; a fork bomb in an `exec` | the init ends its processes, then `cgroup.kill` (F204) | `a_pid_joined_containers_cgroup_ends_with_its_monitor` (root and rootless) |
| `run --pid host` with a command that can't run | the clean image says the failure itself (127), where nobody read the pipe (15.2g3) | `a_container_joins_another_s_pid_namespace` (root) |
| A rootful container with `SYS_PTRACE` tracing an `exec`'s command during its set-up | the command is born with only its own powers, in every namespace, with none of the host's descriptors | `exec_s_command_is_born_with_only_its_own_powers` |
| A forged record naming another process as the exec's command | taken only if it's shipyard's own child | `only_our_own_child_is_taken_for_exec_s_command` |
| A container that joined another's network or IPC namespace, the other restarted | the joiner told: `ps`, `inspect --shipyard`'s `joins_lost`, a `network_lost`/`ipc_lost` event (F136a) | `a_joiner_is_told_the_container_it_joined_started_again`, `a_lost_join_is_said_in_the_status` |
| Ubuntu 26.04's pasta profile refusing Shipyard's signals | the launcher leaves for `unconfined`; the sweep asks (SIGTERM) before it kills | `a_leftover_is_asked_to_stop_before_it_is_killed`, `only_shipyards_own_profile_is_left_for_pasta`; `pasta-control.sh` in CI |
| An API attach waiting for a start, taking its passing states for an end (F200) | only a new, real end ends it | `only_a_new_real_end_gives_an_attach_up` |
| A DNS-over-TCP reader taking a byte at a time | the reply written whole within 5 s (F111) | `a_slow_reader_doesnt_keep_its_connection` |
| The host's uplink a bridge (`br0`), the routing record unreadable | closed, and what's bridged on it left alone; only bridges exempt (F111, 15.6b1) | `the_uplink_is_closed_even_when_it_s_a_bridge`, `what_s_bridged_on_an_uplink_is_left_alone`, `only_a_closed_bridge_keeps_its_own_traffic` |
| An archive listing one image twice | loaded once by both names; every blob read before any move (F165) | `an_image_listed_twice_loads_once_by_both_names`, `a_manifest_two_indexes_share_loads_as_two_images` |
| BuildKit's readiness across an NTP step or a sleep | waited for by `/proc/uptime` (F166) | `buildkit_s_readiness_is_waited_for_by_the_clock` |
| One huge volume first in the desktop's list | each volume an equal share of what's left of 3 s (F194) | `each_volume_gets_a_share_of_the_time` |
| A huge volume's delete past the client's bound; two removals of one tree | answered once trashed, deleted after; a stop lets that go, the next start sweeps (F195, F208) | `a_deleted_volume_s_files_go_after_the_answer`, `two_removals_of_one_tree_at_once_both_succeed`, `a_stop_lets_background_work_go`, `a_service_sweeps_the_volume_trash_at_its_start` |
| The API's backlog full | an answer, never "nothing serving" (F196) | `a_full_backlog_answers_without_waiting`, `only_no_listener_is_nothing_serving` |
| A pull's client that stops reading | the byte lines offered once and dropped; the layer's own lines wait (F192) | `a_pull_s_byte_lines_never_wait_for_a_slow_client` |
| A pull silent while unpacking, waiting out `Retry-After` or the store's lock | each said, as Docker says them (F209) | `each_extracted_layer_is_told`, `a_layer_s_wait_before_a_retry_is_said`, `a_wait_for_the_store_is_said` |
| `launchctl print` failing for another reason than absence (over SSH: no `gui/UID`) | 112 and 113 are absent; unsure, launchd is kicked, a refused kick falls back (F182) | `launchctl_s_exits_are_these`, `an_unsure_launchd_with_the_agent_installed_is_kicked` |
| A Mac client of a forwarded port gone (the guest sees a half-close alone) | the Mac hangs the guest's relay up by its session token after an unclean end (F180(f)) | `a_port_s_relay_is_hung_up_after_an_unclean_end_by_its_token`; VM `a_gone_client_of_a_streaming_port_is_hung_up` (51 ms) |
| Another VM on the Mac's network reaching the guest | nothing new taken on its own address (F175); the firewall retried until made (15.6b1) | VM `the_vms_own_address_answers_nothing_new`; `a_link_outside_opens_nothing_new_on_the_host` |
| A restarted agent starting a second service on Docker's socket | waits while another process serves it, found by the socket's inode | VM `docker_s_socket_is_given_to_a_container` |
| A test that changed `/var/tmp`'s mode on the host (F199) | only what's below the temporary directory touched; the runner checks `TMPDIR` after each executable | `a_context_leaves_the_temporary_directory_alone` |
| A Ctrl-C landing between a shell's traps and its `read` (F193) | Enter typed after it | `interrupt_the_read` (five tests); 0 failures in 3,264 runs beside full passes |
| A proxy variable holding a password, systemd before 250 | never in argv: the service started as the caller's child | `an_older_systemd_gets_values_but_no_password`, `systemd_s_version_is_read` |
| A `brew upgrade` removing the keg launchd's agent named | a guest found beside the binary is never written down | `a_guest_beside_is_the_user_s_or_root_s` |
| A `.deb`'s unit directories made 0644 | the test fails on any package directory not `drwxr-xr-x` | `test-packages.sh` |
| Alpine withdrawing the pinned kernel (2026-11-24) | the guest built from the release's kept `.apk` set, offline, each signature checked, the result against `alpine.installed` | the third build in `release.yml`'s guest job, compared |
| A release run cancelled, or re-run | nothing published on a cancel (`!cancelled()`); no second draft | dry run 45436995206 (Linux attested without the Mac's part) |
| A guest built with its downloads inside the checkout | outside it: the manifest never `-dirty` (every Mac warned of another build) | dry run 45157813900 |
| Docker Hub's 429 in CI's build tests | each image pulled once a run from Google's mirror, loaded into every store | the build tests 12/12 from the mirror |
| A nightly race on a slow nested VM (health event, `ps`, `update`, pasta's first connection) | each test waits for its own condition | `failing_checks_make_it_unhealthy`, `ps_lists_the_containers_processes`, `update_changes_limits_live` |
| Root on `--network pasta` under Ubuntu 26.04's AppArmor profile for pasta (files only under `/tmp`, the user's runtime directory, the home) | root's pasta run directory in the temporary directory's place first (root's own, checked and swept as the others) | `roots_pasta_run_goes_in_the_temporary_directory`, `roots_pasta_run_leaves_no_mount_on_the_host` (CI's Ubuntu root pass), the 26.04 VM |
| A container refused in the moment `state` says stopped and its monitor is still finishing | the word may be "stopping": either is a refusal of an ended container | `cp_wants_a_live_container`, `exec_wants_a_created_or_running_container` (`ended_refusal`) |

## Fuzzing

No target was added in stage 15: no new parser of untrusted input (the API 1.44's fields go
through the `api_request` target's request parsing). The nightly `quality.yml` fuzzes
every target since 15.0. The close's run (2026-11-27, `scripts/test-linux.sh --fuzz 300`, arm64): all 19 targets,
300 s each, no crash, and no deprecation in the nightly toolchain's build.

## Known limitations (by design, for stage 15)

- The Mac's files aren't notarized (D15-2): Gatekeeper asks once.
- A start is over 100 ms (122 ms root, 168 ms rootless, `--network none`): reported, not
  blocking (D30).
- Until its `exec`, an `exec`'s command is root in the container and not yet under
  seccomp; with `no-new-privileges=false` it keeps `SYS_ADMIN` (`containers.md`, the
  threat model). Its set-up takes a `pids` slot, the container's memory and one key.
- On a Mac, Docker's socket in a container is root's in the VM, with the home, `/Volumes`,
  `/tmp` and `/var/folders` read-write: the docs say so.
- Rootful binds keep set-user-ID files and devices, as Docker's.
- A root host without the boot units keeps a restarting container's networks until it's
  started or removed (F203).
- F210: before Linux 5.12, a mount covered by another below a read-only bind stays
  writable.
- The runtime used directly as an OCI runtime takes no join lock: its `/proc` scan alone.
- A later rebuild of a release needs its kept assets (the `.apk` set) and Debian's
  snapshot service.

## Carried forward

- **To 15.6d:** v1.0.0-rc.1, then v1.0.0; the CHANGELOG's section dated and named; the
  first run of Ubuntu's root leg on `main` after #50 (off since it was found too slow: it
  may show failures not seen elsewhere).
- **The user's own:** GitHub's private vulnerability reporting turned on (and Support's
  purge of the old PR refs); at v1.0, the tap repository `Zena4L/homebrew-shipyard` and
  its `HOMEBREW_TAP_TOKEN`, then the user docs' install section published; stage 13's
  `scripts/macos/milestone-13.sh` and stage 14's manual checklist, still to run.
- **After v1.0:** F125, F156, F157, F159; notarization; F222 (each container's rx/tx rate
  on the map) and F223 (the desktop app's Linux package), both stage 14's, recorded at the
  close.
- **Accepted at v1.0,** each with its reason: 44 from 15.1 and the rests of the rows fixed
  in part, F203, F210, F212–F221 (F215 the runtime's refusals as text: a kind needs the
  monitor's protocol; F216 the stage's drift; F219 the Mac's build in CI).
- **Decisions to revisit:** `freezePrototype` (stage 14); Fedora on PRs if it gets fast;
  Ubuntu's root pass on PRs.

## Tests

| Where | Stage 15's |
|---|---|
| `crates/shipyard-cli/tests/api.rs` | `docker_s_cli_29_is_served_at_1_44`, `a_pull_says_its_layers_bytes_as_they_come`, `a_deleted_volume_s_files_go_after_the_answer`, `a_stop_whose_client_goes_lets_the_service_end`, `a_wait_for_a_container_that_never_ran_has_no_error`, `an_api_save_has_the_service_s_raised_limit` |
| `crates/shipyard-cli/tests/containers.rs` | `a_container_joins_another_s_pid_namespace`, `a_pid_join_waits_for_another_s_set_up`, `a_pid_joined_containers_cgroup_ends_with_its_monitor`, `a_joiner_is_told_the_container_it_joined_started_again` |
| `crates/shipyard-cli/tests/runtime.rs` | `exec_s_command_is_born_with_only_its_own_powers`, `exec_as_root_without_capabilities_starts_in_its_directory` |
| `crates/shipyard-cli/tests/vm.rs` | `docker_s_socket_is_given_to_a_container`, `a_gone_client_of_a_streaming_port_is_hung_up`, `a_glibc_amd64_image_runs_through_rosetta` |
| `crates/shipyard-cli/tests/` other | `firewall.rs` `a_link_outside_opens_nothing_new_on_the_host`; `signals.rs` `a_leftover_is_asked_to_stop_before_it_is_killed`; `build.rs` `a_context_leaves_the_temporary_directory_alone`; `terminal.rs`'s `interrupt_the_read` at five sites; `events.rs`'s linearity |
| `crates/shipyard-acceptance/tests/devcontainers.rs` | `a_devcontainer_comes_up_against_shipyard` |
| `crates/shipyard-runtime/tests/containers.rs` | `a_container_joins_another_s_pid_namespace`; the runtime's container tests, tester 13/13 and root 31/31 at 15.2b |
| Unit tests across the crates | about 80 new: `shipyard-api` 9, `shipyard-engine` 22, `shipyard-image` 14, `shipyard-network` 8, `shipyard-vm` 7, `shipyard-runtime` 6, `shipyard-cli` 6, `shipyard-sys` 3, `shipyard-storage` 2, `shipyard-client` 1 |
| `desktop/ui` | the inspector's "shared with NAME" (15.3c) |
| Scripts | `docs-site.sh`, `test-packages.sh`, `pasta-control.sh`, `compare.sh`, the release's dry runs |

**Totals per step** (`verify.sh`):

| Step | macOS | Linux both passes | VM |
|---|---|---|---|
| 15.2a | 1,669 / 0 | 5,081 / 1 (the new F139 test as root: fixed) | 48/48 |
| 15.2b | 1,673 / 0 | 5,106 / 0 | 48/48 |
| 15.2c1 | 1,675 / 0 | 5,112 / 0; Ubuntu 26.04 VM: terminal 84/84, API 66/66 | — |
| 15.2c2 | 1,680 / 0 | 5,127 / 1 (an `ETXTBSY` flake, fixed) | 48/48 |
| 15.2d | 1,684 / 0 | 5,140 / 0 | 48/48 |
| 15.2e1 | 1,692 / 0 | 5,160 / 0 | 48/48 |
| 15.2e2 | 1,697 / 0 | 5,171 / 1 (F193) | 48/48 |
| 15.2f1 | 1,704 / 0 | 5,176 / 0 | 47/48 (the guest's build: Alpine dropped 6.18.60) |
| 15.2f2a, f2b | unit and Linux crate tests | — | not run (the guest couldn't build) |
| 15.2g1 | 1,715 / 0 | 5,192 / 4 (rootless builds on DNS; 10/10 rerun) | — |
| 15.2g2 | 1,722 / 0 | 5,210 / 0 | — |
| 15.2g3 | 1,722 / 0 | 5,210 / 0 | 50/50 (f2a's and f2b's at last) |
| 15.3a | 1,723 / 0 | 5,214 / 0 | 51/51 |
| 15.3b | 1,723 / 0 | 5,214 / 0 | 51/51 |
| 15.3c | 1,724 / 0 | 5,224 / 0 | 51/51 |
| 15.4 | 1,724 / 0 | (no Rust) | — |
| 15.5a | 1,724 / 0 | 5,223 / 1 (an H7 bound 2 ms over beside a release build; 5/5 alone) | 51/51 |
| 15.5b | 1,725 / 0 | (no Linux code) | 51/51 |
| 15.6a | 1,725 / 0 | 5,228 / 0 | — |
| 15.6b1 | 1,727 / 0 | 5,236 / 0; build tests 12/12 | — |
| 15.6b2 | 1,727 / 0 | 5,242 / 0 | 50 + 1 (an older guest; `doctor` rightly warned) |
| 15.6b3a | (scripts) | (scripts) | 51/51 on the re-pinned guest |
| 15.6c1 | (Linux tests) | root 2,834 / 0 on CI's Ubuntu runner | — |
| **15.6c (the close)** | **1,728 / 0** | **5,244 / 0** (both passes), rustdoc, glibc and musl clippy, the desktop on Linux | **51/51** (the guest from this tree) |

**Mutants, hand-picked, controls first, per step:** 15.2a 11/11; 15.2b 11/11, 9/9 and
3/3; 15.2c1 3/3; 15.2c2 7/7; 15.2d 5/5; 15.2e1 11/12 (one equivalent: `ensure` returns at
`answers`); 15.2e2 8/8; 15.2f1 7/7; 15.2f2a 7/7; 15.2f2b 4/5 (one equivalent: `ends_with`
for an absolute path is `==`); 15.2g1 10/10; 15.2g2 12/12; 15.2g3 1/1; 15.3a 3/3; 15.3c
6/6; 15.6b2 6/6. In all 124 of 126, the two others equivalent. 15.3b's fix is in tests
(its evidence the counts); 15.4 to 15.6a and 15.6b3–b4 are scripts and workflows, checked
by controls and dry runs; 15.6b1 ran none.

**The milestone** (2026-11-27, the release's own packages from dry run 45485076316,
installed as users install them, each VM's earlier hand install removed first, its data
kept): on Ubuntu 24.04, Ubuntu 26.04 and Fedora (SELinux enforcing), as root and as the
VM's user, the package's files and notices, AppArmor's profile placed and loaded (Ubuntu),
a run, a network reached by name (root), a published port, `exec`, a `build` and its
image, a restart policy; the boot units and linger enabled; a reboot, both restart-policy
containers back; a clean removal. All passed on 24.04 and 26.04; on Fedora all but a root
bridge network's name lookup, refused by firewalld until its bridge is trusted, which
Shipyard's own warning says how to do (done, it resolved). The Mac: the dry run's tarball
unpacked, `doctor` found the guest beside the binary, a container ran and a published port
answered on the Mac's loopback. Then `main`'s first Ubuntu root pass (after 15.6b4) found
root on `--network pasta` broken on Ubuntu 26.04, which the milestone's root checks (on a
bridge) hadn't reached: 15.6c1 fixed it.

## Review history

| Step | What | A finding of its own reviews |
|---|---|---|
| 15.0 (no review) | public, Apache-2.0, CI, CONTRIBUTING | CI's first runs: the Fedora job brought up to stages 8–14; F199 and F200 recorded |
| 15.1 (the user's approval) | the triage of 82 findings | F95 already fixed (its rest F201) |
| 15.2a (clean-code 6.5, Murphy 7) | F162, F126, F150, F130, F123, F31, F139, F201 | F162's directory swapped after its check (fixed: the pinned directory); an F139 case reporting success for a lost exit; F201's test passed without the fix |
| 15.2b (b1 7/6, b2 7/5) | `--pid container:` and `host` | a joiner holding `CAP_SYS_PTRACE` could trace a later joiner's set-up (critical); the parent-death check by `getppid` never fired |
| 15.2c1 (7.5, 8) | Ubuntu 26.04's pasta, uutils, F200 | an unbounded wait on a pasta it couldn't kill; one grace for the whole sweep |
| 15.2c2 (7, 7.5) | F136(a), F111 | closing a `br0` uplink would have dropped its bridged VMs' new connections (medium) |
| 15.2d (7.5, 7.5) | F165, F166, F174 | refusing a cgroup-less rootless build broke `RUN`-less builds; a merge by manifest alone gave a name another index's digest |
| 15.2e1 (7.5, 7) | F194, F195, F196, F198 | the in-process delete held the image store's lock (an `rmi` after waited 15 s); proxy and CA variables lost under the unit |
| 15.2e2 (7.5, 6) | F192 | a client that stops reading stalled every download thread; a 120 s bound too short for pulls that succeed |
| 15.2f1 (8, 7) | F182, F180(e), F183, F185 | over SSH (no `gui/UID`), every command failed |
| 15.2f2a (7.5, 7) | F175, F180(f) | a hang-up after a clean end could cut an upload's tail; the firewall raced the data disk's mount |
| 15.2f2b (7, 4) | F171 | every bind failed at the start (high: checked and canonicalised before the mapping); a restarted agent started a second service |
| 15.2g1 (7, 8) | API 1.44 | `ReadOnlyForceRecursive` without `ReadOnly` gave a writable bind (medium) |
| 15.2g2 (7.5, 7) | F208, F209, F210 | F210's refusal turned away `-v /:/host:ro` and guarded nothing (accepted, asked) |
| 15.2g3 (the VM run) | the guest on 6.18.64, #34's and #35's VM tests | `run --pid host` with a bad command said nothing (since 15.2b2); both VM tests failed on their own bugs first |
| 15.3a (8.5, 8) | F199, F187, F170 | the events ratio sat at its bound for the quadratic read with a slow start |
| 15.3b (7) | F193 | the test still typed its own Ctrl-C before the helper's |
| 15.3c (7, 7) | F202, F204, F205, F206 | the stop's undo of its mark could clear another's (a stopped container came back at boot); `cgroup.kill` could be the caller's cgroup |
| 15.4 (7.5) | the docs site | links leaving the site passed locally and 404 published; a corrupt cached tool trusted |
| 15.5a (6.5) | release builds | the guest's downloads in the checkout made it `-dirty` (every Mac warned); attestations tied to nothing |
| 15.5b (6.5) | packages, Homebrew | `vm install` wrote the keg's versioned path into launchd's plist (high); the `.deb`'s unit directories 0644 (high) |
| 15.6a (7) | devcontainers, the report, the threat model | four wrong claims in the threat model; the report's first rootless timings were of failures |
| 15.6b (seven reviewers: 8, 7; 7, 8.5, 8, 6, 7.5) | the whole-stage and release reviews | `exec`'s set-up visible to a rootful `SYS_PTRACE` container (high); `same_uplink` routed `eth0`→`eth0` and `wg0`→`wg0` |
| 15.6b1 (no further review) | the confirmed fixes | the runtime's refusals can't be kinds without the monitor's protocol (F215) |
| 15.6b2 (plan Murphy 7; 7.5, 7.5) | `exec` confined before the clone | `exec` inherited the caller's keyring (plan); root with `--cap-drop ALL` couldn't start in another's 0700 directory |
| 15.6b3a (7.5) | inputs kept | a cancelled run published a draft; a checkout's guest still took `musl-tools` from the live archive |
| 15.6b3b (6.5) | notices, sources | the page's bundle, Rust's standard library and musl missing from the notices |
| 15.6b4 (no review) | CI's mirror, the nightly's races, root on `main` | the nightly's four failures were the tests' races, not Shipyard's |
| 15.6c1 (CI's first Ubuntu root pass) | root's pasta run directory; tests free of GNU coreutils' wording | root on pasta broken under Ubuntu 26.04's AppArmor profile for pasta (its run directory in `/run/shipyard`); 7 of 12 failures |
| 15.6c (the close) | these notes, fuzz, `verify.sh`, the milestone | Fedora's firewalld refuses a bridge's DNS until it's trusted (known, said by Shipyard's own warning); my milestone script's rootless bridge (refused by design) |

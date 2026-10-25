#!/usr/bin/env bash
# Runs the Linux CI checks in a Linux container: clippy, the test suite as a
# normal user, then everything again as root, including the root-only tests
# (as the CI `root` job does). For Linux-only code (subreaper cleanup,
# close_range, signal sources, credential switching, namespaces, mounts) on a
# Mac, or any machine with Docker.
#
#   scripts/test-linux.sh                # run everything
#   scripts/test-linux.sh --shell        # a shell in the same container, to debug
#   scripts/test-linux.sh --root-shell   # the same, as root, with / made shared
#   scripts/test-linux.sh --fuzz [SECONDS]  # each fuzz target (fuzz/) for SECONDS (300)
#   scripts/test-linux.sh --mutants [BASE|FILE.diff]  # cargo-mutants on the changes
#                                        # against BASE (main), committed or not,
#                                        # or on FILE.diff's: a report, as tester
#   scripts/test-linux.sh --conformance [NAME…]  # the OCI conformance suite, as
#                                        # scripts/conformance/expected.txt expects
#
# The container is privileged: creating namespaces and mounting need
# CAP_SYS_ADMIN, and Docker's default seccomp profile blocks `clone3` and
# namespace flags. The root pass uses that; the normal-user pass still has no
# capabilities, so it sees what an unprivileged user would. For development
# only: never run untrusted code this way.
#
# The image is `rust:1-bookworm` plus what the user-namespace tests need
# (scripts/test-image/Dockerfile: `newuidmap`, and a `tester` user with
# subordinate IDs). It's built the first time, which needs network, and again
# only when the Dockerfile changes (its tag is the file's hash); after that,
# runs need no network. `--init` gives the container a PID 1 that reaps
# orphans: the terminal tests kill processes and wait for them to be gone,
# and a PID 1 that never reaps (a plain `timeout`, say) leaves them zombies.
#
# The repository is mounted read-only: nothing in it changes. Build output,
# the cargo registry and the pinned toolchain live in named Docker volumes
# (shipyard-linux-*), so later runs are fast. Remove them with
#   docker volume rm shipyard-linux-target shipyard-linux-cargo shipyard-linux-rustup
# TARGET_VOLUME names another build volume (default shipyard-linux-target), for a
# second checkout's runs beside this one's (scripts/watch.sh uses its own).
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
dockerfile="$repo/scripts/test-image/Dockerfile"
if command -v sha256sum >/dev/null; then
    hash=$(sha256sum "$dockerfile")
else
    hash=$(shasum -a 256 "$dockerfile")
fi
image="shipyard-test:${hash:0:12}"
if ! docker image inspect "$image" >/dev/null 2>&1; then
    echo "building the test image $image (once; needs network)" >&2
    docker build --quiet --tag "$image" - <"$dockerfile" >/dev/null
fi

mode="test"
fuzz_seconds=300
conformance_tests=""
mutants_diff=""
case "${1:-}" in
    "") ;;
    --shell) mode=shell ;;
    --root-shell) mode=root-shell ;;
    --fuzz)
        mode=fuzz
        fuzz_seconds=${2:-300}
        ;;
    --mutants)
        mode=mutants
        # The changes, from the host's git (the container's would see a
        # repository owned by someone else): against where BASE and HEAD
        # meet, the working tree's included, so uncommitted work counts.
        if [ -f "${2:-}" ]; then
            mutants_diff=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
        else
            base=${2:-main}
            mutants_diff=$(mktemp "${TMPDIR:-/tmp}/shipyard-mutants.XXXXXX")
            git -C "$repo" diff "$(git -C "$repo" merge-base "$base" HEAD)" >"$mutants_diff"
        fi
        if [ ! -s "$mutants_diff" ]; then
            echo "no changes to mutate" >&2
            exit 0
        fi
        ;;
    --conformance)
        mode=conformance
        shift
        conformance_tests="$*"
        # Its own image, on top of the test image: runtime-tools and its
        # root filesystem (scripts/conformance/Dockerfile), tagged by both
        # files' hash.
        conformance_dockerfile="$repo/scripts/conformance/Dockerfile"
        if command -v sha256sum >/dev/null; then
            chash=$(cat "$dockerfile" "$conformance_dockerfile" | sha256sum)
        else
            chash=$(cat "$dockerfile" "$conformance_dockerfile" | shasum -a 256)
        fi
        if ! docker image inspect "shipyard-conformance:${chash:0:12}" >/dev/null 2>&1; then
            echo "building the conformance image (once; needs network)" >&2
            docker build --quiet --build-arg BASE="$image" \
                --tag "shipyard-conformance:${chash:0:12}" - <"$conformance_dockerfile" >/dev/null
        fi
        image="shipyard-conformance:${chash:0:12}"
        ;;
    *)
        echo "usage: $0 [--shell | --root-shell | --fuzz [SECONDS] | --conformance [NAME…]]" >&2
        exit 2
        ;;
esac

# Runs inside the container, as root.
read -r -d '' inside <<'EOF' || true
set -euo pipefail

# `tester`, the image's normal user, owns the build volumes.
chown -R tester /target /usr/local/cargo /usr/local/rustup

# cgroups, as a systemd host has them (stage 5):
# - this container's cgroup namespace root holds our processes, so it can't
#   hand controllers to children (the kernel's "no internal processes" rule):
#   move them into a leaf, as docker-in-docker does, then enable the
#   controllers, each it has;
# - `tester` gets a subtree of its own, as systemd's Delegate= gives each user
#   `user@UID.service` (here `user.slice/tester`): the directories and their
#   cgroup.procs, cgroup.subtree_control and cgroup.threads are tester's, and
#   its shells start inside (`user.slice/tester/session`). Like systemd's
#   default (Delegate=pids memory cpu), only those three controllers reach it:
#   `user.slice`, root's, hands down nothing else.
#
# Only at the root of this container's own cgroup namespace (`0::/`): with
# Docker's `--cgroupns=host`, /sys/fs/cgroup would be the host's (or the VM's)
# real root, which this must never rearrange. There, the cgroup tests find no
# delegation and skip.
cgroup=/sys/fs/cgroup
if [ "$(cat /proc/self/cgroup)" = "0::/" ] && [ -w "$cgroup/cgroup.subtree_control" ]; then
    mkdir -p "$cgroup/init"
    # A few passes (processes started meanwhile), but never forever: one
    # that can't move must fail the run, not hang it.
    for _ in 1 2 3 4 5; do
        [ -z "$(cat "$cgroup/cgroup.procs")" ] && break
        while read -r pid; do
            echo "$pid" >"$cgroup/init/cgroup.procs" 2>/dev/null || true
        done <"$cgroup/cgroup.procs"
    done
    if [ -n "$(cat "$cgroup/cgroup.procs")" ]; then
        echo "test-linux.sh: can't move these processes out of the cgroup root: $(cat "$cgroup/cgroup.procs")" >&2
        exit 1
    fi
    mkdir -p "$cgroup/user.slice/tester/session"
    for controller in $(cat "$cgroup/cgroup.controllers"); do
        echo "+$controller" >"$cgroup/cgroup.subtree_control"
    done
    echo "+cpu +memory +pids" >"$cgroup/user.slice/cgroup.subtree_control"
    echo "+cpu +memory +pids" >"$cgroup/user.slice/tester/cgroup.subtree_control"
    for dir in user.slice/tester user.slice/tester/session; do
        chown tester "$cgroup/$dir" "$cgroup/$dir/cgroup.procs" \
            "$cgroup/$dir/cgroup.subtree_control" "$cgroup/$dir/cgroup.threads"
    done
fi

# A real /tmp: the container's own is overlayfs, which refuses what an image
# store needs (whiteout devices for a normal user; an overlay's upper layer).
# A host's /tmp is a disk or tmpfs, as here. (A rootless store on tmpfs also
# needs its `user.*` extended attributes: Linux 6.6 or later.)
mount -t tmpfs -o mode=1777 tmpfs /tmp

# Drops to `tester` (all its IDs, no capabilities: what an ordinary user
# has), keeping the toolchain's environment, in its delegated cgroup.
as_tester() (
    if [ -w "$cgroup/user.slice/tester/session/cgroup.procs" ]; then
        echo "$BASHPID" >"$cgroup/user.slice/tester/session/cgroup.procs"
    fi
    exec setpriv --reuid tester --regid tester --init-groups \
        env HOME=/home/tester "PATH=$PATH" "CARGO_HOME=$CARGO_HOME" \
        SHIPYARD_TEST_DELEGATED=required SHIPYARD_TEST_ROOTLESS=required \
        SHIPYARD_TEST_TOOLS=required SHIPYARD_TEST_NETWORK=required \
        "RUSTUP_HOME=$RUSTUP_HOME" "CARGO_TARGET_DIR=$CARGO_TARGET_DIR" "$@"
)

# Becomes the root pass's environment:
# - `/` shared, like a systemd host, where a mount namespace that isn't made
#   private would leak mounts out (Docker's own `/` is private, which would
#   hide that bug; only this container is affected);
# - whatever root builds goes back to `tester` on the way out, so the next
#   normal-user pass can write to it.
as_root() {
    mount --make-rshared /
    trap 'chown -R tester /target /usr/local/cargo /usr/local/rustup' EXIT
}

case "$MODE" in
    shell)
        echo "shell as tester; the repo is at /src (read-only)"
        as_tester bash
        exit
        ;;
    root-shell)
        as_root
        echo "shell as root (/ is shared); the repo is at /src (read-only)"
        bash
        exit
        ;;
    fuzz)
        # As `tester`: a layer that escaped its directory reaches only what
        # a normal user can. Nightly and cargo-fuzz are installed once, into
        # the toolchain volumes (that needs network). The corpora, and any
        # input that crashed a target, are kept in the build volume, under
        # /target/fuzz.
        as_tester bash -c '
            set -euo pipefail
            rustup toolchain install nightly --profile minimal >/dev/null
            [ -x "$CARGO_HOME/bin/cargo-fuzz" ] || cargo install --locked cargo-fuzz
            python3 scripts/fuzz-seeds.py /target/fuzz/corpus
            # cargo-fuzz writes beside its crate, and /src is read-only: a
            # copy, pointed back at the sources.
            rm -rf /target/fuzz/crate
            cp -r fuzz /target/fuzz/crate
            sed -i "s|path = \"../crates/|path = \"/src/crates/|" /target/fuzz/crate/Cargo.toml
            # Every target, whichever crashes: the crashed ones named at the
            # end (as the Quality workflow does).
            crashed=()
            for target in $(scripts/fuzz-targets.sh); do
                echo "== fuzz $target, $FUZZ_SECONDS s"
                mkdir -p "/target/fuzz/crashes/$target"
                cargo +nightly fuzz run --fuzz-dir /target/fuzz/crate --target-dir /target/fuzz/build \
                    "$target" "/target/fuzz/corpus/$target" -- \
                    -max_total_time="$FUZZ_SECONDS" \
                    -artifact_prefix="/target/fuzz/crashes/$target/" || crashed+=("$target")
            done
            if [ "${#crashed[@]}" -gt 0 ]; then
                echo "== crashed: ${crashed[*]} (inputs in /target/fuzz/crashes)"
                exit 1
            fi
            echo "== no fuzz target crashed"
        '
        exit
        ;;
    mutants)
        # As `tester`, unprivileged, with each crate's own tests
        # (.cargo/mutants.toml): a report, not a gate. A mutant only a root
        # test catches shows as missed: those are the step's own runner's.
        # cargo-mutants is installed once into the toolchain volume (that
        # needs network); its copies and builds go to the build volume.
        as_tester bash -c '
            set -euo pipefail
            [ -x "$CARGO_HOME/bin/cargo-mutants" ] || cargo install --locked cargo-mutants --version 27.1.0
            mkdir -p /target/mutants/tmp
            # Never an earlier run'"'"'s results.
            rm -rf /target/mutants/mutants.out
            # Each mutant builds in its own copy, with its own target dir:
            # never the shared one (a shared dir runs the wrong binary).
            status=0
            TMPDIR=/target/mutants/tmp env -u CARGO_TARGET_DIR cargo mutants --in-diff /changes.diff \
                --output /target/mutants --no-shuffle -j 2 || status=$?
            # 2 (missed) and 3 (timeouts) are the report; anything else
            # (a stale diff, a baseline that fails) is a failure to run.
            case $status in
                0 | 2 | 3) ;;
                *) echo "== cargo-mutants failed to run (exit $status)"; exit "$status" ;;
            esac
            for outcome in caught missed timeout unviable; do
                file=/target/mutants/mutants.out/$outcome.txt
                echo "$outcome: $( [ -f "$file" ] && wc -l <"$file" || echo 0 )"
            done
            if [ -s /target/mutants/mutants.out/missed.txt ]; then
                echo "== missed"
                cat /target/mutants/mutants.out/missed.txt
            fi
        '
        exit
        ;;
    conformance)
        # Built as `tester`, run as root: the suite runs containers as root.
        as_tester cargo build --locked -p shipyard-cli --bin shipyard-runtime
        as_root
        # Not `exec`: as_root's trap gives tester its files back on exit.
        # shellcheck disable=SC2086 # the test names, split
        scripts/conformance/run.sh $CONFORMANCE_TESTS
        exit
        ;;
esac

echo "== clippy"
as_tester cargo clippy --locked --workspace --all-targets -- -D warnings
# The tests see a resolver on the host's loopback, as on a systemd host
# (systemd-resolved's 127.0.0.53, as on CI's Ubuntu runners), not Docker's: a
# container with a network of its own can't reach it, and runs say so. (Every
# crate was fetched by clippy: nothing below needs the network's DNS.)
printf 'nameserver 127.0.0.53\noptions edns0 trust-ad\n' >/tmp/resolv.conf
mount --bind /tmp/resolv.conf /etc/resolv.conf
# Both passes run whole (`--no-fail-fast`: every test binary, not up to the
# first that fails), and the root pass runs even if the first failed; then
# any failure fails the run.
failed=
echo "== tests, as a normal user"
# The build tests (12.3) run on their own after both passes (below).
as_tester env SHIPYARD_TEST_BUILD=later cargo test --locked --workspace --no-fail-fast \
  || failed="$failed tester"
as_root
echo "== tests, including the root-only ones, run as root"
# The image has ip, nft and dig: the differential tests (H5) must run.
SHIPYARD_TEST_TOOLS=required SHIPYARD_TEST_NETWORK=required SHIPYARD_TEST_BUILD=later \
  cargo test --locked --workspace --no-fail-fast -- --include-ignored \
  || failed="$failed root"
# The build tests (12.3) need a registry by name: a rootless builder's DNS
# goes through pasta to the host's resolver, which the loopback one above
# isn't (nothing serves it here; a systemd host's does). Run with the
# container's own resolver, after the passes, four at a time, each step on a
# fresh /tmp (each test holds a BuildKit image there: beside what the passes
# left, rootless stores' subordinate files among it, the tmpfs ran out).
umount /etc/resolv.conf
fresh_tmp() {
    umount -l /tmp && mount -t tmpfs -o mode=1777 tmpfs /tmp
}
echo "== build tests, as a normal user"
fresh_tmp
as_tester cargo test --locked -p shipyard-cli --test build -- --test-threads 4 \
  || failed="$failed build-tester"
echo "== build tests, as root"
fresh_tmp
SHIPYARD_TEST_NETWORK=required \
  cargo test --locked -p shipyard-cli --test build -- --include-ignored --test-threads 4 \
  || failed="$failed build-root"
if [ -n "$failed" ]; then
  echo "== tests failed:$failed"
  exit 1
fi
echo "== all Linux checks passed"
EOF

tty_flags=()
if [ "$mode" = shell ] || [ "$mode" = root-shell ]; then
    tty_flags=(--interactive --tty)
fi
diff_flags=()
if [ "$mode" = mutants ]; then
    diff_flags=(--volume "$mutants_diff:/changes.diff:ro")
fi

# (`${a[@]+"${a[@]}"}`: an empty array is "unbound" to `set -u` in bash before
# 4.4, which is macOS's /bin/bash.)
exec docker run --rm --privileged --init ${tty_flags[@]+"${tty_flags[@]}"} \
    ${diff_flags[@]+"${diff_flags[@]}"} \
    --volume "$repo:/src:ro" \
    --volume "${TARGET_VOLUME:-shipyard-linux-target}:/target" \
    --volume shipyard-linux-cargo:/usr/local/cargo \
    --volume shipyard-linux-rustup:/usr/local/rustup \
    --env CARGO_TARGET_DIR=/target \
    --env MODE="$mode" \
    --env FUZZ_SECONDS="$fuzz_seconds" \
    --env CONFORMANCE_TESTS="$conformance_tests" \
    --workdir /src \
    "$image" bash -c "$inside"

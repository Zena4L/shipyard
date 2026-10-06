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
#
# The container is privileged: creating namespaces and mounting need
# CAP_SYS_ADMIN, and Docker's default seccomp profile blocks `clone3` and
# namespace flags. The root pass uses that; the normal-user pass still has no
# capabilities, so it sees what an unprivileged user would. For development
# only: never run untrusted code this way.
#
# Nothing is installed in the container: the image has what the tests use
# (`setpriv` and `useradd` from util-linux, `pgrep` and `ps` from procps), so
# the run needs no network. `--init` gives the container a PID 1 that reaps
# orphans: the terminal tests kill processes and wait for them to be gone,
# and a PID 1 that never reaps (a plain `timeout`, say) leaves them zombies.
#
# The repository is mounted read-only: nothing in it changes. Build output,
# the cargo registry and the pinned toolchain live in named Docker volumes
# (shipyard-linux-*), so later runs are fast. Remove them with
#   docker volume rm shipyard-linux-target shipyard-linux-cargo shipyard-linux-rustup
set -euo pipefail

image=rust:1-bookworm
repo=$(cd "$(dirname "$0")/.." && pwd)

mode=test
case "${1:-}" in
    "") ;;
    --shell) mode=shell ;;
    --root-shell) mode=root-shell ;;
    *)
        echo "usage: $0 [--shell | --root-shell]" >&2
        exit 2
        ;;
esac

# Runs inside the container, as root.
read -r -d '' inside <<'EOF' || true
set -euo pipefail

# A normal user for the main run.
id tester >/dev/null 2>&1 || useradd --create-home tester
chown -R tester /target /usr/local/cargo /usr/local/rustup

# Drops to `tester` (all its IDs, no capabilities: what an ordinary user
# has), keeping the toolchain's environment.
as_tester() {
    setpriv --reuid tester --regid tester --init-groups \
        env HOME=/home/tester "PATH=$PATH" "CARGO_HOME=$CARGO_HOME" \
        "RUSTUP_HOME=$RUSTUP_HOME" "CARGO_TARGET_DIR=$CARGO_TARGET_DIR" "$@"
}

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
esac

echo "== clippy"
as_tester cargo clippy --locked --workspace --all-targets -- -D warnings
echo "== tests, as a normal user"
as_tester cargo test --locked --workspace
as_root
echo "== tests, including the root-only ones, run as root"
cargo test --locked --workspace -- --include-ignored
echo "== all Linux checks passed"
EOF

tty_flags=()
if [ "$mode" != test ]; then
    tty_flags=(--interactive --tty)
fi

# (`${a[@]+"${a[@]}"}`: an empty array is "unbound" to `set -u` in bash before
# 4.4, which is macOS's /bin/bash.)
exec docker run --rm --privileged --init ${tty_flags[@]+"${tty_flags[@]}"} \
    --volume "$repo:/src:ro" \
    --volume shipyard-linux-target:/target \
    --volume shipyard-linux-cargo:/usr/local/cargo \
    --volume shipyard-linux-rustup:/usr/local/rustup \
    --env CARGO_TARGET_DIR=/target \
    --env MODE="$mode" \
    --workdir /src \
    "$image" bash -c "$inside"

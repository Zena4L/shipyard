#!/usr/bin/env bash
# Runs the Linux CI checks in a Linux container: clippy, the test suite as a
# normal user, then the root-only tests through `sudo` (as the CI `root` job
# does). For Linux-only code (subreaper cleanup, close_range, signal sources,
# credential switching) on a Mac, or any machine with Docker.
#
#   scripts/test-linux.sh            # run everything
#   scripts/test-linux.sh --shell    # a shell in the same container, to debug
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
    *)
        echo "usage: $0 [--shell]" >&2
        exit 2
        ;;
esac

# Runs inside the container, as root.
read -r -d '' inside <<'EOF' || true
set -euo pipefail
apt-get update -qq
apt-get install -y -qq procps sudo >/dev/null

# A normal user for the main run, with passwordless sudo for the root pass.
id tester >/dev/null 2>&1 || useradd --create-home tester
echo 'tester ALL=(ALL) NOPASSWD:ALL' >/etc/sudoers.d/tester
chown -R tester /target /usr/local/cargo /usr/local/rustup

# The test runner variable is named after the target triple.
case "$(uname -m)" in
    aarch64) runner=CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER ;;
    x86_64) runner=CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER ;;
    *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

# Through `env`: sudo looks commands up in its own `secure_path`, which
# doesn't include cargo's directory.
as_tester() {
    sudo -u tester env HOME=/home/tester "PATH=$PATH" "CARGO_HOME=$CARGO_HOME" \
        "RUSTUP_HOME=$RUSTUP_HOME" "CARGO_TARGET_DIR=$CARGO_TARGET_DIR" "$@"
}

if [ "$MODE" = shell ]; then
    echo "shell as tester; the repo is at /src (read-only)"
    as_tester bash
    exit
fi

echo "== clippy"
as_tester cargo clippy --locked --workspace --all-targets -- -D warnings
echo "== tests, as a normal user"
as_tester cargo test --locked --workspace
echo "== tests, including the root-only ones, run as root"
as_tester env "$runner=sudo -E" cargo test --locked --workspace -- --include-ignored
echo "== all Linux checks passed"
EOF

tty_flags=()
if [ "$mode" = shell ]; then
    tty_flags=(--interactive --tty)
fi

exec docker run --rm "${tty_flags[@]}" \
    --volume "$repo:/src:ro" \
    --volume shipyard-linux-target:/target \
    --volume shipyard-linux-cargo:/usr/local/cargo \
    --volume shipyard-linux-rustup:/usr/local/rustup \
    --env CARGO_TARGET_DIR=/target \
    --env MODE="$mode" \
    --workdir /src \
    "$image" bash -c "$inside"

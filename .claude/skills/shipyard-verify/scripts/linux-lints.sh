#!/usr/bin/env bash
# The Linux checks `scripts/test-linux.sh` doesn't run: rustdoc as CI's docs job
# runs it (private items, warnings denied) and clippy for musl, which catches
# what glibc hides (libc types differ). Glibc clippy too, for a quick look
# without the test passes.
#
# Run it as root in the test container (musl-tools is installed with apt):
#   timeout 3000 run-in-linux.sh root linux-lints.sh
# Exits non-zero if any check failed; each one's first errors are printed.
set -uo pipefail
cd /src
failed=0

check() {
    local name=$1
    shift
    echo "== $name"
    local log
    log=$(mktemp)
    if "$@" >"$log" 2>&1; then
        echo "ok"
    else
        echo "FAILED"
        grep -E '^(error|warning)' -A6 "$log" | head -30
        failed=1
    fi
    rm -f "$log"
}

check "clippy glibc" cargo clippy --locked --workspace --all-targets -- -D warnings
check "rustdoc" env RUSTDOCFLAGS="-D warnings" \
    cargo doc --locked --workspace --no-deps --document-private-items
# The image keeps no package lists (its Dockerfile removes them): fetched first.
# (apt over IPv4 alone: the mirror answers on IPv6 first, and Docker's VM
# may have no IPv6 route.)
apt=(apt-get -o Acquire::ForceIPv4=true -o Acquire::Retries=30 -o Acquire::http::Timeout=5)
command -v musl-gcc >/dev/null || { "${apt[@]}" update >/dev/null 2>&1 && "${apt[@]}" install -y musl-tools >/dev/null 2>&1; } \
    || echo "musl-tools couldn't be installed (no network?): clippy musl will fail" >&2
rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1
check "clippy musl" env CC_aarch64_unknown_linux_musl=musl-gcc \
    cargo clippy --locked --workspace --all-targets --target aarch64-unknown-linux-musl \
    -- -D warnings

exit $failed

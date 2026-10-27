#!/usr/bin/env bash
# Shipyard's check of record, in one command (AGENTS.md section 7):
#
#   verify.sh            everything: macOS, then Linux (both test passes, then
#                        rustdoc and the clippy targets the passes don't run)
#   verify.sh --mac      fmt, clippy, the macOS suite, cargo deny
#   verify.sh --linux    the Linux half only
#   verify.sh --vm       the macOS VM's: the guest built from this tree, then
#                        the VM tests (each boots a VM; four at once)
#
# On a Mac, everything includes the VM's part.
#
# Each check's full output goes to $VERIFY_LOGS (default target/verify/), and a
# summary with the test totals is printed at the end. It exits non-zero if any
# check failed. The Linux half takes about an hour: run it in the background.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(git -C "$here" rev-parse --show-toplevel)
cd "$repo"
logs=${VERIFY_LOGS:-$repo/target/verify}
mkdir -p "$logs"
mode=${1:---all}
case $mode in
--all | --mac | --linux | --vm) ;;
*)
    echo "usage: verify.sh [--all|--mac|--linux|--vm]" >&2
    exit 2
    ;;
esac

summary=()
failed=0

# Totals of every `test result:` line in a log: "passed P, failed F, ignored I".
totals() {
    grep -E '^test result' "$1" |
        awk '{p+=$4; f+=$6; i+=$8} END {printf "passed %d, failed %d, ignored %d", p, f, i}'
}

# run NAME LOGFILE COMMAND...: runs it, logs it, records ok or FAILED.
run() {
    local name=$1 log=$2
    shift 2
    echo "== $name (log: $log)"
    if "$@" >"$log" 2>&1; then
        summary+=("ok      $name")
    else
        summary+=("FAILED  $name")
        failed=1
        tail -20 "$log"
    fi
}

if [ "$mode" != --linux ] && [ "$mode" != --vm ]; then
    run "fmt" "$logs/fmt.log" cargo fmt --all --check
    run "clippy (macOS)" "$logs/clippy-mac.log" \
        cargo clippy --workspace --all-targets -- -D warnings
    run "tests (macOS)" "$logs/test-mac.log" cargo test --workspace --no-fail-fast
    summary+=("        tests (macOS): $(totals "$logs/test-mac.log")")
    run "cargo deny" "$logs/deny.log" cargo deny check
    # The fuzz targets' own code (`#[cfg(fuzzing)]`) no other build compiles:
    # 11.6c's fuzz run found it broken by a signature change.
    run "fuzz-only code (--cfg fuzzing)" "$logs/cfg-fuzzing.log" \
        env RUSTFLAGS="--cfg fuzzing" CARGO_TARGET_DIR=target/cfg-fuzzing \
        cargo check --workspace
    # A Mac's signing (step 13.7): one identity across builds, in a throwaway
    # keychain; the minimum macOS. (Off a Mac there's nothing to sign.)
    if [ "$(uname)" = Darwin ]; then
        run "signing (macOS)" "$logs/sign.log" bash -c \
            'cargo build -p shipyard-cli --bins && scripts/macos/test-sign.sh target/debug/shipyard target/debug/shipyard-runtime'
    fi
fi

if [ "$mode" = --all ] || [ "$mode" = --linux ]; then
    run "Linux: clippy + tests as tester + as root" "$logs/test-linux.log" \
        timeout 7200 scripts/test-linux.sh
    summary+=("        tests (Linux, both passes): $(totals "$logs/test-linux.log")")
    run "Linux: rustdoc, clippy glibc + musl" "$logs/linux-lints.log" \
        timeout 3000 "$here/run-in-linux.sh" root "$here/linux-lints.sh"
fi

if [ "$mode" = --vm ] || { [ "$mode" = --all ] && [ "$(uname)" = Darwin ]; }; then
    run "the macOS guest, built from this tree" "$logs/guest.log" \
        timeout 3600 scripts/guest/build.sh
    run "macOS VM tests" "$logs/test-vm.log" \
        env SHIPYARD_TEST_VM=required timeout 3600 \
        cargo test -p shipyard-cli --test vm --test vm_terminal --test vm_docker --test vm_launchd --no-fail-fast
    summary+=("        tests (macOS VM): $(totals "$logs/test-vm.log")")
fi

echo
echo "== summary ($mode)"
printf '%s\n' "${summary[@]}"
exit $failed

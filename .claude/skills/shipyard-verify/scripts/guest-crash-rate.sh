#!/usr/bin/env bash
# How often the macOS guest's kernel crashes (F132): boots fresh guests,
# each with the VM tests' kind of work, and counts those whose console shows
# an oops or that hang. A diagnosis tool, macOS only.
#
# usage: guest-crash-rate.sh N P [GUEST_DIR] [OUT_DIR]
#   N boots, P at a time; GUEST_DIR the guest's files (default
#   target/guest, scripts/guest/build.sh's); OUT_DIR keeps each boot's
#   console and log (default target/guest-crash-rate/<time>).
# Settings to compare go through the environment, as the VM reads them:
#   SHIPYARD_VM_CPUS, SHIPYARD_VM_MEMORY_MIB (diagnosis overrides).
#
# The work: a container, the home's share bound in (read, then 64 MiB
# written and read back), a detached container on the bridge network (its
# firewall's modules), an `exec`, its removal.
#
# Each of the P slots keeps its network card's address from boot to boot
# (its `network.json`), as one VM does: a new address each boot would take
# another of the Mac's NAT leases (253, an hour each) and use them up.
#
# Prints a line per boot (ok, CRASH, HANG, or NET: the guest had its lease
# and a lookup failed, the Mac's network, not counted; a guest with no lease
# is a HANG), with its seconds, then the rate with its 95% bounds (Wilson).
# Each boot is bounded (90 s a step, 150 s its stop); a slot whose VM
# didn't stop gets a new address. Interrupted, it stops its VMs and removes
# their directories.
set -uo pipefail
repo=$(cd "$(dirname "$0")/../../../.." && pwd)
n=${1:?usage: guest-crash-rate.sh N P [GUEST_DIR] [OUT_DIR]}
p=${2:?usage: guest-crash-rate.sh N P [GUEST_DIR] [OUT_DIR]}
guest=${3:-$repo/target/guest}
out=${4:-$repo/target/guest-crash-rate/$(date +%Y%m%d-%H%M%S)}
shipyard=$repo/target/debug/shipyard
# Docker Hub's own, through its mirror: a fresh VM pulls at each boot, past
# Docker Hub's anonymous limit (100 in 6 hours).
image=${IMAGE:-mirror.gcr.io/library/alpine:3.20}
mkdir -p "$out"
# Absolute, as the VM wants them.
guest=$(cd "$guest" && pwd)
out=$(cd "$out" && pwd)
[ -x "$shipyard" ] || { echo "build it first: cargo build -p shipyard-cli" >&2; exit 2; }
[ -f "$guest/manifest.json" ] || { echo "no guest at $guest: scripts/guest/build.sh" >&2; exit 2; }
command -v timeout >/dev/null || { echo "needs GNU timeout (brew install coreutils)" >&2; exit 2; }
"$repo/scripts/macos/sign.sh" "$shipyard" >/dev/null
# The Mac kept awake while it runs: asleep, its guests lose their network
# and every boot fails as a hang (11.5a's first 6.12 run).
caffeinate -i -w $$ &

# Its VMs stopped and their directories removed, however it ends.
cleanup() {
    local dir
    for dir in "$HOME"/.shipyard-crash-rate-$$-*; do
        case $dir in *-share | *"*"*) continue ;; esac
        timeout 150 env -u SHIPYARD_VM SHIPYARD_VM_DIR="$dir" "$shipyard" vm stop \
            </dev/null >/dev/null 2>&1
    done
    rm -rf "$HOME"/.shipyard-crash-rate-$$-*
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# One boot: its own VM directory (under the home: the VM shares it), its
# work, its verdict on stdout.
boot() {
    local i=$1 dir="$HOME/.shipyard-crash-rate-$$-$1" log="$out/boot-$1.log" started verdict
    local card="$out/slot-$(((i - 1) % p + 1))-network.json"
    started=$(date +%s)
    rm -rf "$dir"
    mkdir -m 700 -p "$dir"
    [ -f "$card" ] && cp "$card" "$dir/network.json"
    run() {
        timeout 90 env -u SHIPYARD_VM SHIPYARD_VM_DIR="$dir" SHIPYARD_GUEST_DIR="$guest" \
            "$shipyard" "$@" </dev/null >>"$log" 2>&1
    }
    # A directory of the home's, through the VM's share (virtiofs), as
    # the VM tests bind it (beside the VM's directory: that one the guest
    # hides).
    local share="$dir-share"
    rm -rf "$share"
    mkdir -p "$share"
    echo "from the mac" >"$share/note"
    {
        run vm start &&
            run run --rm "$image" true &&
            run run --rm --volume "$share:/w" "$image" cat /w/note &&
            run run --rm --volume "$share:/w" "$image" \
                sh -c "dd if=/dev/zero of=/w/big bs=1M count=64 && cat /w/big >/dev/null" &&
            run run -d --name web "$image" sleep 60 &&
            run exec web true &&
            run rm -f web
    }
    local worked=$?
    # Its slot's address kept for the next boot only once this VM is
    # stopped: two VMs with one address clash on the NAT.
    if timeout 150 env -u SHIPYARD_VM SHIPYARD_VM_DIR="$dir" "$shipyard" vm stop \
        </dev/null >>"$log" 2>&1; then
        [ -f "$dir/network.json" ] && cp "$dir/network.json" "$card"
    else
        rm -f "$card"
    fi
    cp "$dir/console.log" "$out/console-$i.log" 2>/dev/null
    if grep -qE "Oops|Unable to handle|Internal error|BUG:|Kernel panic" "$out/console-$i.log" 2>/dev/null; then
        verdict=CRASH
    elif grep -q "failed to lookup address information" "$log" &&
        grep -q "udhcpc: lease of" "$out/console-$i.log" 2>/dev/null; then
        # An address, and still no name: the Mac's network, not counted.
        verdict=NET
    elif [ "$worked" -ne 0 ]; then
        verdict=HANG
    else
        verdict=ok
    fi
    echo "$verdict boot $i $(($(date +%s) - started))s"
    rm -rf "$dir" "$share"
}

i=0
while [ "$i" -lt "$n" ]; do
    batch=0 boots=()
    while [ "$batch" -lt "$p" ] && [ "$i" -lt "$n" ]; do
        i=$((i + 1))
        batch=$((batch + 1))
        boot "$i" >>"$out/verdicts" &
        boots+=($!)
    done
    # (These boots alone: caffeinate runs until this script ends.)
    wait "${boots[@]}"
done
sort -t' ' -k3n "$out/verdicts"
bad=$(grep -cE "^(CRASH|HANG) " "$out/verdicts")
net=$(grep -c "^NET " "$out/verdicts")
[ "$net" -gt 0 ] && echo "$net boots without the Mac's network (NET): not counted"
[ "$net" -lt "$n" ] || { echo "no boot counted" >&2; exit 1; }
python3 - "$bad" "$((n - net))" <<'EOF'
import math, sys
bad, n = int(sys.argv[1]), int(sys.argv[2])
z = 1.96
p = bad / n
centre = (p + z * z / (2 * n)) / (1 + z * z / n)
half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / (1 + z * z / n)
print(f"{bad}/{n} crashed or hung: {p:.0%} (95%: {max(0, centre - half):.0%} to {min(1, centre + half):.0%})")
EOF
echo "consoles and logs: $out"

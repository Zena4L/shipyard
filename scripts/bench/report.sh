#!/usr/bin/env bash
# FEATURES §16's performance targets, measured and reported (step 15.6a;
# reported, never blocking): a container's start (under 100 ms, the image
# already in the store), what runs while nothing does (no Shipyard process
# but each container's monitor), and a monitor's own memory (under 1 MiB,
# idle, shared libraries not counted). Prints Markdown, for the stage notes.
#
# usage: scripts/bench/report.sh [IMAGE]
#   IMAGE: one in the store, or pulled first (default alpine:3.20); the
#   `shipyard` on PATH, as the user running this (root's or rootless), on a
#   host where this user runs no other container (its processes are counted).
set -euo pipefail
image=${1:-alpine:3.20}
runs=20
prefix=bench-$$

shipyard pull --quiet "$image" >/dev/null 2>&1 || shipyard pull "$image" >/dev/null
# Every container this made, whatever stopped it.
cleanup() {
    shipyard ps -aq --filter "name=$prefix" 2>/dev/null | while read -r id; do
        shipyard rm -f "$id" >/dev/null 2>&1 || true
    done
}
trap cleanup EXIT

# Milliseconds a command took, its output dropped. A failure stops the
# whole report (from inside `$(…)` too): never the timing of a failure.
took() {
    local start end
    start=$(date +%s%N)
    "$@" >/dev/null 2>&1 || {
        echo "failed: $*" >&2
        kill "$$"
        exit 1
    }
    end=$(date +%s%N)
    echo $(((end - start) / 1000000))
}

# The median and the 90th percentile of the numbers on standard input.
summary() {
    sort -n | awk '{ v[NR] = $1 } END {
        m = (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2
        p = v[int(NR * 0.9 + 0.999)]
        printf "%s ms (90th percentile %s ms)", m, p
    }'
}

# `runs` timings of a command, after 3 to warm the caches. Called inside
# `$(…)`, where a failure wouldn't stop this script: `works` first.
timed() {
    for _ in 1 2 3; do "$@" >/dev/null 2>&1; done
    for _ in $(seq "$runs"); do took "$@"; done | summary
}

# The command once, out here: a failure stops the report (never a timing
# of a failure).
works() {
    "$@" >/dev/null 2>&1 || {
        echo "failed: $*" >&2
        "$@" >/dev/null
    }
}

whoami=$(id -un)
mode=root
[ "$(id -u)" -eq 0 ] || mode=rootless
echo "### $(uname -sr), $(uname -m), $mode ($whoami): $(shipyard --version)"
echo
works shipyard run --rm --network none "$image" true
works shipyard run --no-image --network none true
echo "| Measure | Result | Target |"
echo "|---|---|---|"
works shipyard run --rm "$image" true
echo "| \`run --rm --network none $image true\` | $(timed shipyard run --rm --network none "$image" true) | under 100 ms |"
echo "| \`run --rm $image true\` (the default network: $([ "$mode" = root ] && echo "a bridge and its DNS" || echo pasta)) | $(timed shipyard run --rm "$image" true) | under 100 ms |"
echo "| \`run --no-image --network none true\` | $(timed shipyard run --no-image --network none true) | (no image) |"
for i in $(seq "$runs"); do shipyard create --name "$prefix-$i" --network none "$image" true >/dev/null; done
works shipyard start "$prefix-1"
started=$(for i in $(seq 2 "$runs"); do took shipyard start "$prefix-$i"; done | summary)
for i in $(seq "$runs"); do shipyard rm -f "$prefix-$i" >/dev/null; done
echo "| \`start\` of a created container (detached) | $started | (its share) |"

# What runs: Shipyard's processes (its binary, by any name it takes: a
# monitor, a DNS server, an init) and pasta's, outside any container, with
# nothing running, then beside a detached container with no network and one
# on the default network.
ours() {
    local binary
    binary=$(readlink -f "$(command -v shipyard)")
    for exe in /proc/[0-9]*/exe; do
        [ "$(readlink "$exe" 2>/dev/null)" = "$binary" ] || continue
        pid=${exe#/proc/}
        pid=${pid%/exe}
        tr '\0' ' ' <"/proc/$pid/cmdline" | cut -c1-40
        echo
    done | grep -v '^$' | grep -v 'scripts/bench' || true
}
pastas() { pgrep -x pasta -u "$(id -u)" | wc -l; }
# What runs beside: monitors, other processes of Shipyard's (not an init,
# which is the container's), and pasta.
beside() {
    local listed
    listed=$(ours)
    printf '%s monitor, %s other, %s pasta' \
        "$(grep -c __shipyard-monitor <<<"$listed" || true)" \
        "$(grep -vc -e __shipyard-monitor -e shipyard-init -e '^$' <<<"$listed" || true)" \
        "$(pastas)"
}
echo "| Shipyard's processes, nothing running | $(ours | wc -l), $(pastas) pasta | none |"
shipyard run -d --name "$prefix-quiet" --network none "$image" sleep 600 >/dev/null
sleep 2
echo "| ... one detached container, \`--network none\` | $(beside) | its monitor alone |"
shipyard run -d --name "$prefix-networked" "$image" sleep 600 >/dev/null
sleep 2
echo "| ... and one on the default network | $(beside) | the monitors (and the network's own: DNS, or pasta) |"

# The monitor's own memory, the quiet container's (found by its ID, in its
# cgroup's path), idle: its anonymous pages and swap (heap, stacks: what it
# allocated), not the binary's text or a library's, which are file-backed
# (counted "private" only when no other process maps them); its RSS beside.
id=$(shipyard inspect -f '{{.Id}}' "$prefix-quiet")
monitor=
for pid in $(pgrep -f __shipyard-monitor -u "$(id -u)"); do
    grep -q "$id" "/proc/$pid/cgroup" 2>/dev/null && monitor=$pid
done
[ -n "$monitor" ] || { echo "no monitor found for $id" >&2; exit 1; }
own=$(awk '/^(Anonymous|Swap):/ { kb += $2 } END { print kb }' "/proc/$monitor/smaps_rollup")
rss=$(awk '/^VmRSS:/ { print $2 }' "/proc/$monitor/status")
echo "| A monitor's own memory, idle | $own KiB anonymous and swapped ($rss KiB RSS, the binary's text included) | under 1 MiB |"

#!/usr/bin/env bash
# The per-container monitor's memory (stage 8, step 8.1a): the baseline that
# step 8.3's budget is measured against (FEATURES: "under 1 MB each").
#
# Today's monitor is already what 8.3 extends: the whole `shipyard-runtime`
# binary, re-executed (as `shipyard-monitor`), holding a container until it
# ends. So this runs a container from a minimal bundle (`sleep`), with a debug
# and a release build, and reads the monitor's memory from /proc:
#
# - VmRSS (status): every resident page it maps, the binary's shared text
#   included;
# - from smaps_rollup: Pss (shared pages divided among their users),
#   Private_Clean and Private_Dirty (pages no other process maps), Rss;
# - from smaps: which mappings hold the private dirty pages.
#
# Run it in the test container, as root and as tester (rootless):
#
#   timeout 1800 .claude/skills/shipyard-dev/references/run-in-linux.sh root scripts/probes/monitor-memory.sh
#   timeout 1800 .claude/skills/shipyard-dev/references/run-in-linux.sh tester scripts/probes/monitor-memory.sh
#
# Everything it makes is under one mktemp directory, removed at the end.
set -euo pipefail
cd /src
work=$(mktemp -d)
# A failed step (set -e) mustn't leave a container and its monitor running.
started=()
clean_up() {
    for runtime in "${started[@]}"; do
        "$runtime" --root "$work/state-$(basename "$(dirname "$runtime")")" delete --force memory >/dev/null 2>&1 || true
    done
    rm -rf "$work"
}
trap clean_up EXIT

echo "kernel: $(uname -r) $(uname -m); as $(id -un)"
for profile in dev release; do
    cargo build --locked --quiet --profile "$profile" --bin shipyard-runtime
done
# A binary just written has dirty page-cache pages, which its mappings count
# as Private_Dirty until they're written back (1.7 MB of "private" memory
# that wasn't, on the first try).
sync

# A root filesystem with `sleep` and the libraries it loads.
rootfs=$work/rootfs
mkdir -p "$rootfs/bin" "$rootfs/tmp"
sleep_path=$(command -v sleep)
cp "$sleep_path" "$rootfs/bin/"
for lib in $(ldd "$sleep_path" | grep -o '/[^ ]*'); do
    mkdir -p "$rootfs$(dirname "$lib")"
    cp -L "$lib" "$rootfs$lib"
done
namespaces='{"type": "pid"}, {"type": "mount"}, {"type": "uts"}, {"type": "ipc"}'
id_maps=
if [ "$(id -u)" != 0 ]; then
    namespaces="$namespaces, {\"type\": \"user\"}"
    id_maps="\"uidMappings\": [{\"containerID\": 0, \"hostID\": $(id -u), \"size\": 1}],
             \"gidMappings\": [{\"containerID\": 0, \"hostID\": $(id -g), \"size\": 1}],"
fi
cat >"$work/config.json" <<EOF
{
  "ociVersion": "1.2.0",
  "root": {"path": "$rootfs"},
  "process": {
    "user": {"uid": 0, "gid": 0},
    "args": ["sleep", "120"],
    "env": ["PATH=/bin"],
    "cwd": "/",
    "capabilities": {"bounding": ["CAP_KILL"], "effective": ["CAP_KILL"], "permitted": ["CAP_KILL"]},
    "noNewPrivileges": true
  },
  "linux": {
    $id_maps
    "namespaces": [$namespaces]
  }
}
EOF

field() { awk -v key="$1:" '$1 == key { print $2 }' "$2"; }

# The binaries just built (run-in-linux.sh sets CARGO_TARGET_DIR=/target).
target=${CARGO_TARGET_DIR:-/src/target}
for build in debug release; do
    runtime=$target/$build/shipyard-runtime
    state=$work/state-$build
    started+=("$runtime")
    "$runtime" --root "$state" create --bundle "$work" memory
    "$runtime" --root "$state" start memory
    sleep 1
    init=$("$runtime" --root "$state" state memory | sed -n 's/.*"pid": *\([0-9]*\).*/\1/p')
    monitor=$(field PPid "/proc/$init/status")
    name=$(tr '\0' ' ' <"/proc/$monitor/cmdline" | cut -d' ' -f1)
    if [ "$name" != shipyard-monitor ]; then
        echo "the init's parent (PID $monitor) is $name, not the monitor" >&2
        exit 1
    fi
    rollup=/proc/$monitor/smaps_rollup
    printf '%-7s binary %6s KiB | monitor (%s, PID %s): VmRSS %5s KiB, Rss %5s, Pss %5s, Private_Clean %5s, Private_Dirty %5s KiB\n' \
        "$build" "$(($(stat -c %s "$runtime") / 1024))" "$name" "$monitor" \
        "$(field VmRSS "/proc/$monitor/status")" "$(field Rss "$rollup")" "$(field Pss "$rollup")" \
        "$(field Private_Clean "$rollup")" "$(field Private_Dirty "$rollup")"
    # Where its own pages are: the mappings with the most Private_Dirty.
    awk '/^[0-9a-f]+-[0-9a-f]+ / { name = (NF >= 6 ? $6 : "[anonymous]") }
         /^Private_Dirty:/ { dirty[name] += $2 }
         END { for (n in dirty) if (dirty[n] > 0) printf "          %5d KiB  %s\n", dirty[n], n }' \
        "/proc/$monitor/smaps" | sort -rn | head -6
    "$runtime" --root "$state" delete --force memory
done

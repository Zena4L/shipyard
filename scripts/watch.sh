#!/usr/bin/env bash
# The unattended checks: what CI would do while it's off (ROADMAP D31). Run
# nightly by launchd (scripts/watch/dev.shipyard.watch.plist), or by hand:
#
#   scripts/watch.sh            # tonight's run, now
#   scripts/watch.sh --quick    # the plumbing only: macOS checks and 20 s of fuzzing
#   scripts/watch.sh --install  # run it nightly at 02:00 (a launchd agent)
#   scripts/watch.sh --uninstall
#
# It never touches your working tree: it checks origin/main in a clone of its own
# ($SHIPYARD_WATCH_HOME, default ~/Library/Caches/shipyard-watch), with its own
# cargo target dir and its own Docker build volume (shipyard-watch-target), so it
# runs beside an interactive session. Each night:
#
#   1. verify  .claude/skills/shipyard-verify/scripts/verify.sh (everything), when
#              main moved since the last verified commit;
#   2. fuzz    every target for WATCH_FUZZ_SECONDS (600) each; the corpora grow
#              night after night in the watch volume;
#   3. mutants cargo-mutants on the lines changed since the last sweep.
#
# A report lands in runs/<date>/report.md (runs/latest). You're notified only
# when something got worse: a check that passed now fails, a fuzz target
# crashed, or the watch itself couldn't run. Silence means it ran and nothing
# regressed.
set -uo pipefail

# --install / --uninstall: the launchd agent that runs it every night at 02:00
# (a missed night runs on wake), from this checkout's copy of this script.
agent=dev.shipyard.watch
plist=$HOME/Library/LaunchAgents/$agent.plist
case "${1:-}" in
--install)
    here=$(cd "$(dirname "$0")" && pwd)
    logs=${SHIPYARD_WATCH_HOME:-$HOME/Library/Caches/shipyard-watch}
    mkdir -p "$logs" "$(dirname "$plist")"
    cat >"$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$agent</string>
  <key>ProgramArguments</key>
  <array><string>/bin/bash</string><string>$here/watch.sh</string></array>
  <key>StartCalendarInterval</key>
  <dict><key>Hour</key><integer>2</integer><key>Minute</key><integer>0</integer></dict>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key><string>$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    <key>HOME</key><string>$HOME</string>
  </dict>
  <key>StandardOutPath</key><string>$logs/launchd.log</string>
  <key>StandardErrorPath</key><string>$logs/launchd.log</string>
</dict>
</plist>
EOF
    launchctl bootout "gui/$(id -u)/$agent" 2>/dev/null || true
    launchctl bootstrap "gui/$(id -u)" "$plist" && echo "installed: $plist (nightly at 02:00)"
    exit
    ;;
--uninstall)
    launchctl bootout "gui/$(id -u)/$agent" 2>/dev/null || true
    rm -f "$plist" && echo "uninstalled: $agent (its clone, runs and the shipyard-watch-target volume are kept)"
    exit
    ;;
esac

# Stay awake until it's done (on battery too: the run is bounded). First, so
# the process that takes the lock below is the one that runs.
if [ -z "${WATCH_AWAKE:-}" ]; then
    WATCH_AWAKE=1 exec caffeinate -i "$0" "$@"
fi

home=${SHIPYARD_WATCH_HOME:-$HOME/Library/Caches/shipyard-watch}
origin=${SHIPYARD_WATCH_ORIGIN:-https://github.com/Zena4L/shipyard.git}
fuzz_seconds=${WATCH_FUZZ_SECONDS:-600}
quick=false
[ "${1:-}" = --quick ] && quick=true && fuzz_seconds=20

mkdir -p "$home/runs"
run_dir="$home/runs/$(date +%Y-%m-%d-%H%M)"
mkdir -p "$run_dir"
report="$run_dir/report.md"
state="$home/state"
touch "$state"

# A notification on the desktop (launchd runs this in your login session).
notify() {
    osascript -e "display notification \"$2\" with title \"Shipyard watch\" subtitle \"$1\"" \
        >/dev/null 2>&1 || true
    echo "NOTIFIED: $1: $2" >>"$report"
}
get() { grep "^$1=" "$state" | tail -1 | cut -d= -f2-; }
put() {
    grep -v "^$1=" "$state" >"$state.new" || true
    echo "$1=$2" >>"$state.new"
    mv "$state.new" "$state"
}

# One run at a time (mkdir is atomic); a lock whose process is gone is stale.
lock="$home/lock"
if ! mkdir "$lock" 2>/dev/null; then
    if kill -0 "$(cat "$lock/pid" 2>/dev/null)" 2>/dev/null; then
        echo "another watch run is going ($(cat "$lock/pid")): not starting" >&2
        exit 0
    fi
    rm -rf "$lock" && mkdir "$lock"
fi
echo $$ >"$lock/pid"
trap 'rm -rf "$lock"' EXIT

{
    echo "# Shipyard watch, $(date '+%Y-%m-%d %H:%M')"
    $quick && echo "(quick: the plumbing only)"
    echo
} >"$report"
ln -sfn "$run_dir" "$home/runs/latest"

# Can it run at all? A watch that silently didn't run is the worst outcome.
cant_run() {
    echo "**Couldn't run:** $1" >>"$report"
    notify "didn't run" "$1"
    exit 1
}
timeout 60 docker info >/dev/null 2>&1 || cant_run "Docker isn't running"
clone="$home/main"
if [ ! -d "$clone/.git" ]; then
    git clone --quiet "$origin" "$clone" >>"$report" 2>&1 || cant_run "couldn't clone $origin"
fi
git -C "$clone" fetch --quiet origin main >>"$report" 2>&1 || cant_run "couldn't fetch origin/main"
git -C "$clone" reset --quiet --hard origin/main && git -C "$clone" clean -qfdx
sha=$(git -C "$clone" rev-parse --short HEAD)
echo "Checked: \`main\` at \`$sha\` ($(git -C "$clone" log -1 --format=%s))" >>"$report"
echo >>"$report"

export CARGO_TARGET_DIR="$home/target"
export TARGET_VOLUME=shipyard-watch-target
cd "$clone"

# record TASK STATUS SUMMARY: into the report, and a notification if it got
# worse than last time (ok → failed), or on any crash.
record() {
    local task=$1 status=$2 summary=$3
    echo "- **$task:** $status — $summary" >>"$report"
    local was
    was=$(get "$task")
    if [ "$status" = failed ] && [ "$was" != failed ]; then
        notify "$task regressed" "$summary"
    fi
    put "$task" "$status"
}
totals() {
    grep -E '^test result' "$1" |
        awk '{p+=$4; f+=$6} END {printf "passed %d, failed %d", p, f}'
}

# 1. verify, when main moved (quick: the macOS half, always).
verify=.claude/skills/shipyard-verify/scripts/verify.sh
if $quick; then
    VERIFY_LOGS="$run_dir/verify" "$verify" --mac >"$run_dir/verify.log" 2>&1
    status=$?
    record verify-mac "$([ $status = 0 ] && echo ok || echo failed)" \
        "$(totals "$run_dir/verify/test-mac.log")"
elif [ "$(get verified-sha)" != "$sha" ]; then
    VERIFY_LOGS="$run_dir/verify" "$verify" >"$run_dir/verify.log" 2>&1
    status=$?
    summary="macOS $(totals "$run_dir/verify/test-mac.log"); Linux $(totals "$run_dir/verify/test-linux.log")"
    record verify "$([ $status = 0 ] && echo ok || echo failed)" "$summary"
    [ $status = 0 ] && put verified-sha "$sha"
else
    echo "- **verify:** skipped — \`main\` unchanged since the last verified run" >>"$report"
fi

# 2. fuzz, every night (a crash is always news: the input is kept).
timeout $((fuzz_seconds * 20 + 3600)) scripts/test-linux.sh --fuzz "$fuzz_seconds" \
    >"$run_dir/fuzz.log" 2>&1
if grep -q '^== crashed:' "$run_dir/fuzz.log"; then
    crashed=$(grep '^== crashed:' "$run_dir/fuzz.log" | sed 's/^== crashed: //')
    echo "- **fuzz:** CRASHED — $crashed" >>"$report"
    notify "fuzz crash" "$crashed"
    put fuzz failed
elif grep -q '^== no fuzz target crashed' "$run_dir/fuzz.log"; then
    record fuzz ok "every target, ${fuzz_seconds} s each"
else
    record fuzz failed "it didn't finish: see fuzz.log"
fi

# 3. mutants, on what changed since the last sweep (not in quick runs).
if ! $quick; then
    base=$(get mutated-sha)
    if [ -z "$base" ]; then
        put mutated-sha "$sha"
        echo "- **mutants:** first run — the sweep starts from \`$sha\`" >>"$report"
    elif [ "$base" = "$sha" ]; then
        echo "- **mutants:** skipped — nothing changed since the last sweep" >>"$report"
    else
        timeout 21600 scripts/test-linux.sh --mutants "$base" >"$run_dir/mutants.log" 2>&1
        missed=$(sed -n 's/^missed: *//p' "$run_dir/mutants.log" | tail -1)
        if [ -z "$missed" ]; then
            record mutants failed "cargo-mutants didn't run to its report: see mutants.log"
        else
            echo "- **mutants:** $missed missed since \`$base\` (mutants.log; one only a root test catches shows as missed: check it by hand)" >>"$report"
            [ "$missed" -gt 0 ] && notify "mutants missed" "$missed since $base"
            put mutants ok
            put mutated-sha "$sha"
        fi
    fi
fi

echo >>"$report"
echo "Logs: $run_dir" >>"$report"
cat "$report"

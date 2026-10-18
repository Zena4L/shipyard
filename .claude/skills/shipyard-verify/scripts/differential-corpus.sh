#!/usr/bin/env bash
# The differential run's corpus: runs one build of `shipyard` and
# `shipyard-runtime` through a fixed list of invocations and writes a
# transcript (each command, its stdout, its stderr, its exit code), normalised
# so that two builds' transcripts differ only where their behaviour does.
#
# usage: differential-corpus.sh BIN_DIR TRANSCRIPT
#
# BIN_DIR holds `shipyard` and `shipyard-runtime`. They're copied to one fixed
# place first, so both builds print the same program path. Every store lives
# under one fixed scratch root, wiped before and after, so both builds see the
# same fresh state. Run by differential.sh (once per build, one after the
# other), never two at once: root's runtime state (/run/shipyard) is shared.
set -uo pipefail

bin_dir=$1
transcript=$2

work=${DIFF_WORK:-${TMPDIR:-/tmp}/shipyard-differential}
case $work in
*shipyard-differential* | */target/differential/work-*) ;;
*) echo "refusing to wipe $work" >&2; exit 2 ;;
esac
rm -rf "$work"
mkdir -p "$work/bin" "$work/files"
cp "$bin_dir/shipyard" "$bin_dir/shipyard-runtime" "$work/bin/"
SY=$work/bin/shipyard
SYR=$work/bin/shipyard-runtime

export SHIPYARD_IMAGE_STORE=$work/images
export SHIPYARD_CONTAINER_DIR=$work/containers
export SHIPYARD_NETWORK_DIR=$work/networks
export SHIPYARD_EVENTS_DIR=$work/events
export NO_COLOR=1 COLUMNS=200
unset SHIPYARD_DNS_UPSTREAM SHIPYARD_DNS_IDLE
runtime_root=$work/runtime
# A login session's runtime directory, which a rootless caller needs.
if [ "$(id -u)" != 0 ]; then
    export XDG_RUNTIME_DIR=$work/xdg
    mkdir -m 700 "$XDG_RUNTIME_DIR"
fi

raw=$work/raw.txt
: >"$raw"

# One case: the label, then the command (run through bash -c, so pipes and
# redirections work). `$SY`, `$SYR`, `$W` (the files dir) and `$RR` (the
# runtime root) are available inside it.
case_() {
    local label=$1 command=$2 out err code
    out=$work/out err=$work/err
    SY=$SY SYR=$SYR W=$work/files RR=$runtime_root \
        timeout 120 bash -c "$command" >"$out" 2>"$err" </dev/null
    code=$?
    last_code=$code
    {
        printf '### %s\n$ %s\n' "$label" "$command"
        cat "$out"
        printf -- '--- stderr\n'
        cat "$err"
        printf -- '--- exit %s\n\n' "$code"
    } >>"$raw"
}

# A positive control on the case just run: it must have ended 0 and printed
# `want` (a regular expression). Else the corpus proves nothing past it (a pull
# that failed fails every later case the same way on both builds), so the run
# stops and says so: differential.sh reports a failed control, never
# "identical".
control() {
    local want=$1
    if [ "$last_code" != 0 ] || { [ -n "$want" ] && ! grep -qE "$want" "$work/out"; }; then
        echo "CONTROL FAILED after the last case: wanted exit 0 and /$want/" >>"$raw"
        cp "$raw" "$transcript"
        echo "differential-corpus: a control failed (see $transcript)" >&2
        exit 3
    fi
}

# Wait (up to 30 s) until a container's state, as `ps -a` prints it, matches.
wait_for() {
    local name=$1 pattern=$2
    for _ in $(seq 300); do
        # Whole reads (no `grep -q`): a reader gone early is a broken pipe
        # for `ps`, said on its stderr.
        "$SY" ps -a 2>/dev/null | grep -E "[[:space:]]$name\$" | grep -E "$pattern" >/dev/null && return 0
        sleep 0.1
    done
    echo "wait_for $name $pattern: gave up" >>"$raw"
}

# --- help and usage: every command's --help, and refusals before any state ---
case_ help '$SY --help'
for c in run create start attach stop restart update kill rename exec cp logs events \
    wait ps port inspect rm pull images rmi image volume network system doctor \
    "image ls" "image prune" "volume create" "volume ls" "volume inspect" "volume rm" \
    "volume prune" "network create" "network ls" "network inspect" "network rm" \
    "network prune" "network connect" "network disconnect" "system df" \
    "system recover" "system stop"; do
    case_ "help $c" "\$SY $c --help"
done
case_ runtime-help '$SYR --help'
for c in create logs attach exec cp start state kill pause resume ps events update delete; do
    case_ "runtime help $c" "\$SYR $c --help"
done
case_ no-command '$SY'
case_ bad-flag '$SY run --no-such-flag alpine'
case_ bad-subcommand '$SY frobnicate'
case_ bad-restart '$SY run -d --restart sometimes alpine true'
case_ bad-publish '$SY run -p 99999:80 alpine true'
case_ bad-memory '$SY run --memory lots alpine true'
case_ bad-ulimit '$SY run --ulimit nofile=2:1 alpine true'
case_ bad-user '$SY run -u "" alpine true'
case_ bad-workdir '$SY run -w relative alpine true'
case_ bad-name '$SY create --name "-bad" alpine true'
case_ bad-detach-keys '$SY start -a --detach-keys ctrl-zz nothing'
case_ bad-health '$SY run --health-interval 0.5us alpine true'
case_ bad-events-time '$SY events --since yesterday'
case_ bad-events-filter '$SY events --filter colour=red --until 0'
case_ pull-never-missing '$SY run --pull=never alpine:3.0-not-here true'
case_ rootfs-missing '$SY run --rootfs $W/no-such-dir /bin/true'
case_ missing-container-stop '$SY stop nothing-here'
case_ missing-container-rm '$SY rm nothing-here'
case_ missing-container-logs '$SY logs nothing-here'
case_ missing-container-inspect '$SY inspect nothing-here'
case_ missing-container-exec '$SY exec nothing-here true'
case_ missing-container-cp '$SY cp nothing-here:/etc/hostname $W/x'
case_ missing-container-wait '$SY wait nothing-here'
case_ missing-container-port '$SY port nothing-here'
case_ missing-container-rename '$SY rename nothing-here other'
case_ missing-container-update '$SY update --restart no nothing-here'
case_ missing-container-attach '$SY attach nothing-here'
case_ missing-image-rmi '$SY rmi nothing-here:1'
case_ empty-ps '$SY ps -a'
case_ empty-images '$SY images'
case_ empty-volumes '$SY volume ls'
case_ empty-networks '$SY network ls'
case_ empty-df '$SY system df'
case_ runtime-state-missing '$SYR --root $RR state nothing-here'
case_ runtime-delete-missing '$SYR --root $RR delete nothing-here'
case_ runtime-kill-missing '$SYR --root $RR kill nothing-here'

# --- no image: the host's files ---
case_ no-image-echo '$SY run --no-image -- /bin/echo hello'
control '^hello$'
case_ no-image-exit '$SY run --no-image -- /bin/sh -c "exit 7"'
case_ no-image-notfound '$SY run --no-image -- /no/such/command'
case_ no-image-notexec '$SY run --no-image -- /etc/hostname'
case_ no-isolation '$SY run --no-isolation -- /bin/sh -c "echo out; echo err >&2; exit 3"'

if [ "$(uname -s)" = Linux ]; then
    # --- images ---
    case_ pull '$SY pull alpine:3.20 >/dev/null; echo pulled $?'
    control '^pulled 0$'
    case_ images '$SY images'
    case_ image-ls '$SY image ls'

    # --- foreground runs ---
    case_ run-echo '$SY run --rm --name diff-run1 alpine:3.20 echo hello'
    control '^hello$'
    case_ run-exit '$SY run --rm --name diff-run2 alpine:3.20 sh -c "exit 42"'
    case_ run-127 '$SY run --rm --name diff-run3 alpine:3.20 no-such-command'
    case_ run-126 '$SY run --rm --name diff-run4 alpine:3.20 /etc/hostname'
    case_ run-env '$SY run --rm --name diff-run5 -e A=1 -e B alpine:3.20 sh -c "echo \$A \$B"'
    case_ run-user '$SY run --rm --name diff-run6 -u 1000:1000 alpine:3.20 id'
    case_ run-workdir '$SY run --rm --name diff-run7 -w /tmp alpine:3.20 pwd'
    case_ run-stdin '$SY run --rm --name diff-run8 -i alpine:3.20 cat <<<"from stdin"'
    case_ run-read-only '$SY run --rm --name diff-run9 --read-only alpine:3.20 touch /x'

    # --- a container's life by name ---
    case_ create '$SY create --name diff-a alpine:3.20 sh -c "echo started; sleep 300" >/dev/null; echo $?'
    case_ ps-created '$SY ps -a --format "{{.Names}} {{.Status}}" 2>&1 || $SY ps -a'
    case_ inspect-created '$SY inspect diff-a'
    case_ start '$SY start diff-a'
    control '^diff-a$'
    wait_for diff-a Up
    case_ logs '$SY logs diff-a'
    case_ exec '$SY exec diff-a sh -c "echo inside; exit 5"'
    case_ exec-missing '$SY exec diff-a no-such-command'
    case_ cp-out '$SY cp diff-a:/etc/alpine-release - | tar -t'
    case_ cp-in 'echo hi >$W/hi.txt && $SY cp $W/hi.txt diff-a:/tmp/hi.txt && $SY exec diff-a cat /tmp/hi.txt'
    case_ cp-missing '$SY cp diff-a:/no/such/file $W/'
    case_ port '$SY port diff-a'
    case_ rename '$SY rename diff-a diff-b'
    case_ rename-taken '$SY create --name diff-c alpine:3.20 true >/dev/null; $SY rename diff-b diff-c'
    case_ update '$SY update --restart on-failure:2 diff-b'
    case_ inspect-running '$SY inspect diff-b'
    case_ rm-running '$SY rm diff-b'
    case_ stop '$SY stop -t 1 diff-b'
    case_ wait '$SY wait diff-b'
    case_ inspect-stopped '$SY inspect diff-b'
    case_ cp-stopped '$SY cp diff-b:/tmp/hi.txt -  | tar -xO'
    case_ restart '$SY restart -t 1 diff-b'
    wait_for diff-b Up
    case_ kill '$SY kill diff-b'
    wait_for diff-b Exited
    case_ kill-stopped '$SY kill diff-b'
    case_ start-attached '$SY start -a diff-c; echo code $?'
    case_ detached '$SY run -d --name diff-d alpine:3.20 sh -c "echo bye; exit 3" >/dev/null; $SY wait diff-d'
    case_ logs-ended '$SY logs diff-d'
    case_ restart-policy '$SY run -d --name diff-e --restart on-failure:2 alpine:3.20 sh -c "exit 1" >/dev/null; for _ in $(seq 300); do $SY inspect diff-e | grep "\"restart_count\": 2" >/dev/null && $SY ps -a | grep -E "diff-e\$" | grep Exited >/dev/null && break; sleep 0.1; done; $SY inspect diff-e | grep -E "\"(restart_count|restarts|status)\"" || true'
    case_ health '$SY run -d --name diff-f --health-cmd "true" --health-interval 1s alpine:3.20 sleep 300 >/dev/null; for _ in $(seq 300); do $SY ps -a | grep -E "diff-f\$" | grep "(healthy)" >/dev/null && break; sleep 0.1; done; $SY ps -a'
    case_ ps-all '$SY ps -a'
    case_ ps '$SY ps'
    case_ records 'for n in diff-b diff-c diff-d diff-e diff-f; do grep -l "\"name\": \"$n\"" $SHIPYARD_CONTAINER_DIR/*/record.json | xargs cat; echo; done'
    case_ events '$SY events --since 1h --until 0s | cut -d" " -f2,3,5- | sort'
    case_ events-json '$SY events --since 1h --until 0s --format json | sed -E "s/\"(time|id)\":(\"[0-9a-f]+\"|[0-9]+),//g" | sort'
    case_ events-filter '$SY events --since 1h --until 0s --filter event=die | cut -d" " -f2,3,5- | sort'
    case_ df '$SY system df'

    # --- volumes and networks ---
    case_ volume-create '$SY volume create diff-vol'
    case_ volume-use '$SY run --rm --name diff-run10 --volume diff-vol:/data alpine:3.20 sh -c "echo kept >/data/f"; $SY run --rm --name diff-run11 --volume diff-vol:/data alpine:3.20 cat /data/f'
    case_ volume-inspect '$SY volume inspect diff-vol'
    case_ volume-ls '$SY volume ls'
    case_ volume-rm-used '$SY create --name diff-v --volume diff-vol:/data alpine:3.20 true >/dev/null; $SY volume rm diff-vol'
    case_ network-create '$SY network create diff-net'
    case_ network-ls '$SY network ls'
    case_ network-inspect '$SY network inspect diff-net'
    case_ network-connect '$SY network connect diff-net diff-b'
    case_ network-disconnect '$SY network disconnect diff-net diff-b'

    # --- the runtime binary, by runtime ID, on a bundle made from the image ---
    case_ bundle 'mkdir -p $W/bundle/rootfs && $SY cp diff-c:/ - | tar -x -C $W/bundle/rootfs 2>/dev/null; ls $W/bundle/rootfs | head -3'
    cat >"$work/files/bundle/config.json" <<'EOF'
{"ociVersion":"1.2.0","process":{"args":["sh","-c","echo from-bundle; sleep 300"],"cwd":"/","env":["PATH=/usr/bin:/bin"],"user":{"uid":0,"gid":0}},"root":{"path":"rootfs"},"hostname":"diff","mounts":[{"destination":"/proc","type":"proc","source":"proc"}],"linux":{"namespaces":[{"type":"pid"},{"type":"mount"},{"type":"uts"},{"type":"ipc"}]}}
EOF
    case_ runtime-create '$SYR --root $RR create --bundle $W/bundle --log-dir $W/logs diff-r'
    control ''
    case_ runtime-state-created '$SYR --root $RR state diff-r'
    case_ runtime-start '$SYR --root $RR start diff-r'
    case_ runtime-logs 'sleep 1; $SYR --root $RR logs diff-r'
    case_ runtime-cp '$SYR --root $RR cp diff-r:/etc/hostname - | tar -xO'
    case_ runtime-kill '$SYR --root $RR kill diff-r KILL'
    case_ runtime-delete 'sleep 1; $SYR --root $RR delete diff-r'

    # --- removal ---
    case_ rm-force '$SY rm -f diff-b diff-c diff-d diff-e diff-f diff-v'
    case_ network-rm '$SY network rm diff-net'
    case_ volume-rm '$SY volume rm diff-vol'
    case_ volume-prune '$SY volume prune -f'
    case_ network-prune '$SY network prune -f'
    case_ rmi '$SY rmi alpine:3.20'
    case_ ps-after '$SY ps -a'
    case_ doctor '$SY doctor'
fi

# Leave nothing running for the next build.
"$SY" rm -f $("$SY" ps -aq 2>/dev/null) >/dev/null 2>&1 || true
"$SYR" --root "$runtime_root" delete --force diff-r >/dev/null 2>&1 || true

# Normalise what legitimately differs from run to run (perl: its \b is a
# word boundary on macOS too, where BSD sed's isn't). A table's column widths
# follow its widest cell ("Up 9 seconds" against "Up 10 seconds"), so runs of
# spaces are folded.
WORK=$work perl -pe '
    s/\Q$ENV{WORK}\E/<WORK>/g;
    s/[0-9a-f]{64}/<ID>/g;
    s/sha256:<ID>/sha256:<DIGEST>/g;
    s/\b[0-9a-f]{12}\b/<SHORT>/g;
    s/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+(Z|[+-][0-9:]+)?/<TIME>/g;
    s/\b(Up|Exited \([0-9-]+\)|Created|Restarting \([0-9-]+\)) [^ ]+ (seconds|second|minutes|minute|hours|hour) ago/$1 <AGO>/g;
    s/\b(Up) (About a minute|[0-9]+ (seconds|second|minutes|minute))/$1 <FOR>/g;
    s/[0-9]+ (seconds|second|minutes|minute) ago/<AGO>/g;
    s/"(secs_since_epoch|nanos_since_epoch)": [0-9]+/"$1": <N>/g;
    s/"Created": [0-9]+/"Created": <N>/g;
    s/"(pid|started_at|finished_at|time|timeNano|started|finished|boot|boot_id|serial)": ?[^,}]+/"$1": <N>/g;
    s/(pid|PID) [0-9]+/$1 <N>/g;
    s/[0-9.]+ ?(B|kB|KB|MB|GB|KiB|MiB|GiB)\b/<SIZE>/g;
    s/in [0-9.]+(ms|s)\b/in <DUR>/g;
    s/ {2,}/  /g;
' "$raw" >"$transcript"
rm -rf "$work"

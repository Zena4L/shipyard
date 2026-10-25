#!/usr/bin/env bash
# The differential run: proves a change kept behaviour by running the same
# corpus of invocations (differential-corpus.sh) through a baseline build and
# the working tree's build, and diffing what each printed and returned.
#
# usage: differential.sh [--base REF | --base-dir DIR] mac|tester|root...
#
#   REF     the baseline (default v0.2.0), exported with `git archive` (no
#           worktree, nothing in .git changes)
#   DIR     a baseline tree on disk instead (an uncommitted tree copied aside
#           before a change), copied in fresh each run
#   mac     both builds on this Mac (most container commands are refused here:
#           the refusals are compared)
#   tester  both builds in the Linux test container, as tester (rootless)
#   root    the same, as root
#
# Output lands in target/differential/: one transcript per build and place,
# and one .diff per place. Exits non-zero if any place differs. A Linux run
# builds both trees in the container (the old one in its own target dir) and
# takes a few minutes; wrap it in `timeout` and run it in the background.
set -euo pipefail

base=v0.2.0
base_dir=
case ${1:-} in
--base | --base-dir) [ $# -ge 2 ] || { sed -n '5,15p' "$0"; exit 2; } ;;
esac
if [ "${1:-}" = --base ]; then
    base=$2
    shift 2
elif [ "${1:-}" = --base-dir ]; then
    base_dir=$(cd "$2" && pwd)
    shift 2
fi
[ $# -gt 0 ] || { sed -n '5,15p' "$0"; exit 2; }

here=$(cd "$(dirname "$0")" && pwd)
repo=$(git -C "$here" rev-parse --show-toplevel)
out=$repo/target/differential
mkdir -p "$out"

if [ -n "$base_dir" ]; then
    # A tree on disk: copied fresh (it may have changed since the last run).
    rm -rf "$out/old"
    mkdir -p "$out/old"
    (cd "$base_dir" && tar -c --exclude ./target --exclude ./fuzz/target .) | tar -x -C "$out/old"
    echo "dir:$base_dir" >"$out/old.ref"
else
    base_commit=$(git -C "$here" rev-parse --verify --quiet "$base^{commit}") || {
        echo "differential: no such commit: $base" >&2
        exit 2
    }
    # The baseline's source, exported fresh unless it's already this REF.
    if [ "$(cat "$out/old.ref" 2>/dev/null)" != "$base_commit" ]; then
        rm -rf "$out/old"
        mkdir -p "$out/old"
        git -C "$repo" archive "$base_commit" | tar -x -C "$out/old"
        echo "$base_commit" >"$out/old.ref"
    fi
fi
cp "$here/differential-corpus.sh" "$out/"

status=0
for place in "$@"; do
    case $place in
    mac)
        (cd "$out/old" && CARGO_TARGET_DIR=$out/old-target cargo build --locked -q \
            -p shipyard-cli --bins)
        (cd "$repo" && cargo build --locked -q -p shipyard-cli --bins)
        for build in old new; do
            dir=$out/old-target/debug
            [ $build = new ] && dir=$repo/target/debug
            # (On the Mac itself, as v0.2.0 ran: not in Shipyard's VM.)
            SHIPYARD_VM=off DIFF_WORK=$out/work-mac bash "$out/differential-corpus.sh" "$dir" "$out/mac.$build.txt" || true
        done
        ;;
    tester | root)
        # Each build in a fresh container of its own: root's networks and
        # runtime state are the host's, so a first build's would steer the
        # second's. /scratch is $out (read-only), /src the working tree.
        cat >"$out/inside-old.sh" <<'EOF'
set -euo pipefail
(cd /scratch/old && CARGO_TARGET_DIR=/target/differential-old cargo build --locked -q -p shipyard-cli --bins)
# A failed control still leaves its transcript (it names the case).
bash /scratch/differential-corpus.sh /target/differential-old/debug /tmp/t.txt || true
echo '@@@ transcript'; cat /tmp/t.txt
EOF
        cat >"$out/inside-new.sh" <<'EOF'
set -euo pipefail
(cd /src && CARGO_TARGET_DIR=/target/differential-new cargo build --locked -q -p shipyard-cli --bins)
# A failed control still leaves its transcript (it names the case).
bash /scratch/differential-corpus.sh /target/differential-new/debug /tmp/t.txt || true
echo '@@@ transcript'; cat /tmp/t.txt
EOF
        failed=
        for build in old new; do
            timeout 3600 "$here/run-in-linux.sh" "$place" "$out/inside-$build.sh" \
                >"$out/$place.$build.raw" 2>"$out/$place.$build.log" || {
                echo "$place: the $build build's container run failed (see $out/$place.$build.log)" >&2
                failed=1
                break
            }
            sed -n '/^@@@ transcript$/,$p' "$out/$place.$build.raw" | sed '1d' >"$out/$place.$build.txt"
        done
        if [ -n "$failed" ]; then
            status=1
            continue
        fi
        ;;
    *)
        echo "unknown place: $place" >&2
        exit 2
        ;;
    esac
    if grep -q '^CONTROL FAILED' "$out/$place.old.txt" "$out/$place.new.txt"; then
        echo "$place: a control failed, nothing proven: $(grep -h '^CONTROL FAILED' "$out/$place.old.txt" "$out/$place.new.txt" | head -1)"
        status=1
        continue
    fi
    if diff -a -u "$out/$place.old.txt" "$out/$place.new.txt" >"$out/$place.diff"; then
        echo "$place: identical ($(grep -c '^### ' "$out/$place.new.txt") cases, $(grep -c '^--- exit 0$' "$out/$place.new.txt") of them exit 0)"
    else
        echo "$place: DIFFERS ($(grep -c '^[-+][^-+]' "$out/$place.diff") lines): $out/$place.diff"
        status=1
    fi
done
exit $status

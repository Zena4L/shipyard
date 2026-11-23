#!/usr/bin/env bash
# Whether two builds of a release made the same bytes (step 15.5a): every
# file in FIRST is in AGAIN, and the same. A tarball that differs is opened
# and its members compared, so the difference is named.
#
# usage: scripts/release/compare.sh FIRST AGAIN
set -euo pipefail
first=${1:?usage: scripts/release/compare.sh FIRST AGAIN}
again=${2:?usage: scripts/release/compare.sh FIRST AGAIN}
differ=0
for file in "$first"/*; do
    name=$(basename "$file")
    if [ ! -f "$again/$name" ]; then
        echo "$name: not built again" >&2
        differ=1
    elif cmp -s "$file" "$again/$name"; then
        echo "same: $name ($(sha256sum "$file" | cut -c1-16)…)"
    else
        echo "DIFFERENT: $name" >&2
        differ=1
        case $name in
        *.tar.gz)
            work=$(mktemp -d)
            mkdir "$work/first" "$work/again"
            tar -xzf "$file" -C "$work/first"
            tar -xzf "$again/$name" -C "$work/again"
            (cd "$work/first" && find . -type f -exec sha256sum {} + | sort -k2) >"$work/first.sums"
            (cd "$work/again" && find . -type f -exec sha256sum {} + | sort -k2) >"$work/again.sums"
            if diff "$work/first.sums" "$work/again.sums" >&2; then
                echo "(its members are the same: the archive itself differs)" >&2
            fi
            rm -rf "$work"
            ;;
        esac
    fi
done
exit "$differ"

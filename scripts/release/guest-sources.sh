#!/usr/bin/env bash
# The guest's sources, for a release (step 15.6b3b): every package in the
# guest (its SOURCES: name, version, licence, aports origin and commit), its
# Alpine build recipe (the APKBUILD and its patches, from Alpine's aports at
# that commit) and the upstream sources it names, each checked against the
# APKBUILD's sha512sums by Alpine's own `abuild` (fetched from Alpine's
# distfiles mirror first, which keeps what Alpine built from, else from its
# URL). Packed reproducibly as OUT/shipyard-VERSION-guest-sources.tar, one
# directory per origin. The kernel, busybox and e2fsprogs are GPL: what's
# published with the binaries is their complete source.
#
# usage: scripts/release/guest-sources.sh OUT GUEST-TARBALL [VERSION]
#   GUEST-TARBALL: scripts/release/guest.sh's (its SOURCES is read);
#   VERSION as linux.sh's.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
usage="usage: scripts/release/guest-sources.sh OUT GUEST-TARBALL [VERSION]"
out=${1:?$usage}
guest=${2:?$usage}
mkdir -p "$out"
out=$(cd "$out" && pwd)
. "$repo/scripts/release/build.env"
. "$repo/scripts/guest/alpine.lock"
commit=$(git -C "$repo" rev-parse HEAD)
version=${3:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)-${commit:0:12}}
epoch=$(git -C "$repo" log -1 --format=%ct)

work=$(mktemp -d)
# (What abuild fetched is root's: removed from a container too.)
trap 'docker run --rm --volume "$work:/work" "$ALPINE_IMAGE" rm -rf /work/sources >/dev/null 2>&1; rm -rf "$work"' EXIT
# (Whole: macOS's tar has no --wildcards.)
mkdir "$work/guest"
tar -xzf "$guest" -C "$work/guest"
name=shipyard-$version-guest-sources
mkdir -p "$work/sources/$name"
cp "$work"/guest/*/SOURCES "$work/sources/$name/SOURCES"

# Each origin once (a source package can make several of the guest's).
# (Alpine's distfiles for the guest's branch, alpine.lock's.)
timeout 3600 docker run --rm --volume "$work/sources/$name:/out" \
    --env DISTFILES_MIRROR="https://distfiles.alpinelinux.org/distfiles/$ALPINE_BRANCH" \
    "$ALPINE_IMAGE" sh -euc '
    apk add --quiet --no-cache abuild curl git
    grep -v "^#" /out/SOURCES | cut -f4,5 | sort -u | while IFS="	" read -r origin commit; do
        echo "== $origin ($commit)"
        # The aports of Alpine at that commit, from their mirror on GitHub
        # (the GitLab of Alpine refuses CI runners: 418). The commit is the
        # one the package says it was built from, and git checks every
        # object against it: the host does not matter. Only the files of
        # that directory are fetched.
        rm -rf /tmp/aports
        git init -q /tmp/aports
        git -C /tmp/aports remote add origin https://github.com/alpinelinux/aports.git
        for try in 1 2 3; do
            git -C /tmp/aports fetch -q --depth 1 --filter=blob:none origin "$commit" && break
            [ "$try" = 3 ] && { echo "aports $commit not fetched" >&2; exit 1; }
            sleep 10
        done
        got=
        for branch in main community; do
            if git -C /tmp/aports checkout -q FETCH_HEAD -- "$branch/$origin" 2>/dev/null &&
                [ -f "/tmp/aports/$branch/$origin/APKBUILD" ]; then
                mv "/tmp/aports/$branch/$origin" "/out/$origin"
                got=$branch
                break
            fi
        done
        [ -n "$got" ] || { echo "no $origin at aports $commit" >&2; exit 1; }
        # Fetched beside its recipe, and every file checked against it.
        (cd "/out/$origin" && SRCDEST="/out/$origin" abuild -F fetch verify)
        # (abuild links each download into a src/ of its own: not shipped.)
        rm -rf "/out/$origin/src"
    done'

# Packed in the pinned build image: the same tar wherever it's built.
timeout 600 docker run --rm --volume "$work/sources:/sources:ro" --volume "$out:/out" \
    --env NAME="$name" --env EPOCH="$epoch" "$BUILD_IMAGE" bash -c '
        set -euo pipefail
        tar --create --directory=/sources --sort=name --mtime="@$EPOCH" \
            --owner=0 --group=0 --numeric-owner --mode="u=rwX,go=rX" \
            --format=gnu "$NAME" >"/out/$NAME.tar"'
(cd "$out" && ls -la "$name.tar" && shasum -a 256 "$name.tar")

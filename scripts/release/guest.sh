#!/usr/bin/env bash
# Builds the Mac's guest for a release (step 15.5a): scripts/guest/build.sh
# in the pinned image (scripts/release/build.env), carrying the release's own
# arm64 `shipyard` (BINARY, from linux.sh's tarball), packed reproducibly as
# OUT/shipyard-VERSION-guest-arm64.tar.gz (its kernel, initramfs and
# manifest), with its packages' list (scripts/guest/alpine.installed) beside
# it as OUT/shipyard-VERSION-guest-packages.txt.
#
# usage: scripts/release/guest.sh OUT BINARY [VERSION]   (VERSION as linux.sh's)
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
out=${1:?usage: scripts/release/guest.sh OUT BINARY [VERSION]}
binary=${2:?usage: scripts/release/guest.sh OUT BINARY [VERSION]}
mkdir -p "$out"
out=$(cd "$out" && pwd)
. "$repo/scripts/release/build.env"
if [ -n "$(git -C "$repo" status --porcelain)" ]; then
    echo "the tree has changes: a release is built from a commit" >&2
    exit 1
fi
commit=$(git -C "$repo" rev-parse HEAD)
version=${3:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)-${commit:0:12}}
epoch=$(git -C "$repo" log -1 --format=%ct)

built=$(mktemp -d)
trap 'rm -rf "$built"' EXIT
IMAGE=$BUILD_IMAGE SHIPYARD_GUEST_BINARY=$binary REVISION=${commit:0:12} \
    "$repo/scripts/guest/build.sh" "$built/guest"

name=shipyard-$version-guest-arm64
mkdir "$built/$name"
cp "$built/guest/Image" "$built/guest/initramfs.cpio.gz" "$built/guest/manifest.json" "$built/$name/"
# Packed in the pinned image too: the same tar and gzip wherever it's built.
timeout 600 docker run --rm --volume "$built:/built:ro" --volume "$out:/out" \
    --env NAME="$name" --env EPOCH="$epoch" "$BUILD_IMAGE" bash -c '
        set -euo pipefail
        tar --create --directory=/built --sort=name --mtime="@$EPOCH" \
            --owner=0 --group=0 --numeric-owner --mode="u=rwX,go=rX" \
            --format=gnu "$NAME" | gzip -n -9 >"/out/$NAME.tar.gz"'
cp "$repo/scripts/guest/alpine.installed" "$out/shipyard-$version-guest-packages.txt"
(cd "$out" && shasum -a 256 "$name.tar.gz")

#!/usr/bin/env bash
# Builds a Linux release (step 15.5a): `shipyard` and `shipyard-runtime`,
# static (musl), for this machine's architecture, reproducibly, packed as
# OUT/shipyard-VERSION-linux-ARCH.tar.gz. Built in the pinned image
# (scripts/release/build.env) at fixed paths, so the same commit gives the
# same bytes on any machine with Docker: the release workflow builds it
# twice, on two runners, and compares.
#
# usage: NOTICES=DIR scripts/release/linux.sh OUT [VERSION]
#   NOTICES: the release's notices (scripts/release/notices.sh's OUT): its
#   THIRD-PARTY-LICENSES.html is packed beside LICENSE (step 15.6b3b).
#   VERSION: the tag's (`1.0.0`); else the workspace's and the commit's
#   (`0.5.0-276b34b1c0de`), for a dry run.
#   The tree must be clean: a release is a commit, never a working copy.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
out=${1:?usage: NOTICES=DIR scripts/release/linux.sh OUT [VERSION]}
notices=${NOTICES:?NOTICES: the notices of the release, scripts/release/notices.sh}/THIRD-PARTY-LICENSES.html
[ -f "$notices" ] || { echo "no notices at $notices" >&2; exit 1; }
notices=$(cd "$(dirname "$notices")" && pwd)/$(basename "$notices")
mkdir -p "$out"
out=$(cd "$out" && pwd)
. "$repo/scripts/release/build.env"

# One image: the test image's base is this one (F186).
from=$(sed -n 's/^FROM //p' "$repo/scripts/test-image/Dockerfile")
if [ "$from" != "$BUILD_IMAGE" ]; then
    echo "scripts/test-image/Dockerfile's FROM ($from) isn't BUILD_IMAGE ($BUILD_IMAGE)" >&2
    exit 1
fi
if [ -n "$(git -C "$repo" status --porcelain)" ]; then
    echo "the tree has changes: a release is built from a commit" >&2
    exit 1
fi
commit=$(git -C "$repo" rev-parse HEAD)
version=${2:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)-${commit:0:12}}
case $(uname -m) in
x86_64 | amd64) arch=x86_64 ;;
aarch64 | arm64) arch=aarch64 ;;
*)
    echo "no release for $(uname -m)" >&2
    exit 1
    ;;
esac

# The commit, exported clean (no .git, no ignored files): what's built is
# what's committed, at /build/shipyard in every build.
src=$(mktemp -d)
trap 'rm -rf "$src"' EXIT
git -C "$repo" archive --format=tar "$commit" | tar -x -C "$src"

timeout 3600 docker run --rm \
    --volume "$src:/build/shipyard:ro" --volume "$out:/out" \
    --volume "$notices:/notices/THIRD-PARTY-LICENSES.html:ro" \
    --env SOURCE_DATE_EPOCH="$(git -C "$repo" log -1 --format=%ct "$commit")" \
    --env SHIPYARD_REVISION_GIVEN="${commit:0:12}" \
    --env VERSION="$version" --env ARCH="$arch" --env MUSL_TOOLS="$MUSL_TOOLS" \
    --env DEBIAN_SNAPSHOT="$DEBIAN_SNAPSHOT" \
    --workdir /build/shipyard "$BUILD_IMAGE" bash scripts/release/linux-inside.sh

#!/usr/bin/env bash
# The Linux packages (step 15.5b): a .deb and an .rpm for each of a release's
# Linux tarballs (scripts/release/linux.sh's), into OUT, so a package holds
# the very bytes that were reproduced. Made by nfpm (pinned in build.env, by
# version and checksum) in the pinned image, every time in them the commit's
# (SOURCE_DATE_EPOCH): built twice, they're the same.
#
# usage: scripts/release/packages.sh OUT LINUX-TARBALL…
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
usage="usage: scripts/release/packages.sh OUT LINUX-TARBALL…"
out=${1:?$usage}
shift
[ "$#" -gt 0 ] || { echo "$usage" >&2; exit 2; }
mkdir -p "$out"
out=$(cd "$out" && pwd)
. "$repo/scripts/release/build.env"
epoch=$(git -C "$repo" log -1 --format=%ct)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
for tarball in "$@"; do
    name=$(basename "$tarball" .tar.gz)
    # shipyard-VERSION-linux-ARCH
    arch=${name##*-linux-}
    version=${name#shipyard-}
    version=${version%-linux-*}
    case $arch in
    x86_64) package_arch=amd64 ;;
    aarch64) package_arch=arm64 ;;
    *)
        echo "$tarball: no package for $arch" >&2
        exit 1
        ;;
    esac
    stage=$work/$name
    mkdir -p "$stage"
    tar -xzf "$tarball" -C "$work"
    # The units, root's and a user's, naming /usr/bin where a package puts
    # the binary (their README is a manual install's).
    mkdir -p "$stage/systemd/system" "$stage/systemd/user"
    for unit in "$stage"/contrib/systemd/*.service "$stage"/contrib/systemd/*.socket; do
        sed 's|/usr/local/bin/|/usr/bin/|g' "$unit" >"$stage/systemd/system/$(basename "$unit")"
    done
    for unit in "$stage"/contrib/systemd/user/*; do
        sed 's|/usr/local/bin/|/usr/bin/|g' "$unit" >"$stage/systemd/user/$(basename "$unit")"
    done
    if grep -rq /usr/local/bin "$stage/systemd"; then
        echo "a unit still names /usr/local/bin" >&2
        exit 1
    fi
    printf '%s %s %s\n' "$stage" "$package_arch" "$version" >>"$work/list"
done
cp "$repo/scripts/release/package/"* "$work/"

cp "$repo/scripts/release/packages-inside.sh" "$work/"
# A fixed hostname: an .rpm's header records the host it was built on.
timeout 1200 docker run --rm --hostname shipyard-release \
    --volume "$work:/work" --volume "$out:/out" \
    --env SOURCE_DATE_EPOCH="$epoch" --env NFPM_VERSION="$NFPM_VERSION" \
    --env NFPM_SHA256_X86_64="$NFPM_SHA256_X86_64" --env NFPM_SHA256_ARM64="$NFPM_SHA256_ARM64" \
    "$BUILD_IMAGE" bash /work/packages-inside.sh
(cd "$out" && sha256sum -- *.deb *.rpm)

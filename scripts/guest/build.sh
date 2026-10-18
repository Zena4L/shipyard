#!/usr/bin/env bash
# Builds the macOS guest (step 10.3a, D29, D10.3-2): Alpine's linux-virt
# kernel (raw, as Virtualization.framework boots it) and an initramfs root
# holding Alpine's minirootfs, the kernel's modules, e2fsprogs and a static
# `shipyard`, with a manifest. Runs in Shipyard's Linux test container (its
# image: scripts/test-image/Dockerfile); writes only files, into OUT.
#
# usage: scripts/guest/build.sh [OUT]   (default: target/guest)
set -euo pipefail
repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
out=${1:-$repo/target/guest}
mkdir -p "$out/cache"
out=$(cd "$out" && pwd)
hash=$(shasum -a 256 "$repo/scripts/test-image/Dockerfile")
image="${IMAGE:-shipyard-test:${hash:0:12}}"
revision=$(git -C "$repo" rev-parse --short=12 HEAD)
# (Untracked files count: new code is code the binary is built from.)
dirty=$(git -C "$repo" status --porcelain | head -c1)
[ -n "$dirty" ] && revision="$revision-dirty"
exec timeout 3600 docker run --rm --privileged \
    --volume "$repo:/src:ro" --volume "$out:/out" \
    --volume "${TARGET_VOLUME:-shipyard-linux-target}:/target" \
    --volume shipyard-linux-cargo:/usr/local/cargo \
    --volume shipyard-linux-rustup:/usr/local/rustup \
    --env CARGO_TARGET_DIR=/target --env REVISION="$revision" \
    --workdir /src "$image" bash /src/scripts/guest/build-inside.sh

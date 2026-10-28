#!/usr/bin/env bash
# The desktop app on Linux (step 14.7, D14.7-2): builds `shipyard` and the
# app in the desktop image (WebKitGTK, Xvfb, tauri-driver: its Dockerfile is
# scripts/desktop-image/Dockerfile), then, as `tester` (rootless, with a
# delegated cgroup and nothing serving its API), opens the app under a
# virtual display and drives it with Tauri's WebDriver
# (desktop/ui/tests-linux/drive.mjs): the app starts the API's service
# itself, draws the user's containers, and every section opens. Its
# screenshots land in target/desktop-linux/. Wrap the call in `timeout`.
#
# usage: scripts/desktop-linux.sh [--shell | --build-only]
#   --build-only: the two binaries built and left in target/desktop-linux/bin/
#   (the desktop VM's: scripts/macos/desktop-vm.sh), nothing driven.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
hash=$(shasum -a 256 "$repo/scripts/desktop-image/Dockerfile")
image="${IMAGE:-shipyard-desktop:${hash:0:12}}"
if ! docker image inspect "$image" >/dev/null 2>&1; then
    echo "building $image (once after each change to its Dockerfile)" >&2
    timeout 3000 docker build --tag "$image" - < "$repo/scripts/desktop-image/Dockerfile" >&2
fi
out="$repo/target/desktop-linux"
rm -rf "$out"
mkdir -p "$out"
run=(bash /src/scripts/desktop-linux/inside.sh)
tty=()
case "${1:-}" in
    "") ;;
    --shell) run=(bash); tty=(-it) ;;
    --build-only) run=(env BUILD_ONLY=1 bash /src/scripts/desktop-linux/inside.sh) ;;
    *) echo "usage: $0 [--shell | --build-only]" >&2; exit 2 ;;
esac
# (`${tty[@]+…}`: an empty array under `set -u` is an error in macOS's bash 3.2.)
exec docker run --rm --privileged --init ${tty[@]+"${tty[@]}"} \
    --volume "$repo:/src:ro" --volume "$out:/out" \
    --volume shipyard-desktop-target:/target \
    --volume shipyard-desktop-registry:/usr/local/cargo/registry \
    --volume "$HOME/.cargo/registry:/seed/mac:ro" \
    --volume shipyard-linux-cargo:/seed/linux:ro \
    --env CARGO_TARGET_DIR=/target --workdir /src "$image" "${run[@]}"

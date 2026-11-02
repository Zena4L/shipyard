#!/usr/bin/env bash
# A control for the `apparmor` job: pasta's launcher leaves Shipyard's
# AppArmor profile before it starts pasta (Ubuntu 26.04 confines pasta by a
# profile that takes signals only from unconfined processes), and no pasta
# outlives its container. As the job's user, rootless, the installed binary:
#   a detached container's monitor:   `shipyard (unconfined)` (the control:
#                                      the profile attaches, else this proves
#                                      nothing)
#   its pasta's launcher:              `unconfined`
#   once the container is removed:     no pasta left
set -euo pipefail
shipyard=/usr/local/bin/shipyard
name="pasta-control-$$"
label() { cat "/proc/$1/attr/apparmor/current" 2>/dev/null || cat "/proc/$1/attr/current"; }
found() { pgrep -u "$(id -u)" -f "$1" | head -1; }
# A detached container needs a root of its own: the host's `sleep`, its
# libraries and loader.
root=$(mktemp -d)
mkdir -p "$root/bin"
cp -L "$(command -v sleep)" "$root/bin/sleep"
for lib in $(ldd "$(command -v sleep)" | grep -oE '/[^ ]+'); do
    mkdir -p "$root$(dirname "$lib")"
    cp -L "$lib" "$root$lib"
done
"$shipyard" run -d --rootfs "$root" --name "$name" sleep 120 >/dev/null
trap '"$shipyard" rm -f "$name" >/dev/null 2>&1 || true; rm -rf "$root"' EXIT
monitor=$(found "__shipyard-monitor")
launcher=$(found "__shipyard-pasta")
[ -n "$monitor" ] && [ -n "$launcher" ] || {
    echo "no monitor ($monitor) or no pasta launcher ($launcher) found" >&2
    exit 1
}
echo "monitor: $(label "$monitor"); pasta's launcher: $(label "$launcher")"
[ "$(label "$monitor")" = "shipyard (unconfined)" ] || {
    echo "Shipyard's profile doesn't confine its monitor: this control proves nothing" >&2
    exit 1
}
[ "$(label "$launcher")" = "unconfined" ] || {
    echo "pasta's launcher kept Shipyard's profile" >&2
    exit 1
}
"$shipyard" rm -f "$name" >/dev/null
rm -rf "$root"
trap - EXIT
for _ in $(seq 50); do
    [ -z "$(pgrep -u "$(id -u)" -x pasta)" ] && { echo "no pasta left"; exit 0; }
    sleep 0.1
done
echo "pasta outlived its container: $(pgrep -u "$(id -u)" -x pasta | tr '\n' ' ')" >&2
exit 1

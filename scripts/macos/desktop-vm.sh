#!/usr/bin/env bash
# The desktop VM for trying Shipyard's app on Linux by hand (step 14.7):
#   scripts/macos/desktop-vm.sh up       make it (once: Debian 13 and a
#                                         desktop, a few GB), copy the app
#                                         and `shipyard` in, open its window
#   scripts/macos/desktop-vm.sh install  copy a new build in (after changes)
#   scripts/macos/desktop-vm.sh down     stop it (`limactl delete
#                                         shipyard-desktop` removes it)
# The binaries are the desktop image's (scripts/desktop-linux.sh
# --build-only: Debian trixie's libraries, as the VM's). In the VM's desktop,
# open a terminal and run `shipyard-desktop`: it starts your own rootless
# Shipyard service when it opens.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
name=shipyard-desktop

install_binaries() {
    timeout 3600 "$repo/scripts/desktop-linux.sh" --build-only
    # Distinct names in /tmp first (AGENTS.md §7), then into place.
    limactl copy "$repo/target/desktop-linux/bin/shipyard" "$name:/tmp/shipyard-bin"
    limactl copy "$repo/target/desktop-linux/bin/shipyard-desktop" "$name:/tmp/shipyard-desktop-bin"
    limactl shell "$name" sudo install -m 755 /tmp/shipyard-bin /usr/local/bin/shipyard
    limactl shell "$name" sudo install -m 755 /tmp/shipyard-desktop-bin /usr/local/bin/shipyard-desktop
    limactl shell "$name" rm /tmp/shipyard-bin /tmp/shipyard-desktop-bin
    # The guard in AGENTS.md §7: /dev/null still a character device.
    if [ "$(limactl shell "$name" stat -c %F /dev/null)" != "character special file" ]; then
        echo "the VM's /dev/null isn't a character device any more: stop and look" >&2
        exit 1
    fi
}

case "${1:-}" in
    up)
        if ! limactl list --quiet | grep -qx "$name"; then
            limactl create --tty=false --name "$name" "$repo/scripts/macos/desktop-vm.yaml"
            # The first boot installs a desktop (a GB or so): past Lima's
            # 10-minute default.
            limactl start --timeout 60m "$name"
            # The desktop comes up at the next boot (its target set).
            limactl stop "$name"
        fi
        limactl start "$name"
        install_binaries
        echo "the VM's window shows its desktop; in a terminal there: shipyard-desktop"
        ;;
    install) install_binaries ;;
    down) limactl stop "$name" ;;
    *) echo "usage: $0 up|install|down" >&2; exit 2 ;;
esac

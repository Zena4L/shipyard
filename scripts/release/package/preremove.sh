#!/bin/sh
# Before the .deb or .rpm is removed (step 15.5b): the AppArmor profile this
# package placed goes with it, unloaded first. Never fails the removal.
#
# A .deb calls it with "remove" (and "upgrade", "deconfigure", "failed-upgrade");
# an .rpm with how many versions will be left: 0 on a removal, 1 on an upgrade.
# Only a removal touches the profile: an upgrade's postinstall replaces it.
profile=/etc/apparmor.d/shipyard
mark="# Placed by the shipyard package: replaced on upgrade, removed with it."
case "${1:-}" in
remove | 0)
    if [ -e "$profile" ] && head -n 1 "$profile" | grep -qxF "$mark"; then
        if [ -d /sys/kernel/security/apparmor ] && command -v apparmor_parser >/dev/null 2>&1; then
            apparmor_parser -R "$profile" >/dev/null 2>&1 || true
        fi
        rm -f "$profile"
    fi
    ;;
esac
exit 0

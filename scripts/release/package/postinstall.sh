#!/bin/sh
# After the .deb or .rpm is installed or upgraded (step 15.5b): what a
# manual install's README says to do by hand, and nothing else. Never fails
# the install. Nothing is enabled or started (D20).

shipped=/usr/share/shipyard/apparmor/shipyard
profile=/etc/apparmor.d/shipyard
# The first line of a profile this package placed: one without it (a manual
# install's, or a user's own) is never replaced or removed.
mark="# Placed by the shipyard package: replaced on upgrade, removed with it."

# The AppArmor profile, where AppArmor runs (rootless containers on Ubuntu
# 23.10 and later need it), and only where its parser takes it: it needs
# AppArmor 4 (`abi <abi/4.0>`), and a profile older AppArmor can't read
# would fail at every boot.
if [ -d /etc/apparmor.d ] && command -v apparmor_parser >/dev/null 2>&1; then
    if [ -e "$profile" ] && ! head -n 1 "$profile" | grep -qxF "$mark"; then
        echo "shipyard: $profile isn't this package's (a manual install's?): left as it is" >&2
    elif apparmor_parser -QT "$shipped" >/dev/null 2>&1; then
        { echo "$mark"; cat "$shipped"; } >"$profile.new" &&
            chmod 0644 "$profile.new" && mv "$profile.new" "$profile"
        if [ -d /sys/kernel/security/apparmor ]; then
            apparmor_parser -r "$profile" 2>&1 >/dev/null |
                sed 's/^/shipyard: the AppArmor profile: /' >&2 || true
        fi
    else
        echo "shipyard: this AppArmor can't read its profile (it needs AppArmor 4): not installed" >&2
    fi
fi

# A manual install beside the package: the one first on PATH, and its units,
# win over the package's.
if [ -e /usr/local/bin/shipyard ]; then
    echo "shipyard: /usr/local/bin/shipyard is still there and comes first on PATH: remove it to use this package's" >&2
fi
for unit in /etc/systemd/system/shipyard*.service /etc/systemd/system/shipyard*.socket; do
    [ -e "$unit" ] &&
        echo "shipyard: $unit overrides the package's unit of that name: remove it to use the package's" >&2
done

# systemd told of the units, where it runs (not in a container's build).
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload 2>/dev/null || true
fi
exit 0

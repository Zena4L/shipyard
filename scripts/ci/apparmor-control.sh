#!/usr/bin/env bash
# The control for the `apparmor` job: with the restriction on
# (kernel.apparmor_restrict_unprivileged_userns=1), the installed shipyard
# may make user namespaces because of its profile, and only because of it.
# `shipyard doctor` tries (it makes a user namespace and a mount):
#   with the profile loaded:    `ok`
#   with it unloaded:           `fail` (else the job would prove nothing)
#   loaded again:               `ok`
set -euo pipefail
profile=/etc/apparmor.d/shipyard
# The installed one, where the profile attaches (not another on PATH).
shipyard=/usr/local/bin/shipyard
userns() {
    "$shipyard" doctor 2>/dev/null | awk '$2 == "user" && $3 == "namespaces" { print $1 }' || true
}
expect() {
    local got
    got=$(userns)
    if [ "$got" != "$1" ]; then
        echo "user namespaces: expected '$1' $2, got '$got'" >&2
        "$shipyard" doctor >&2 || true
        exit 1
    fi
    echo "user namespaces: $got $2"
}
[ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns)" = 1 ] || {
    echo "AppArmor's restriction on user namespaces is off here: this control proves nothing" >&2
    exit 1
}
expect ok "with the profile"
sudo apparmor_parser -R "$profile"
trap 'sudo apparmor_parser -r "$profile"' EXIT
expect fail "without it"
sudo apparmor_parser -r "$profile"
trap - EXIT
expect ok "with it again"

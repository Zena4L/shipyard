#!/usr/bin/env bash
# What rootless containers need on a CI runner (Ubuntu), for the user the
# job runs as: newuidmap/newgidmap and a subordinate range (the full ID map,
# stage 4), pasta (their network, stage 7), and a delegated cgroup (limits,
# stage 5): the user's systemd manager, kept running (lingering), whose
# scope the tests then run in (`systemd-run --user --scope`).
#
# Used by the `check` and `apparmor` jobs. It doesn't touch AppArmor: the
# `check` job turns the restriction off itself, the `apparmor` job leaves it
# on and installs Shipyard's profile.
set -euo pipefail
sudo apt-get update -qq
sudo apt-get install -y -qq uidmap passt
me=$(id -un)
grep -q "^$me:" /etc/subuid || sudo usermod --add-subuids 100000-165535 "$me"
grep -q "^$me:" /etc/subgid || sudo usermod --add-subgids 100000-165535 "$me"
sudo loginctl enable-linger "$me"
bus="/run/user/$(id -u)/bus"
for _ in $(seq 50); do [ -S "$bus" ] && break; sleep 0.2; done
[ -S "$bus" ]
if [ -n "${GITHUB_ENV:-}" ]; then
    echo "XDG_RUNTIME_DIR=/run/user/$(id -u)" >> "$GITHUB_ENV"
    echo "DBUS_SESSION_BUS_ADDRESS=unix:path=$bus" >> "$GITHUB_ENV"
fi

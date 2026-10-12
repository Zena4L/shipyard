#!/usr/bin/env bash
# The cargo runner for root tests on a real machine (CI's runner): each test
# binary runs as root in a network namespace of its own, with only `lo`, up.
#
# As root, `shipyard run` puts containers on the default bridge network: it
# makes the bridge `shipyard0`, nf_tables rules and turns on IPv4
# forwarding. Done in a throwaway namespace, none of that touches the
# machine's networking, and it all goes when the test binary exits. The
# networks' state goes to a scratch directory too (SHIPYARD_NETWORK_DIR).
#
# usage (as a cargo runner): ci-netns.sh TEST_BINARY [ARGS...]
set -euo pipefail
networks=$(mktemp -d)
trap 'rm -rf "$networks" 2>/dev/null || sudo rm -rf "$networks"' EXIT
# Already root (the Fedora VM's runner): no sudo, which would also make
# SUDO_UID root's.
sudo=(sudo -E)
[ "$(id -u)" = 0 ] && sudo=()
"${sudo[@]}" env SHIPYARD_NETWORK_DIR="$networks" unshare --net sh -c 'ip link set lo up 2>/dev/null || true; exec "$@"' -- "$@"

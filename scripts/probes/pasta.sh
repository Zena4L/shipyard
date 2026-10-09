#!/usr/bin/env bash
# Checks behind rootless networking with pasta (stage 7): which options this
# pasta takes, how it signals that it's ready, whether it quits when the
# container it serves is gone, and what its port-forwarding defaults publish
# and expose. Each line answers one question; docs/stages/07-networking.md
# records the results.
#
# It installs pasta (the `passt` package) itself, so run it in a throwaway
# privileged container, once per distribution to compare versions:
#
#   for d in bookworm trixie; do
#     docker run --rm --privileged -v "$PWD/scripts/probes":/p:ro debian:$d bash /p/pasta.sh
#   done
#
# The checks run as an ordinary user, as Shipyard's rootless runs do: the
# "container" is a process in a user and network namespace of that user's
# own (`unshare --user --map-root-user --net`), and pasta attaches to it.
set -uo pipefail

if [ "$(id -u)" = 0 ]; then
    apt-get update -qq >/dev/null && apt-get install -y -qq passt iproute2 python3 procps util-linux >/dev/null 2>&1 ||
        { echo "probe setup failed: installing passt"; exit 2; }
    echo "debian: $(cat /etc/debian_version)"
    echo "passt package: $(dpkg-query -W -f '${Version}' passt)"
    useradd --create-home prober 2>/dev/null
    exec setpriv --reuid prober --regid prober --init-groups env HOME=/home/prober bash "$0"
fi

cd /tmp || exit 2
work=$(mktemp -d)

# A "container": a process in a user and network namespace of its own. Its
# PID goes to stdout; if it never gets its own namespace, the probe stops
# (every later line would be meaningless).
container() {
    unshare --user --map-root-user --net sleep 60 >/dev/null 2>&1 &
    local pid=$!
    # wait until it's in its namespaces
    for _ in $(seq 50); do
        if [ "$(readlink /proc/$pid/ns/net)" != "$(readlink /proc/self/ns/net)" ]; then
            echo "$pid"
            return
        fi
        sleep 0.02
    done
    echo "probe setup failed: no user and network namespace for an ordinary user" >&2
    kill -9 "$pid" 2>/dev/null
    exit 2
}
inside() { nsenter --preserve-credentials --user --net --target "$1" "${@:2}"; }

# P1: which options this pasta accepts (an unknown option fails at once).
for option in "--config-net" "--dns-forward 169.254.1.1" "--map-guest-addr 169.254.1.2" \
    "--map-host-loopback none" "--no-map-gw" "--quiet" "--userns /proc/self/ns/user"; do
    # Unquoted on purpose: "--option value" splits into two arguments.
    # shellcheck disable=SC2086
    out=$(timeout 5 pasta --foreground $option --version 2>&1)
    if echo "$out" | grep -qiE "unrecognized|invalid|unknown option"; then have=no; else have=yes; fi
    echo "P1 option ${option%% *}: $have"
done

# P2: pasta attached by PID, in the foreground, with a PID file: written once
# it's set up? And what does the namespace get with --config-net?
c=$(container)
pasta --foreground --quiet --config-net --pid "$work/p2.pid" -t none -u none -T none -U none "$c" \
    >"$work/p2.log" 2>&1 &
p=$!
start=$(date +%s%N)
for _ in $(seq 250); do [ -s "$work/p2.pid" ] && break; sleep 0.02; done
if [ -s "$work/p2.pid" ]; then
    echo "P2 --foreground --pid: written after $((($(date +%s%N) - start) / 1000000)) ms (pid $(cat "$work/p2.pid"), foreground $p)"
else
    echo "P2 --foreground --pid: not written within 5 s: $(head -c 300 "$work/p2.log")"
fi
echo "P2 the namespace has: $(inside "$c" ip -brief address | tr -s ' ' | tr '\n' ';')"
echo "P2 routes there: $(inside "$c" ip route | tr '\n' ';')"

# P3: does pasta quit when the container's last process is gone? (Only
# meaningful if it was running.)
if ! kill -0 "$p" 2>/dev/null; then
    echo "probe setup failed: pasta didn't stay up: $(head -c 300 "$work/p2.log")"
    exit 2
fi
kill -9 "$c"
wait "$c" 2>/dev/null
gone=no
for _ in $(seq 50); do
    if ! kill -0 "$p" 2>/dev/null; then gone=yes; break; fi
    sleep 0.1
done
echo "P3 quits when the container (PID target) is gone: $gone"
kill -9 "$p" 2>/dev/null
wait "$p" 2>/dev/null

# P3b: the same with --netns /proc/PID/ns/net --userns /proc/PID/ns/user
c=$(container)
pasta --foreground --quiet --config-net -t none -u none -T none -U none \
    --userns "/proc/$c/ns/user" --netns "/proc/$c/ns/net" >"$work/p3.log" 2>&1 &
p=$!
sleep 1
if kill -0 "$p" 2>/dev/null; then
    kill -9 "$c"
    wait "$c" 2>/dev/null
    gone=no
    for _ in $(seq 50); do
        if ! kill -0 "$p" 2>/dev/null; then gone=yes; break; fi
        sleep 0.1
    done
    echo "P3 quits when the container is gone (--netns/--userns paths): $gone"
else
    echo "P3 --netns/--userns paths: pasta didn't start: $(head -c 300 "$work/p3.log")"
    kill -9 "$c" 2>/dev/null
fi
kill -9 "$p" 2>/dev/null
wait "$p" 2>/dev/null

# P4: what a port the container listens on looks like from the host.
publish_check() {
    local c
    c=$(container)
    # The server answers a token: a connection pasta accepts on the host side
    # but can't carry into the container gets nothing.
    inside "$c" python3 -c '
import socket
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", 18080)); s.listen(); s.settimeout(6)
try:
    conn, _ = s.accept(); conn.sendall(b"inside")
except OSError:
    pass' &
    local server=$!
    sleep 0.5
    pasta --foreground --quiet --config-net "$@" "$c" >/dev/null 2>&1 &
    local p=$!
    sleep 1.5
    if python3 -c '
import socket, sys
c = socket.create_connection(("127.0.0.1", 18080), timeout=2)
sys.exit(0 if c.recv(16) == b"inside" else 1)' 2>/dev/null; then
        echo "reachable"
    else
        echo "not reachable"
    fi
    kill -9 "$p" "$server" "$c" 2>/dev/null
    wait "$p" "$server" "$c" 2>/dev/null
}
echo "P4 a port the container listens on, from the host, pasta's defaults: $(publish_check)"
echo "P4 the same with -t none -u none -T none -U none: $(publish_check -t none -u none -T none -U none)"
echo "P4 the same with -t 18080 (and the rest none): $(publish_check -t 18080 -u none -T none -U none)"

# P5: can the container reach a service on the host's loopback?
loopback_check() {
    python3 -c '
import socket
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", 18081)); s.listen(); s.settimeout(6)
try:
    while True:
        conn, _ = s.accept(); conn.sendall(b"host")
except OSError:
    pass' &
    local server=$!
    local c
    c=$(container)
    pasta --foreground --quiet --config-net "$@" "$c" >/dev/null 2>&1 &
    local p=$!
    sleep 1.5
    local gw out=""
    gw=$(inside "$c" ip -4 route show default | awk '{print $3; exit}')
    for addr in 127.0.0.1 $gw; do
        if inside "$c" python3 -c "
import socket, sys
c = socket.create_connection(('$addr', 18081), timeout=2)
sys.exit(0 if c.recv(16) == b'host' else 1)" 2>/dev/null; then
            out="$out $addr reachable;"
        else
            out="$out $addr not reachable;"
        fi
    done
    echo "${out# }"
    kill -9 "$p" "$server" "$c" 2>/dev/null
    wait "$p" "$server" "$c" 2>/dev/null
}
echo "P5 the host's 127.0.0.1 service from inside, pasta's defaults: $(loopback_check)"
echo "P5 the same with -T none -U none: $(loopback_check -t none -u none -T none -U none)"
echo "P5 the same with -T none -U none --no-map-gw: $(loopback_check -t none -u none -T none -U none --no-map-gw)"
echo "P5 the same with -T none -U none --map-host-loopback none: $(loopback_check -t none -u none -T none -U none --map-host-loopback none)"

rm -rf "$work"

#!/usr/bin/env bash
# Checks behind giving pasta the container's network namespace as a file
# (stage 8, step 8.1c, F32): SELinux (Fedora's `pasta_t`) refuses pasta's open
# of an unconfined process's /proc/<pid>/ns, so pasta can't attach by PID
# there. Podman's way: start pasta inside the container's user namespace, in a
# mount namespace of its own where the network namespace is bind-mounted onto
# a file, and give it `--netns FILE`.
#
# Run as an ordinary user with pasta, nsenter, unshare, setpriv and ip
# installed, and $XDG_RUNTIME_DIR set (a login session's), on each host to
# compare (Fedora enforcing, Ubuntu with AppArmor, Debian):
#
#   bash scripts/probes/pasta-netns.sh
#
# Each line answers one question; the stage notes record the results.
set -uo pipefail

: "${XDG_RUNTIME_DIR:?needs a runtime directory, as a login session has}"
work=$(mktemp -d "$XDG_RUNTIME_DIR/pasta-probe.XXXXXX")
made_local=
[ -d "$HOME/.local" ] || made_local=yes
mkdir -p "$HOME/.local"
home_work=$(mktemp -d "$HOME/.local/pasta-probe.XXXXXX")
tmp_work="${TMPDIR:-/tmp}/shipyard-$(id -u)/pasta-probe.$$"
mkdir -p "$tmp_work"
trap 'kill $(jobs -p) 2>/dev/null; rm -rf "$work" "$home_work" "$tmp_work"; [ -n "$made_local" ] && rmdir "$HOME/.local" 2>/dev/null' EXIT

echo "host: $(. /etc/os-release; echo "$PRETTY_NAME"), kernel $(uname -r)"
echo "pasta: $(pasta --version 2>&1 | head -1)"
echo "selinux: $(getenforce 2>/dev/null || echo none); apparmor: $(cat /sys/module/apparmor/parameters/enabled 2>/dev/null || echo none)"

# A "container": a process in a user and network namespace of its own.
container() {
    unshare --user --map-root-user --net sleep 60 >/dev/null 2>&1 &
    local pid=$!
    for _ in $(seq 50); do
        if [ "$(readlink /proc/$pid/ns/net)" != "$(readlink /proc/self/ns/net)" ]; then
            echo "$pid"
            return
        fi
        sleep 0.1
    done
    echo "no user and network namespace here (unshare refused): nothing to probe" >&2
    exit 2
}

# Whether the container's namespace got an address from pasta.
configured() {
    nsenter -t "$1" -U -n --preserve-credentials ip -br -4 addr 2>/dev/null |
        grep -v '^lo ' | grep -q '[0-9]\.[0-9]'
}

pasta_args=(--foreground --config-net --quiet -T none -U none -t none -u none --no-map-gw)

# N1: pasta by PID (the control: what Shipyard did until 8.1c).
pid=$(container)
timeout 5 pasta "${pasta_args[@]}" --pid "$work/n1.pid" "$pid" 2>"$work/n1.log" &
sleep 1.5
if configured "$pid"; then echo "N1 by PID: works"; else echo "N1 by PID: fails: $(head -1 "$work/n1.log")"; fi
kill "$pid" 2>/dev/null

# The fix: join the user namespace, make a private mount namespace, bind the
# network namespace onto FILE, exec pasta with --netns FILE.
cat >"$work/bind-and-exec.sh" <<'SCRIPT'
pid=$1 file=$2
shift 2
mount --bind "/proc/$pid/ns/net" "$file" && exec pasta "$@"
SCRIPT
by_file() {
    local pid=$1 file=$2 log=$3
    touch "$file"
    timeout 5 nsenter -t "$pid" -U --preserve-credentials -- \
        unshare -m --propagation private -- \
        sh "$work/bind-and-exec.sh" "$pid" "$file" "${pasta_args[@]}" --netns "$file" 2>"$log" &
}

# N2: the namespace file in the runtime directory.
pid=$(container)
by_file "$pid" "$work/netns" "$work/n2.log"
sleep 1.5
if configured "$pid"; then
    echo "N2 by file in the runtime dir: works"
    # N4: the bind mount is the launcher's alone: the host sees no mount on
    # the file (asked only where N2 made one).
    if awk '{print $5}' /proc/self/mountinfo | grep -qxF "$work/netns"; then
        echo "N4 the host sees the bind mount: yes (leaked)"
    else
        echo "N4 the host sees the bind mount: no"
    fi
else
    echo "N2 by file in the runtime dir: fails: $(head -1 "$work/n2.log")"
fi
kill "$pid" 2>/dev/null

# N3: the namespace file under ~/.local.
pid=$(container)
by_file "$pid" "$home_work/netns" "$work/n3.log"
sleep 1.5
if configured "$pid"; then echo "N3 by file under ~/.local: works"; else echo "N3 by file under ~/.local: fails: $(head -1 "$work/n3.log")"; fi
kill "$pid" 2>/dev/null

# N6: the namespace file in the temporary directory (the run's directory
# without a runtime directory: after `su`, say).
pid=$(container)
by_file "$pid" "$tmp_work/netns" "$work/n6.log"
sleep 1.5
if configured "$pid"; then echo "N6 by file in \$TMPDIR/shipyard-UID: works"; else echo "N6 by file in \$TMPDIR/shipyard-UID: fails: $(head -1 "$work/n6.log")"; fi
kill "$pid" 2>/dev/null

# N5: does joining a user namespace clear the parent-death signal? A parent
# starts `setpriv --pdeathsig KILL nsenter -U … sleep`, then dies: the sleep
# survives if the signal was cleared by the join.
pid=$(container)
(
    setpriv --pdeathsig KILL nsenter -t "$pid" -U --preserve-credentials sleep 30 &
    echo $! >"$work/n5.pid"
    wait
) &
parent=$!
# Only once the sleeper has joined (killed before, the signal is still set
# whatever the join does).
for _ in $(seq 50); do
    sleeper=$(cat "$work/n5.pid" 2>/dev/null)
    [ -n "$sleeper" ] && [ "$(readlink "/proc/$sleeper/ns/user" 2>/dev/null)" = "$(readlink "/proc/$pid/ns/user")" ] && break
    sleep 0.1
done
kill -9 "$parent"
sleep 0.5
if kill -0 "$(cat "$work/n5.pid")" 2>/dev/null; then
    echo "N5 PDEATHSIG after setns(CLONE_NEWUSER): cleared (set it again after joining)"
    kill "$(cat "$work/n5.pid")" 2>/dev/null
else
    echo "N5 PDEATHSIG after setns(CLONE_NEWUSER): kept"
fi
kill "$pid" 2>/dev/null

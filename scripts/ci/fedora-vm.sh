#!/usr/bin/env bash
# Runs Shipyard's tests on Fedora, with SELinux enforcing, in a throwaway
# QEMU/KVM virtual machine: H4's real-host job (the test container is one
# privileged Debian; Fedora differs in SELinux, its cgroup layout, its glibc
# and its packages).
#
# On the CI runner (Ubuntu, x86-64), or by hand on any x86-64 Linux with KVM:
#   1. builds the binaries and the test executables here (the VM never
#      compiles: it wouldn't fit the job's time);
#   2. boots Fedora Cloud Base (pinned by SHA-256) with a cloud-init seed:
#      the user `tester` with a subordinate range and lingering, the packages
#      the tests use, firewalld stopped (F14 is stage 8.6's);
#   3. copies the workspace and the executables to the same absolute path
#      in the VM (the test executables hold compile-time paths);
#   4. runs scripts/ci/run-test-binaries.sh there: both passes, as
#      test-linux.sh runs them.
#
# Needs: qemu-system-x86_64, qemu-img, cloud-localds (cloud-image-utils),
# ssh, rsync, jq, gcc. Caches the image in $VM_CACHE (~/.cache/shipyard-vm).
set -euo pipefail

# A supported release (an end-of-life one loses its package repositories).
IMAGE=Fedora-Cloud-Base-Generic-44-1.7.x86_64.qcow2
URL=https://download.fedoraproject.org/pub/fedora/linux/releases/44/Cloud/x86_64/images/$IMAGE
SHA256=28680fe5b371a5a82ebf43a31926e086a168e59949d03969c5093e7071f90b7f

repo=$(cd "$(dirname "$0")/../.." && pwd)
cache=${VM_CACHE:-$HOME/.cache/shipyard-vm}
work=$(mktemp -d)
port=${VM_SSH_PORT:-2222}
trap 'cleanup' EXIT
# A cancelled job gets SIGINT, then SIGTERM: stop at once (the EXIT trap then
# stops the VM, which `-daemonize` took out of the job's processes).
trap 'exit 130' INT
trap 'exit 143' TERM
cleanup() {
    # What was running in the background (see `interruptible`): ssh into
    # the VM, a build.
    local job
    for job in $(jobs -p); do
        # A function's job is a subshell: its command (ssh, cargo) first.
        pkill -P "$job" 2>/dev/null || true
        kill "$job" 2>/dev/null || true
    done
    if [ -f "$work/qemu.pid" ]; then
        kill "$(cat "$work/qemu.pid")" 2>/dev/null || true
    fi
    if [ -f "$work/console.log" ]; then
        echo "--- the VM's console (last lines) ---"
        tail -n 40 "$work/console.log" || true
    fi
    rm -rf "$work"
}

# Runs "$@" in the background and waits for it: bash runs a trap only once
# its foreground command ends, but `wait` returns at once on a signal, so a
# cancel isn't held up by an hour of tests. Returns the command's status.
interruptible() {
    "$@" &
    wait "$!"
}

# --- KVM: without it the suite would crawl under emulation for hours.
if [ ! -e /dev/kvm ]; then
    echo "no /dev/kvm here: this job needs a runner with KVM (GitHub's Linux runners have it)" >&2
    exit 1
fi
if [ ! -w /dev/kvm ]; then
    # Only on a CI runner: by hand, a rule like this would stay behind.
    if [ "${GITHUB_ACTIONS:-}" != true ]; then
        echo "/dev/kvm isn't yours: add yourself to the kvm group" >&2
        exit 1
    fi
    echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' |
        sudo tee /etc/udev/rules.d/99-shipyard-kvm.rules >/dev/null
    sudo udevadm control --reload-rules
    sudo udevadm trigger --name-match=kvm
    for _ in $(seq 50); do [ -w /dev/kvm ] && break; sleep 0.1; done
    [ -w /dev/kvm ]
fi

# --- 1. Build here.
cd "$repo"
interruptible cargo build --locked --bins
# (json-render-diagnostics: a compile error still prints, as itself.)
list_test_binaries() {
    set -o pipefail
    cargo test --locked --workspace --no-run --message-format=json-render-diagnostics |
        jq -r 'select(.reason == "compiler-artifact" and .profile.test == true and .executable != null)
               | "\(.manifest_path | rtrimstr("/Cargo.toml"))\t\(.executable)"' \
        > "$repo/target/test-binaries.txt"
}
interruptible list_test_binaries
count=$(wc -l < "$repo/target/test-binaries.txt")
echo "$count test executables"
[ "$count" -gt 0 ]
mkdir -p "$repo/target/probes"
gcc -O2 -o "$repo/target/probes/terminal" scripts/probes/terminal.c

# --- 2. The image, verified, and a VM from it.
mkdir -p "$cache"
if ! echo "$SHA256  $cache/$IMAGE" | sha256sum -c --quiet - 2>/dev/null; then
    curl -fL --retry 3 --retry-delay 5 -o "$cache/$IMAGE.part" "$URL"
    echo "$SHA256  $cache/$IMAGE.part" | sha256sum -c --quiet -
    mv "$cache/$IMAGE.part" "$cache/$IMAGE"
fi
qemu-img create -q -f qcow2 -F qcow2 -b "$cache/$IMAGE" "$work/disk.qcow2" 30G
ssh-keygen -q -t ed25519 -N '' -C shipyard-ci -f "$work/key"
cat > "$work/user-data" <<CONFIG
#cloud-config
users:
  - name: tester
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    ssh_authorized_keys:
      - $(cat "$work/key.pub")
packages:
  - passt
  - nftables
  - rsync
  - iproute
  - bind-utils
  - util-linux
  - util-linux-script
  - shadow-utils
  - perl
  - audit
runcmd:
  - [sh, -c, "systemctl disable --now firewalld 2>/dev/null || true"]
  - [sh, -c, "grep -q '^tester:' /etc/subuid || usermod --add-subuids 100000-165535 tester"]
  - [sh, -c, "grep -q '^tester:' /etc/subgid || usermod --add-subgids 100000-165535 tester"]
  - [loginctl, enable-linger, tester]
CONFIG
echo "instance-id: shipyard-ci" > "$work/meta-data"
cloud-localds "$work/seed.iso" "$work/user-data" "$work/meta-data"
qemu-system-x86_64 -enable-kvm -cpu host -smp 4 -m 6144 \
    -drive file="$work/disk.qcow2",if=virtio \
    -drive file="$work/seed.iso",if=virtio,format=raw,readonly=on \
    -netdev user,id=net0,hostfwd=tcp:127.0.0.1:$port-:22 \
    -device virtio-net-pci,netdev=net0 \
    -display none -serial file:"$work/console.log" \
    -daemonize -pidfile "$work/qemu.pid"

vm() {
    ssh -q -i "$work/key" -p "$port" -o StrictHostKeyChecking=no \
        -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 tester@127.0.0.1 "$@"
}
echo "waiting for the VM"
for _ in $(seq 60); do vm true 2>/dev/null && break; sleep 5; done
vm true
interruptible vm cloud-init status --wait >/dev/null || true
vm 'cat /etc/fedora-release; getenforce'

# --- 3. The workspace, and what was built, at the same path.
vm "sudo mkdir -p '$repo' && sudo chown tester: '$repo'"
export RSYNC_RSH="ssh -q -i $work/key -p $port -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"
interruptible rsync -a --exclude /target --exclude /.git --exclude /fuzz/target "$repo/" "tester@127.0.0.1:$repo/"
{
    echo target/debug/shipyard
    echo target/debug/shipyard-runtime
    echo target/test-binaries.txt
    echo target/probes/terminal
    cut -f2 "$repo/target/test-binaries.txt" | sed "s|^$repo/||"
} > "$work/files"
interruptible rsync -a --files-from="$work/files" "$repo/" "tester@127.0.0.1:$repo/"

# --- 4. Both passes, in the VM.
interruptible vm "sudo '$repo/scripts/ci/run-test-binaries.sh' '$repo'"

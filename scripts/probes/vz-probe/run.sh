#!/usr/bin/env bash
# Step 13.1's probe (macOS production): Rosetta, the memory balloon, TRIM, virtiofs and a
# pause, in a VM of Virtualization.framework booted from this tree's guest (built by
# scripts/guest/build.sh into target/guest). Runs on an Apple-silicon Mac, as the user.
#
# usage: run.sh [WORK]   (WORK: a scratch directory, made if missing; default mktemp)
# Results: WORK/share/results.txt (the guest), WORK/share/host.txt (the Mac),
# WORK/share/console.log.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../../.." && pwd)
guest=$repo/target/guest
work=${1:-$(mktemp -d)}
mkdir -p "$work"
share=$work/share
rm -rf "$share"
mkdir -p "$share/amd64"
[ -f "$guest/Image" ] || { echo "no guest: run scripts/guest/build.sh first" >&2; exit 1; }

# amd64 programs: Alpine's static busybox and its Go yq (pinned by version), and a Rust one.
alpine=https://dl-cdn.alpinelinux.org/alpine/v3.24
fetch() { # REPO PACKAGE FILE-IN-IT OUT
    curl -fsS --max-time 120 -o "$work/$2.apk" "$alpine/$1/x86_64/$2.apk"
    tar -xzf "$work/$2.apk" -C "$work" "$3" 2>/dev/null
    cp "$work/$3" "$share/amd64/$4"
}
fetch main busybox-static-1.37.0-r31 bin/busybox.static busybox
fetch community yq-go-4.53.3-r2 usr/bin/yq yq
rustc --edition 2021 -O --target x86_64-unknown-linux-musl -C linker=rust-lld \
    -C link-self-contained=yes -C target-feature=+crt-static \
    -o "$share/amd64/threads" "$here/threads.rs"
file "$share/amd64/"* | sed 's|.*/||'
cp "$here/probe.sh" "$share/"

# The guest's initramfs with the probe's init appended (the kernel unpacks both).
mkdir -p "$work/overlay"
cp "$here/probe-init" "$work/overlay/probe-init"
chmod 755 "$work/overlay/probe-init"
(cd "$work/overlay" && echo probe-init | cpio -o -H newc 2>/dev/null | gzip -9) >"$work/overlay.cpio.gz"
cat "$guest/initramfs.cpio.gz" "$work/overlay.cpio.gz" >"$work/initramfs.cpio.gz"

# A sparse 4 GiB disk.
rm -f "$work/disk.img"
truncate -s 4g "$work/disk.img"

# The host program, signed with the virtualization entitlement.
swiftc -swift-version 5 -O -o "$work/vz-probe" "$here/probe.swift" \
    -framework Virtualization 2>&1 | grep -v "warning:" || true
codesign --force --sign - --entitlements "$repo/contrib/macos/shipyard.entitlements" \
    "$work/vz-probe"

"$work/vz-probe" "$guest/Image" "$work/initramfs.cpio.gz" "$work/disk.img" "$share"
echo "== results ($share)"
cat "$share/results.txt" "$share/host.txt"

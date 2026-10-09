#!/usr/bin/env bash
# Runs the workspace's test executables without cargo, both passes as
# scripts/test-linux.sh runs them: as `tester` (rootless, in its delegated
# cgroup, rootless tests required), then as root (`--include-ignored`, each
# executable in a network namespace of its own: scripts/ci-netns.sh).
#
# In the Fedora VM (scripts/ci/fedora-vm.sh), as root through sudo from
# `tester` (so SUDO_UID names tester, whom the root tests run rootless
# commands as). REPO holds target/test-binaries.txt: "package dir<TAB>
# executable" per line, as cargo's JSON listed them.
#
# usage: run-test-binaries.sh REPO [LIST [PROBE]]
#   LIST:  the executables (default REPO/target/test-binaries.txt)
#   PROBE: the terminal probe, built (default REPO/target/probes/terminal)
set -uo pipefail
repo=$1
list=${2:-$repo/target/test-binaries.txt}
probe=${3:-$repo/target/probes/terminal}
user=tester
uid=$(id -u "$user")

# The host this is meant to be, with what the tests need: a tool missing
# would turn its tests into skips (cloud-init's package step can fail
# quietly).
fail() { echo "$*" >&2; exit 1; }
[ "$(getenforce)" = Enforcing ] || fail "SELinux isn't enforcing here"
if systemctl is-active --quiet firewalld; then fail "firewalld runs (it's stage 8.6's)"; fi
for tool in ip nft dig pasta newuidmap newgidmap script perl; do
    command -v "$tool" >/dev/null || fail "no $tool here"
done
[ -s "$list" ] || fail "no test executables listed in $list"
cat /etc/fedora-release
uname -r
ldd --version | head -1
findmnt -no PROPAGATION /
nft --version

# The kernel probe (H4: the probes on each host): logged, not gating.
echo "--- scripts/probes/terminal.c"
if [ -x "$probe" ]; then
    timeout 120 "$probe" </dev/null || echo "(the probe failed: logged, not gating)"
else
    echo "(no probe built at $probe)"
fi

export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
# Fedora's /tmp is a tmpfs mounted nosuid and nodev: the tests that make
# setuid programs or device nodes need a temporary directory without those.
export TMPDIR=/var/tmp
# Missing reference tools (ip, nft, dig) fail the differential tests here
# instead of skipping them.
export SHIPYARD_TEST_TOOLS=required
failed=()
ran=0
run_pass() {
    local pass=$1
    # The list on its own descriptor, each test's stdin /dev/null: a test
    # reading its stdin mustn't eat the rest of the list. (Closed for the
    # test: the tests check that no stray descriptor reaches a container.)
    while IFS=$'\t' read -r -u 3 package test; do
        echo "=== $pass: ${test##*/}"
        ran=$((ran + 1))
        if [ "$pass" = tester ]; then
            (cd "$package" && timeout 1200 runuser -u "$user" -- env \
                HOME="/home/$user" PATH="$PATH" TMPDIR="$TMPDIR" CARGO_MANIFEST_DIR="$package" \
                XDG_RUNTIME_DIR="/run/user/$uid" \
                DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" \
                SHIPYARD_TEST_ROOTLESS=required SHIPYARD_TEST_DELEGATED=required \
                SHIPYARD_TEST_TOOLS=required \
                systemd-run --user --scope --quiet "$test" </dev/null 3<&-)
        else
            # The root tests run rootless commands as `tester` (a sudo from
            # root would name root as the caller).
            (cd "$package" && CARGO_MANIFEST_DIR="$package" SHIPYARD_TEST_USER="$user" \
                timeout 1200 "$repo/scripts/ci-netns.sh" "$test" --include-ignored </dev/null 3<&-)
        fi || failed+=("$pass: ${test##*/}")
    done 3< "$list"
}
run_pass tester
run_pass root
expected=$(($(wc -l < "$list") * 2))
[ "$ran" = "$expected" ] || failed+=("ran $ran test executables of $expected")

# What SELinux denied meanwhile, passing or not: a denial a test tolerated
# is still worth seeing.
echo "--- SELinux denials (AVC) since boot"
ausearch -m avc -ts boot 2>/dev/null || echo "(none)"

if [ ${#failed[@]} -gt 0 ]; then
    echo "failed:"
    printf '  %s\n' "${failed[@]}"
    exit 1
fi
echo "== all Fedora checks passed"

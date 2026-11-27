#!/usr/bin/env bash
# Runs the workspace's test executables without cargo, both passes as
# scripts/test-linux.sh runs them: as `tester` (rootless, in its delegated
# cgroup, rootless tests required), then as root (`--include-ignored`, each
# executable in a network namespace of its own: scripts/ci-netns.sh); then
# the build tests on their own, as tester and as root, with the network
# (a builder pulls by name), as test-linux.sh runs them.
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
systemctl is-active --quiet firewalld || fail "firewalld isn't running here"
for tool in ip nft iptables dig pasta newuidmap newgidmap script perl docker; do
    command -v "$tool" >/dev/null || fail "no $tool here"
done
docker compose version >/dev/null 2>&1 || fail "no docker compose here"
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
# One test executable: as tester (in its delegated scope) or as root (in a
# network namespace of its own, unless "network" asks for the VM's), its
# extra arguments after.
run_one() {
    local pass=$1 package=$2 test=$3
    shift 3
    if [ "$pass" = tester ]; then
        (cd "$package" && timeout 1200 runuser -u "$user" -- env \
            HOME="/home/$user" PATH="$PATH" TMPDIR="$TMPDIR" CARGO_MANIFEST_DIR="$package" \
            XDG_RUNTIME_DIR="/run/user/$uid" \
            DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" \
            SHIPYARD_TEST_ROOTLESS=required SHIPYARD_TEST_DELEGATED=required \
            SHIPYARD_TEST_TOOLS=required SHIPYARD_TEST_BUILD="${SHIPYARD_TEST_BUILD:-}" \
            SHIPYARD_TEST_MIRROR="${SHIPYARD_TEST_MIRROR:-}" \
            systemd-run --user --scope --quiet "$test" "$@" </dev/null 3<&-)
    elif [ "$pass" = root ]; then
        # The root tests run rootless commands as `tester` (a sudo from
        # root would name root as the caller).
        (cd "$package" && CARGO_MANIFEST_DIR="$package" SHIPYARD_TEST_USER="$user" \
            timeout 1200 "$repo/scripts/ci-netns.sh" "$test" --include-ignored "$@" </dev/null 3<&-)
    else
        (cd "$package" && CARGO_MANIFEST_DIR="$package" SHIPYARD_TEST_USER="$user" \
            SHIPYARD_TEST_NETWORK=required \
            timeout 1200 "$test" --include-ignored "$@" </dev/null 3<&-)
    fi
}
# Whether tester can still make a directory in TMPDIR (a real create, not
# `access`: a label or a full disk can refuse one `test -w` allows).
tester_can_make() {
    local probe="$TMPDIR/.tester-probe-$$"
    runuser -u "$user" -- mkdir "$probe" 2>/dev/null && rmdir "$probe"
}
# Said before the passes, after each, and after the first executable that
# takes it away (F199: a build test's helper once made /var/tmp 0755 as
# root, and the tester build tests after failed there with EACCES); if it's
# gone, what's there: what's mounted, its mode and label, and the kernel's
# recent SELinux denials. Logged, never gating: a test that needs the
# directory fails by itself.
tmp_writable() {
    local when=$1 probe="$TMPDIR/.tester-probe-$$"
    if tester_can_make; then
        echo "--- $when: $TMPDIR is writable by $user"
        return
    fi
    echo "--- $when: $TMPDIR is NOT writable by $user"
    { runuser -u "$user" -- mkdir "$probe" && rmdir "$probe"; } || true
    findmnt -T "$TMPDIR" || true
    findmnt -R -o TARGET,SOURCE,FSTYPE,OPTIONS "$TMPDIR" | head -40 || true
    ls -ldZ "$TMPDIR" / /var || true
    ausearch -m avc -ts recent 2>/dev/null | tail -20 || true
}
run_pass() {
    local pass=$1
    # The list on its own descriptor, each test's stdin /dev/null: a test
    # reading its stdin mustn't eat the rest of the list. (Closed for the
    # test: the tests check that no stray descriptor reaches a container.)
    while IFS=$'\t' read -r -u 3 package test; do
        echo "=== $pass: ${test##*/}"
        ran=$((ran + 1))
        run_one "$pass" "$package" "$test" || failed+=("$pass: ${test##*/}")
        # The first executable after which tester can't write TMPDIR, named.
        if [ -z "$tmp_lost" ] && ! tester_can_make; then
            tmp_lost="$pass: ${test##*/}"
            tmp_writable "just after $tmp_lost"
        fi
    done 3< "$list"
}
tmp_lost=
# The build tests (12.3) skip themselves in the passes and run after them.
export SHIPYARD_TEST_BUILD=later
tmp_writable "before the passes"
run_pass tester
tmp_writable "after the tester pass"
run_pass root
tmp_writable "after the root pass"
expected=$(($(wc -l < "$list") * 2))
[ "$ran" = "$expected" ] || failed+=("ran $ran test executables of $expected")
unset SHIPYARD_TEST_BUILD
build=$(awk -F'\t' '$1 ~ /\/shipyard-cli$/ && $2 ~ /\/build-[0-9a-f]+$/' "$list")
if [ -z "$build" ]; then
    failed+=("no build test executable listed")
else
    IFS=$'\t' read -r package test <<<"$build"
    echo "=== build tests, as tester"
    run_one tester "$package" "$test" --test-threads 4 || failed+=("build: tester")
    echo "=== build tests, as root"
    run_one network "$package" "$test" --test-threads 4 || failed+=("build: root")
fi
tmp_writable "after the build tests"

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

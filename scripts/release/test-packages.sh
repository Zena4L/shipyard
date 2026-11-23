#!/usr/bin/env bash
# The Linux packages installed and run (step 15.5b): the .deb in Ubuntu
# 24.04, the .rpm in Fedora 42 (both pinned in build.env), for this machine's
# architecture, from DIR (scripts/release/packages.sh's). In each: the files
# where they belong, the units naming /usr/bin and enabled nowhere, the
# AppArmor profile in place, `shipyard version` and `doctor` running, a
# container run as root, then the package removed, its binaries with it.
#
# usage: scripts/release/test-packages.sh DIR
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
dir=${1:?usage: scripts/release/test-packages.sh DIR}
dir=$(cd "$dir" && pwd)
. "$repo/scripts/release/build.env"
case $(uname -m) in
x86_64 | amd64) deb_arch=amd64 rpm_arch=x86_64 ;;
aarch64 | arm64) deb_arch=arm64 rpm_arch=aarch64 ;;
*)
    echo "no packages for $(uname -m)" >&2
    exit 1
    ;;
esac
deb=$(cd "$dir" && ls -- *_"$deb_arch".deb)
rpm=$(cd "$dir" && ls -- *."$rpm_arch".rpm)

# What's checked once it's installed, the same in both.
checks='
set -euo pipefail
trap '"'"'echo "a check failed (line $LINENO)" >&2'"'"' ERR
for file in /usr/bin/shipyard /usr/bin/shipyard-runtime; do
    [ -x "$file" ] || { echo "no $file" >&2; exit 1; }
done
for unit in shipyard-recover.service shipyard-stop.service shipyard.service shipyard.socket; do
    for place in /usr/lib/systemd/system /usr/lib/systemd/user; do
        [ -f "$place/$unit" ] || { echo "no $place/$unit" >&2; exit 1; }
        # A service runs the packaged binary (the socket runs nothing).
        case $unit in
        *.service)
            grep -q "^Exec[A-Za-z]*=/usr/bin/shipyard " "$place/$unit" ||
                { echo "$place/$unit runs no /usr/bin/shipyard" >&2; exit 1; } ;;
        esac
    done
done
# (Never `! cmd` under set -e: a negated command never stops the script.)
if grep -rq /usr/local/bin /usr/lib/systemd/system/shipyard* /usr/lib/systemd/user/shipyard*; then
    echo "a unit names /usr/local/bin" >&2; exit 1
fi
# Enabled nowhere: no unit of ours linked into a target.
if [ -n "$(find /etc/systemd -name "shipyard*" 2>/dev/null)" ]; then
    echo "a unit was enabled" >&2; exit 1
fi
[ -f /usr/share/shipyard/apparmor/shipyard ] || { echo "no AppArmor profile shipped" >&2; exit 1; }
# (By the package'"'"'s list: a minimised image keeps no /usr/share/doc.)
$listing | grep -qx /usr/share/doc/shipyard/LICENSE || { echo "no LICENSE in the package" >&2; exit 1; }
shipyard version | grep "Git commit"
shipyard doctor >/tmp/doctor 2>&1 || true
grep -q "^host:" /tmp/doctor || { cat /tmp/doctor >&2; exit 1; }
said=$(shipyard run --no-image --network none echo from-a-package)
[ "$said" = from-a-package ] || { echo "the run said: $said" >&2; exit 1; }
echo "== installed and ran"
'

echo "== $deb in Ubuntu 24.04"
timeout 900 docker run --rm --privileged --volume "$dir:/packages:ro" "$UBUNTU_IMAGE" bash -c "
    set -euo pipefail
    export DEBIAN_FRONTEND=noninteractive
    # Every directory the package would make, enterable by everyone (root's
    # checks below can't see a mode that keeps users out).
    if dpkg-deb -c /packages/$deb | grep '^d' | grep -v '^drwxr-xr-x'; then
        echo 'a directory in the package is not drwxr-xr-x' >&2; exit 1
    fi
    apt-get -o Acquire::Retries=10 update -qq
    # AppArmor 4 (as on an Ubuntu host), so the profile is placed.
    apt-get install -y -qq apparmor >/dev/null
    apt-get install -y -qq /packages/$deb >/dev/null
    dpkg -s uidmap passt >/dev/null || { echo 'its recommends not installed' >&2; exit 1; }
    head -n 1 /etc/apparmor.d/shipyard | grep -q 'Placed by the shipyard package' ||
        { echo 'the profile was not placed' >&2; exit 1; }
    listing='dpkg -L shipyard'
    $checks
    # An upgrade's order (the old package's pre-removal with \"upgrade\",
    # then the new one's install): the profile stays placed.
    apt-get install -y -qq --reinstall /packages/$deb >/dev/null
    head -n 1 /etc/apparmor.d/shipyard | grep -q 'Placed by the shipyard package' ||
        { echo 'an upgrade lost the profile' >&2; exit 1; }
    apt-get remove -y -qq shipyard >/dev/null
    if [ -e /usr/bin/shipyard ] || [ -e /usr/bin/shipyard-runtime ] || [ -e /etc/apparmor.d/shipyard ]; then
        echo 'removal left a binary or the profile' >&2; exit 1
    fi
    # A profile it didn't place (a manual install's) is never replaced or removed.
    echo '# my own' >/etc/apparmor.d/shipyard
    apt-get install -y -qq /packages/$deb >/dev/null 2>&1
    apt-get remove -y -qq shipyard >/dev/null
    [ \"\$(cat /etc/apparmor.d/shipyard)\" = '# my own' ] || { echo 'a manual profile was touched' >&2; exit 1; }
    echo '== removed, and a manual profile kept'
"

echo "== $rpm in Fedora 42"
timeout 900 docker run --rm --privileged --volume "$dir:/packages:ro" "$FEDORA_IMAGE" bash -c "
    set -euo pipefail
    dnf install -y -q --setopt=retries=10 /packages/$rpm >/dev/null
    rpm -q passt >/dev/null || { echo 'its recommends not installed' >&2; exit 1; }
    listing='rpm -ql shipyard'
    $checks
    dnf remove -y -q shipyard >/dev/null
    if [ -e /usr/bin/shipyard ] || [ -e /usr/bin/shipyard-runtime ]; then
        echo 'removal left a binary' >&2; exit 1
    fi
    echo '== removed'
"

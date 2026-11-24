# Sourced, as root in the pinned image or the test image (Debian trixie):
# points apt at Debian's archive as it was at DEBIAN_SNAPSHOT
# (scripts/release/build.env), the one source (F211: every version stays on
# snapshot.debian.org; the live archive drops one at a point release), and
# defines `apt` (a short timeout and many tries: the service is slow) and
# `apt_install PACKAGE=VERSION…`, which says the snapshot when it fails.
# The indexes' signatures are checked with the image's Debian keys; only
# their expiry is waived, as a snapshot's indexes are expired by design.
: "${DEBIAN_SNAPSHOT:?}"
rm -f /etc/apt/sources.list /etc/apt/sources.list.d/*
echo "deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/$DEBIAN_SNAPSHOT trixie main" \
    >/etc/apt/sources.list.d/snapshot.list
apt=(apt-get -o Acquire::ForceIPv4=true -o Acquire::Retries=10 -o Acquire::http::Timeout=30)
apt_install() {
    if ! { "${apt[@]}" update -qq --error-on=any &&
        "${apt[@]}" install -y -qq --no-install-recommends "$@" >/dev/null; }; then
        echo "couldn't install $* from snapshot.debian.org at $DEBIAN_SNAPSHOT (scripts/release/build.env)" >&2
        return 1
    fi
}

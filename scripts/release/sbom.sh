#!/usr/bin/env bash
# The release's software bills of materials (step 15.5a, FEATURES §16):
# CycloneDX 1.5, one per binary and platform, every crate built into it
# (not build-only ones), as OUT/shipyard-VERSION-BINARY-PLATFORM.cdx.json.
# cargo-cyclonedx at the version build.env pins, built once into target/tools.
# (Each carries its time and a serial number: metadata about the build,
# not part of what's reproduced.) The guest's own packages are its
# alpine.installed (scripts/release/guest.sh).
#
# usage: scripts/release/sbom.sh OUT [VERSION]   (VERSION as linux.sh's)
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
out=${1:?usage: scripts/release/sbom.sh OUT [VERSION]}
mkdir -p "$out"
out=$(cd "$out" && pwd)
cd "$repo"
. scripts/release/build.env
commit=$(git rev-parse HEAD)
version=${2:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)-${commit:0:12}}

tool=target/tools/bin/cargo-cyclonedx
# (It says `cargo-cyclonedx-cyclonedx 0.5.10`: the version is its last word.)
if [ "$("$tool" cyclonedx --version 2>/dev/null | awk '{ print $NF }')" != "$CYCLONEDX_VERSION" ]; then
    env -u RUSTFLAGS cargo install --locked --force --quiet --root target/tools \
        --version "$CYCLONEDX_VERSION" cargo-cyclonedx
fi

crate=crates/shipyard-cli
for platform in linux-x86_64:x86_64-unknown-linux-musl linux-aarch64:aarch64-unknown-linux-musl \
    macos-arm64:aarch64-apple-darwin; do
    PATH=$repo/target/tools/bin:$PATH cargo cyclonedx --manifest-path "$crate/Cargo.toml" \
        --format json --spec-version 1.5 --describe binaries --no-build-deps \
        --target "${platform#*:}" --quiet
    for binary in shipyard shipyard-runtime; do
        # The Mac's release has no runtime of its own (kernel isolation is
        # the guest's).
        [ "${platform%%:*}" = macos-arm64 ] && [ "$binary" = shipyard-runtime ] && continue
        mv "$crate/${binary}_bin.cdx.json" "$out/shipyard-$version-$binary-${platform%%:*}.cdx.json"
    done
    rm -f "$crate"/*_bin.cdx.json
done
ls "$out"/*.cdx.json

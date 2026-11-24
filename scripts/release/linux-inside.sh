#!/usr/bin/env bash
# The inside of scripts/release/linux.sh: run in the pinned image, the
# commit at /build/shipyard (read-only), the tarball to /out. Every path the
# build sees is the same in every run, and the ones that would name the
# machine are remapped, so the bytes depend on the commit alone.
set -euo pipefail
: "${VERSION:?}" "${ARCH:?}" "${MUSL_TOOLS:?}" "${DEBIAN_SNAPSHOT:?}" "${SOURCE_DATE_EPOCH:?}" "${SHIPYARD_REVISION_GIVEN:?}"
target=$ARCH-unknown-linux-musl
export CARGO_TARGET_DIR=/tmp/target CARGO_INCREMENTAL=0

# musl's tools from Debian's archive as it was at DEBIAN_SNAPSHOT (F211).
. scripts/release/snapshot-apt.sh
apt_install "musl-tools=$MUSL_TOOLS"
rustup target add "$target" >/dev/null

# The workspace, cargo's and rustup's homes, in the binaries' panic messages
# and debug paths; `ring`'s C the same way through its compiler.
remap="--remap-path-prefix=/build/shipyard=/shipyard"
remap+=" --remap-path-prefix=$CARGO_HOME=/cargo --remap-path-prefix=$RUSTUP_HOME=/rustup"
export RUSTFLAGS="$remap"
cc_var=CC_${target//-/_}
cflags_var=CFLAGS_${target//-/_}
export "$cc_var=musl-gcc" "$cflags_var=-ffile-prefix-map=/build/shipyard=/shipyard -ffile-prefix-map=$CARGO_HOME=/cargo"

cargo build --locked --release --target "$target" -p shipyard-cli \
    --bin shipyard --bin shipyard-runtime
built=$CARGO_TARGET_DIR/$target/release

for binary in shipyard shipyard-runtime; do
    # Static: no program interpreter to load it. (Read whole first: a grep
    # that stops early under pipefail would fail its writer, and the check.)
    headers=$(readelf --program-headers "$built/$binary")
    if grep -q INTERP <<<"$headers"; then
        echo "$binary isn't static" >&2
        exit 1
    fi
done
said=$("$built/shipyard" version)
echo "$said"
grep -q "$SHIPYARD_REVISION_GIVEN" <<<"$said" || {
    echo "shipyard version doesn't name $SHIPYARD_REVISION_GIVEN" >&2
    exit 1
}

# Packed with nothing of this run in it: names sorted, one time, root's.
name=shipyard-$VERSION-linux-$ARCH
stage=/tmp/$name
mkdir -p "$stage/contrib"
install -m 0755 "$built/shipyard" "$built/shipyard-runtime" "$stage/"
install -m 0644 LICENSE README.md "$stage/"
cp -R contrib/systemd contrib/apparmor "$stage/contrib/"
tar --create --directory=/tmp --sort=name --mtime="@$SOURCE_DATE_EPOCH" \
    --owner=0 --group=0 --numeric-owner --mode='u=rwX,go=rX' \
    --format=gnu "$name" | gzip -n -9 >"/out/$name.tar.gz"
(cd /out && sha256sum "$name.tar.gz")

#!/usr/bin/env bash
# Builds the Mac's release (step 15.5a), on an Apple Silicon Mac: `shipyard`
# for aarch64-apple-darwin (macOS 13 and later), signed ad hoc with its
# virtualization entitlement, packed with the guest it boots as
# OUT/shipyard-VERSION-macos-arm64.tar.gz (`bin/shipyard` and `guest/`: found
# beside the binary wherever it's unpacked or installed, F178); and the
# desktop app, signed ad hoc, as OUT/Shipyard-VERSION-macos-arm64.app.zip.
# Ad hoc, never the building machine's own identity: not notarized (D15-2;
# the install docs say how to allow it once). The guest is a Linux build's
# (scripts/release/guest.sh), given as its tarball.
#
# usage: scripts/release/macos.sh OUT GUEST-TARBALL [VERSION]   (VERSION as linux.sh's)
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
usage="usage: scripts/release/macos.sh OUT GUEST-TARBALL [VERSION]"
out=${1:?$usage}
guest=${2:?$usage}
[ -f "$guest" ] || { echo "no guest tarball at $guest" >&2; exit 1; }
guest=$(cd "$(dirname "$guest")" && pwd)/$(basename "$guest")
mkdir -p "$out"
out=$(cd "$out" && pwd)
cd "$repo"
if [ -n "$(git status --porcelain)" ]; then
    echo "the tree has changes: a release is built from a commit" >&2
    exit 1
fi
commit=$(git rev-parse HEAD)
version=${3:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)-${commit:0:12}}
export SOURCE_DATE_EPOCH
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)
export SHIPYARD_SIGN_IDENTITY=- CARGO_INCREMENTAL=0
export RUSTFLAGS="--remap-path-prefix=$repo=/shipyard --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo --remap-path-prefix=${RUSTUP_HOME:-$HOME/.rustup}=/rustup"

target=aarch64-apple-darwin
cargo build --locked --release --target "$target" -p shipyard-cli --bin shipyard
binary=target/$target/release/shipyard
scripts/macos/sign.sh "$binary"
codesign --verify --strict "$binary"
# Its revision baked in (never `shipyard version` here: on a Mac that asks
# the VM, booting one).
grep -aq "${commit:0:12}" "$binary" || {
    echo "$binary doesn't carry its revision ${commit:0:12}" >&2
    exit 1
}

name=shipyard-$version-macos-arm64
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name/bin" "$stage/$name/guest"
cp "$binary" "$stage/$name/bin/shipyard"
# The guest's three files, from its own tarball's one directory: the same
# commit's, or the Mac would call its guest another build.
tar -xzf "$guest" -C "$stage/$name/guest" --strip-components=1
for file in Image initramfs.cpio.gz manifest.json; do
    [ -f "$stage/$name/guest/$file" ] || { echo "the guest tarball has no $file" >&2; exit 1; }
done
grep -q "\"${commit:0:12}\"" "$stage/$name/guest/manifest.json" || {
    echo "the guest names another commit than ${commit:0:12}" >&2
    exit 1
}
cp LICENSE README.md "$stage/$name/"
# bsdtar has no --mtime: every entry given the commit's time first.
stamp=$(TZ=UTC date -r "$SOURCE_DATE_EPOCH" +%Y%m%d%H%M.%S)
TZ=UTC find "$stage/$name" -exec touch -h -t "$stamp" {} +
chmod -R u=rwX,go=rX "$stage/$name"
# Names in the order given, sorted; one owner; none of this Mac's extended
# attributes (its provenance, ACLs) in it.
(cd "$stage" && find "$name" -print | LC_ALL=C sort |
    COPYFILE_DISABLE=1 tar --create --no-recursion --files-from - --format ustar \
        --no-xattrs --no-acls --no-mac-metadata --uid 0 --gid 0 --uname root --gname root \
        --options gzip:!timestamp --gzip --file "$out/$name.tar.gz")

# The desktop app: its page, then its shell, built and signed whole.
(cd desktop/ui && npm ci --no-audit --no-fund && npm run tauri -- build --bundles app)
app=desktop/target/release/bundle/macos/Shipyard.app
scripts/macos/sign.sh --bundle "$app"
codesign --verify --deep --strict "$app"
ditto -c -k --keepParent --norsrc --noextattr --noqtn --noacl "$app" \
    "$out/Shipyard-$version-macos-arm64.app.zip"
(cd "$out" && shasum -a 256 "$name.tar.gz" "Shipyard-$version-macos-arm64.app.zip")

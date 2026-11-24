#!/usr/bin/env bash
# A release's third-party licence notices (step 15.6b3b): everything built
# into what it ships, each licence's text once with what uses it, made from
# the commit's own lockfiles by cargo-about (the version build.env pins,
# built once into target/tools; its settings and template in
# scripts/release/about/), and what no lockfile names (about/extra.py):
#   binaries  OUT/THIRD-PARTY-LICENSES.html: shipyard-cli's crates (both
#             binaries, Linux and Mac), Rust's standard library (rust-src of
#             the pinned toolchain), and musl (linked into the Linux ones);
#   app       OUT/THIRD-PARTY-LICENSES-APP.html: the desktop app's crates,
#             Rust's standard library, and every npm package its page holds
#             (its bundle's sourcemaps, its production dependencies: fonts).
# A crate under a licence the settings don't accept fails it (as cargo deny),
# and so does a crate built in that the page doesn't name.
#
# usage: scripts/release/notices.sh OUT [binaries|app]   (both by default)
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
usage="usage: scripts/release/notices.sh OUT [binaries|app]"
out=${1:?$usage}
parts=${2:-binaries app}
mkdir -p "$out"
out=$(cd "$out" && pwd)
cd "$repo"
. scripts/release/build.env

tool=target/tools/bin/cargo-about
if [ "$("$tool" --version 2>/dev/null | awk '{ print $NF }')" != "$CARGO_ABOUT_VERSION" ]; then
    env -u RUSTFLAGS cargo install --locked --force --quiet --root target/tools \
        --version "$CARGO_ABOUT_VERSION" --features cli cargo-about
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# MANIFEST OUT [--target TARGET…]: the shipped targets (about.toml's) unless
# told fewer.
about() {
    "$tool" generate --locked --fail -c scripts/release/about/about.toml \
        --manifest-path "$1" scripts/release/about/about.hbs -o "$2" "${@:3}"
}

# Every crate built into MANIFEST's package for each TARGET, but Shipyard's
# own, is named in HTML (as the template prints it: `<li>NAME VERSION` then
# `</li>` or ` (`): a notice never misses one. (Not what only a procedural
# macro uses: that runs at the build, and isn't shipped.)
named() {
    local manifest=$1 html=$2 missing
    shift 2
    missing=$(for target in "$@"; do
        cargo tree --locked --manifest-path "$manifest" -e normal,no-proc-macro \
            --prefix none --target "$target" -f '{p}'
    done | sed 's/ (.*//' | sort -u | grep -v -e '^shipyard' |
        while read -r name version; do
            grep -qF -e "<li>$name ${version#v}</li>" -e "<li>$name ${version#v} (" "$html" ||
                echo "$name $version"
        done)
    if [ -n "$missing" ]; then
        echo "$html misses: $missing" >&2
        return 1
    fi
}

# Rust's standard library as the pinned toolchain has it (its source holds
# the crates it vendors, each with its licence).
rustup component add rust-src >/dev/null 2>&1
library="$(rustc --print sysroot)/lib/rustlib/src/rust/library"
[ -d "$library/vendor" ] || { echo "no rust-src at $library" >&2; exit 1; }
# The crates std is built from for TARGET… (`NAME VERSION` lines). Its
# manifest asks for a nightly cargo feature: RUSTC_BOOTSTRAP lets this
# cargo read it, for metadata alone (nothing is compiled).
std_crates() {
    for target in "$@"; do
        RUSTC_BOOTSTRAP=1 cargo tree --manifest-path "$library/Cargo.toml" -p std \
            -e normal,no-proc-macro --prefix none --target "$target" -f '{p}'
    done | sed 's/ (.*//' | sort -u
}

for part in $parts; do
    case $part in
    binaries)
        about crates/shipyard-cli/Cargo.toml "$work/binaries.html"
        named crates/shipyard-cli/Cargo.toml "$work/binaries.html" \
            x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin
        # musl's copyright, from the musl a release links (the snapshot's).
        timeout 900 docker run --rm --env DEBIAN_SNAPSHOT="$DEBIAN_SNAPSHOT" \
            --env MUSL_TOOLS="$MUSL_TOOLS" --volume "$repo:/src:ro" "$BUILD_IMAGE" bash -c '
                set -euo pipefail
                . /src/scripts/release/snapshot-apt.sh
                apt_install "musl-dev=$MUSL_TOOLS" >&2
                cat /usr/share/doc/musl-dev/copyright' >"$work/musl"
        std_crates x86_64-unknown-linux-musl aarch64-unknown-linux-musl aarch64-apple-darwin \
            >"$work/std"
        python3 scripts/release/about/extra.py "$work/binaries.html" \
            --std "$library" "$work/std" --musl "$work/musl" >"$out/THIRD-PARTY-LICENSES.html"
        ;;
    app)
        # (The app ships for the Mac alone.)
        about desktop/src-tauri/Cargo.toml "$work/app.html" --target aarch64-apple-darwin
        named desktop/src-tauri/Cargo.toml "$work/app.html" aarch64-apple-darwin
        # The page as it's bundled, with sourcemaps (to scratch: the app's
        # own build is untouched): installed from its lockfile, no install
        # scripts (as the app's build does).
        (cd desktop/ui && npm ci --ignore-scripts --no-audit --no-fund --loglevel=error &&
            npx --no-install vite build --sourcemap --emptyOutDir --outDir "$work/page" \
                --logLevel error)
        std_crates aarch64-apple-darwin >"$work/std-app"
        python3 scripts/release/about/extra.py "$work/app.html" \
            --std "$library" "$work/std-app" --npm desktop/ui "$work/page" \
            >"$out/THIRD-PARTY-LICENSES-APP.html"
        ;;
    *)
        echo "$usage" >&2
        exit 1
        ;;
    esac
done
ls -la "$out"/THIRD-PARTY-LICENSES*.html

#!/usr/bin/env bash
# The inside of scripts/release/packages.sh: run in the pinned image, the
# staged tarballs and nfpm.yaml at /work (its `list`: a line a package,
# STAGE ARCH VERSION), the packages to /out.
set -euo pipefail
: "${NFPM_VERSION:?}" "${NFPM_SHA256_X86_64:?}" "${NFPM_SHA256_ARM64:?}" "${SOURCE_DATE_EPOCH:?}"

# nfpm, its release tarball checked against the pin.
case $(uname -m) in
x86_64) asset=x86_64 sum=$NFPM_SHA256_X86_64 ;;
aarch64) asset=arm64 sum=$NFPM_SHA256_ARM64 ;;
*)
    echo "no nfpm for $(uname -m)" >&2
    exit 1
    ;;
esac
tarball=nfpm_${NFPM_VERSION}_Linux_$asset.tar.gz
curl -fsSL --retry 10 --retry-all-errors --connect-timeout 10 -o "/tmp/$tarball" \
    "https://github.com/goreleaser/nfpm/releases/download/v$NFPM_VERSION/$tarball"
echo "$sum  /tmp/$tarball" | sha256sum -c --quiet
tar -xzf "/tmp/$tarball" -C /usr/local/bin nfpm

# Every file and the packages' own times the commit's: none of this run's.
find /work -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
mtime=$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%dT%H:%M:%SZ)

while read -r stage package_arch version; do
    stage=/work/${stage##*/}
    # nfpm expands no variables in its files list: filled in here, one
    # description a package, and none left outside the comments.
    sed -e "s|\${STAGE}|$stage|g" -e "s|\${SCRIPTS}|/work|g" \
        -e "s|\${PACKAGE_ARCH}|$package_arch|g" -e "s|\${VERSION}|$version|g" \
        -e "s|\${MTIME}|$mtime|g" \
        /work/nfpm.yaml >/tmp/nfpm.yaml
    if grep -v '^ *#' /tmp/nfpm.yaml | grep -qF '${'; then
        echo "nfpm.yaml names a value packages.sh doesn't fill in" >&2
        exit 1
    fi
    for packager in deb rpm; do
        nfpm package --config /tmp/nfpm.yaml --packager "$packager" --target /out/
    done
done </work/list

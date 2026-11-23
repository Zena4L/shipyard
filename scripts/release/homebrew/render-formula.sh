#!/usr/bin/env bash
# The Homebrew formula for a release (step 15.5b): shipyard.rb.in with the
# Mac tarball's URL, version and SHA-256, on standard output.
#
# usage: scripts/release/homebrew/render-formula.sh VERSION TARBALL [URL]
#   TARBALL: the release's shipyard-VERSION-macos-arm64.tar.gz, read for its
#   checksum; URL: where brew fetches it (default: the release's asset).
set -euo pipefail
usage="usage: scripts/release/homebrew/render-formula.sh VERSION TARBALL [URL]"
version=${1:?$usage}
tarball=${2:?$usage}
url=${3:-https://github.com/Zena4L/shipyard/releases/download/v$version/shipyard-$version-macos-arm64.tar.gz}
# Both go into sed and the formula as they are: plain characters alone.
case $version in
"" | *[!0-9A-Za-z.~-]*)
    echo "not a version: $version" >&2
    exit 1
    ;;
esac
case $url in
https://* | file:///*) ;;
*)
    echo "not an https:// or file:/// URL: $url" >&2
    exit 1
    ;;
esac
case $url in
*[!0-9A-Za-z:/._~%+-]*)
    echo "a URL of plain characters only: $url" >&2
    exit 1
    ;;
esac
if [ "$(basename "$tarball")" != "shipyard-$version-macos-arm64.tar.gz" ]; then
    echo "$tarball isn't shipyard-$version-macos-arm64.tar.gz" >&2
    exit 1
fi
sum=$(shasum -a 256 "$tarball" | cut -d' ' -f1)
sed -e "s|@URL@|$url|" -e "s|@VERSION@|$version|" -e "s|@SHA256@|$sum|" \
    "$(dirname "$0")/shipyard.rb.in"

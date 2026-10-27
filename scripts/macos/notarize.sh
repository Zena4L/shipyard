#!/usr/bin/env bash
# Signs a release's binaries with a Developer ID and has Apple notarize them
# (step 13.7, D13-3: scripted now, run at release, stage 15; never run in this
# repository's checks). A local build is signed by scripts/macos/sign.sh.
#
# usage: scripts/macos/notarize.sh FILE…
#   SHIPYARD_DEVELOPER_ID   the "Developer ID Application: …" identity
#   SHIPYARD_NOTARY_PROFILE a notarytool keychain profile
#                           (`xcrun notarytool store-credentials NAME`)
# Each file is signed with the hardened runtime, a secure timestamp and the
# virtualization entitlement, checked, zipped together and sent; the script
# waits for Apple's answer and prints its log. A bare binary can't be
# stapled: Gatekeeper asks Apple online the first time it runs one.
set -euo pipefail
identity=${SHIPYARD_DEVELOPER_ID:?SHIPYARD_DEVELOPER_ID names the Developer ID identity}
profile=${SHIPYARD_NOTARY_PROFILE:?SHIPYARD_NOTARY_PROFILE names a notarytool profile}
[ "$#" -gt 0 ] || { echo "usage: $0 FILE…" >&2; exit 2; }
repo=$(cd "$(dirname "$0")/../.." && pwd)
entitlements=$repo/contrib/macos/shipyard.entitlements
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# Each file by its own name in the archive: two of one name would overwrite.
names=$(for file in "$@"; do basename "$file"; done | sort)
[ "$(uniq -d <<<"$names")" = "" ] || { echo "notarize.sh: two files share a name" >&2; exit 2; }
for file in "$@"; do
  codesign --force --sign "$identity" --options runtime --timestamp \
    --entitlements "$entitlements" "$file"
  codesign --verify --strict --verbose=2 "$file"
done
# (ditto zips one source: the files in a directory of their own.)
mkdir "$work/shipyard"
cp "$@" "$work/shipyard/"
zip=$work/shipyard.zip
/usr/bin/ditto -c -k --keepParent "$work/shipyard" "$zip"
# (Its status kept: a refusal's log is fetched all the same.)
submitted=0
out=$(xcrun notarytool submit "$zip" --keychain-profile "$profile" --wait --output-format json) ||
  submitted=$?
echo "$out"
id=$(sed -n 's/.*"id" *: *"\([^"]*\)".*/\1/p' <<<"$out" | head -1)
[ -n "$id" ] && xcrun notarytool log "$id" --keychain-profile "$profile"
if [ "$submitted" -ne 0 ] || ! grep -q '"status" *: *"Accepted"' <<<"$out"; then
  echo "notarize.sh: not accepted" >&2
  exit 1
fi

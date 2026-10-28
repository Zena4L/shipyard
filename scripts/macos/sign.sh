#!/usr/bin/env bash
# Signs Shipyard's binaries with the virtualization entitlement
# (contrib/macos/shipyard.entitlements): without it, macOS refuses them a VM
# (step 10.3). The linker's own signature carries no entitlements, so every
# new build needs this again. A file that fails to sign fails the script.
#
# Signed with an identity (step 13.7, D13-3), every rebuild keeps one code
# identity, so macOS's folder permissions are asked for once: the one
# SHIPYARD_SIGN_IDENTITY names (a name or a SHA-1), else "Shipyard Local
# Signing" if the keychain has it (scripts/macos/make-signing-identity.sh),
# by its SHA-1 (two by one name would be ambiguous); else ad hoc, as before
# (each rebuild a new identity). One found so that can't sign (a locked
# keychain, an SSH session) is said, and the build signed ad hoc: a build
# never fails for it. No hardened runtime: a debugger must attach to a local
# build (scripts/macos/notarize.sh adds it for a release).
# SHIPYARD_SIGN_KEYCHAIN names the keychain to look in (the tests' own);
# SHIPYARD_SIGN_VERBOSE=1 says which identity signed.
#
# usage: scripts/macos/sign.sh [FILE…]
#        scripts/macos/sign.sh --bundle APP…
#   --bundle: the desktop app's bundles (step 14.7), signed whole, by the same
#   identity rule but with no entitlements (the app runs `shipyard`; it
#   never boots the VM itself).
#   with no FILE: `shipyard` and `shipyard-runtime` in target/debug and
#   target/release (and their aarch64-apple-darwin twins), and the test
#   binaries cargo made beside them (deps/shipyard*): only Mach-O files.
set -euo pipefail
# The repository by this script's place, not git: a copy without .git (a
# mutant's) still signs.
repo=$(cd "$(dirname "$0")/../.." && pwd)
entitled=(--entitlements "$repo/contrib/macos/shipyard.entitlements")
if [ "${1:-}" = --bundle ]; then
  shift
  entitled=(--deep)
  [ "$#" -gt 0 ] || { echo "sign.sh: --bundle needs an app" >&2; exit 2; }
fi
if [ "$#" -eq 0 ]; then
  set --
  for dir in "$repo"/target/{debug,release} "$repo"/target/aarch64-apple-darwin/{debug,release}; do
    [ -d "$dir" ] || continue
    for file in "$dir"/shipyard "$dir"/shipyard-runtime "$dir"/deps/shipyard*; do
      [ -f "$file" ] && [ -x "$file" ] || continue
      # (Never a Linux build that ended up here.)
      file -b "$file" | grep -q '^Mach-O' && set -- "$@" "$file"
    done
  done
fi
keychain=()
[ -n "${SHIPYARD_SIGN_KEYCHAIN:-}" ] && keychain=(--keychain "$SHIPYARD_SIGN_KEYCHAIN")
identity=${SHIPYARD_SIGN_IDENTITY:-}
found_here=
if [ -z "$identity" ]; then
  # (Into a variable first: under pipefail a `grep` that ends early could
  # fail its writer, and the identity be missed in silence.)
  found=$(security find-identity -p codesigning ${SHIPYARD_SIGN_KEYCHAIN:+"$SHIPYARD_SIGN_KEYCHAIN"} 2>/dev/null || true)
  identity=$(awk '/"Shipyard Local Signing"/ {print $2; exit}' <<<"$found")
  [ -n "$identity" ] && found_here=1
fi
sign() {
  codesign --force "$@" "${entitled[@]}" "$file" 2>&1
}
[ "${SHIPYARD_SIGN_VERBOSE:-}" = 1 ] && echo "sign.sh: signing with ${identity:-an ad-hoc signature}" >&2
for file in "$@"; do
  if [ -n "$identity" ]; then
    if out=$(sign --sign "$identity" ${keychain[@]+"${keychain[@]}"} --timestamp=none); then
      continue
    fi
    if [ -z "$found_here" ]; then
      echo "sign.sh: $file: $out" >&2
      exit 1
    fi
    # The local identity can't sign now: said once, and ad hoc from here.
    echo "sign.sh: the local identity can't sign ($out): signed ad hoc" >&2
    identity=
  fi
  if ! out=$(sign --sign -); then
    echo "sign.sh: $file: $out" >&2
    exit 1
  fi
done

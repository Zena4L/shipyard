#!/usr/bin/env bash
# Signs Shipyard's binaries (ad hoc) with the virtualization entitlement
# (contrib/macos/shipyard.entitlements): without it, macOS refuses them a VM
# (step 10.3). The linker's own signature carries no entitlements, so every
# new build needs this again. A file that fails to sign fails the script.
#
# usage: scripts/macos/sign.sh [FILE…]
#   with no FILE: `shipyard` and `shipyard-runtime` in target/debug and
#   target/release (and their aarch64-apple-darwin twins), and the test
#   binaries cargo made beside them (deps/shipyard*): only Mach-O files.
set -euo pipefail
# The repository by this script's place, not git: a copy without .git (a
# mutant's) still signs.
repo=$(cd "$(dirname "$0")/../.." && pwd)
entitlements=$repo/contrib/macos/shipyard.entitlements
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
for file in "$@"; do
  if ! out=$(codesign --force --sign - --entitlements "$entitlements" "$file" 2>&1); then
    echo "sign.sh: $file: $out" >&2
    exit 1
  fi
done

#!/usr/bin/env bash
# Prints the fuzz targets, one per line: the `[[bin]]` names in
# fuzz/Cargo.toml, the one list (test-linux.sh --fuzz and CI's `quality`
# workflow read it here, so a new target is fuzzed without being listed
# twice).
#
#   scripts/fuzz-targets.sh
set -euo pipefail
cd "$(dirname "$0")/.."
awk '
  /^\[\[bin\]\]/ { in_bin = 1; next }
  /^\[/ { in_bin = 0 }
  in_bin && /^name *= *"/ {
    sub(/^name *= *"/, ""); sub(/".*$/, ""); print
  }
' fuzz/Cargo.toml

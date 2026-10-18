#!/usr/bin/env bash
# Cargo's runner on macOS (.cargo/config.toml): signs the binary it's about
# to run (`cargo run`, each test binary) with the virtualization
# entitlement, then runs it. See scripts/macos/sign.sh.
set -euo pipefail
"$(dirname "$0")/sign.sh" "$1"
exec "$@"

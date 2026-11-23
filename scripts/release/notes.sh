#!/usr/bin/env bash
# A release's notes (step 15.5a): VERSION's section of CHANGELOG.md (its
# `## VERSION - DATE` heading to the next `## `), then how to check what's
# downloaded. No section for it, or one still `unreleased` (the release
# skill dates it first): a failure, never a release without its notes.
#
# usage: scripts/release/notes.sh VERSION
set -euo pipefail
version=${1:?usage: scripts/release/notes.sh VERSION}
cd "$(dirname "$0")/../.."
heading=$(grep -m1 "^## $version " CHANGELOG.md || true)
if ! grep -qE "^## $version - [0-9]{4}-[0-9]{2}-[0-9]{2}\$" <<<"$heading"; then
    echo "CHANGELOG.md's heading for $version isn't \`## $version - YYYY-MM-DD\` (it's \`${heading:-none}\`)" >&2
    exit 1
fi
section=$(awk -v heading="## $version " '
    index($0, heading) == 1 { found = 1; next }
    found && /^## / { exit }
    found { print }
' CHANGELOG.md)
if [ -z "$(tr -d '[:space:]' <<<"$section")" ]; then
    echo "CHANGELOG.md has no section for $version (a heading \`## $version - DATE\`)" >&2
    exit 1
fi
printf '%s\n' "$section"
sed "s/VERSION/$version/" <<'EOF'

## Checking a download

Each file is attested by the release workflow, from this tag:

```sh
gh attestation verify FILE --repo Zena4L/shipyard \
    --signer-workflow Zena4L/shipyard/.github/workflows/release.yml --source-ref refs/tags/vVERSION
sha256sum -c SHA256SUMS --ignore-missing      # or, on an older Mac: shasum -a 256 -c …
```

The Linux binaries and the Mac's guest are reproducible: at release time,
`scripts/release/linux.sh` and `scripts/release/guest.sh` at this tag build the same
bytes, on another runner, before it's published (`docs/DEVELOPMENT.md`).
EOF

#!/usr/bin/env bash
# Builds the docs site (D15-4: docs/user/ as it is, by mdBook, into
# target/site) and checks it: every page in docs/user/ is in SUMMARY.md and
# every entry there is a page, mdBook says no warning, no link climbs out of
# the site (a repository file a GitHub reader reaches is a 404 there), and
# every link and #anchor between the pages resolves (lychee on the built
# HTML, offline). verify.sh --mac, CI's `docs` job and the Pages workflow
# run it.
#
#   scripts/docs-site.sh
#
# Its tools at the versions docs/site/tools.env pins: one already on PATH
# at that version, else built once into target/tools (nothing global; the
# first build takes some minutes, lychee's most).
set -euo pipefail
cd "$(dirname "$0")/.."

# shellcheck source=../docs/site/tools.env
. docs/site/tools.env
SITE=target/site

# Whether `$1 --version` says exactly version `$2` (mdBook says
# `mdbook v0.5.4`, lychee `lychee 0.24.2`).
is_version() {
    local said
    said=$("$1" --version 2>/dev/null | awk 'NR == 1 { print $2 }') || return 1
    [ "${said#v}" = "$2" ]
}

# The tool `name` at `version`, from PATH or target/tools.
tool() {
    local name=$1 version=$2 found
    if found=$(command -v "$name") && is_version "$found" "$version"; then
        echo "$found"
        return
    fi
    found=target/tools/bin/$name
    if ! is_version "$found" "$version"; then
        echo "== building $name $version into target/tools" >&2
        # Not Shipyard's code: its warnings aren't ours to fail on (CI sets
        # RUSTFLAGS=-D warnings for Shipyard's). `--force`: a binary there
        # that won't run (a cache from another runner image, a killed
        # build) is built again, where cargo would call it installed.
        env -u RUSTFLAGS cargo install --locked --force --quiet \
            --root target/tools --version "$version" "$name" >&2
        is_version "$found" "$version" || {
            echo "$name $version didn't build into target/tools" >&2
            exit 1
        }
    fi
    echo "$found"
}

mdbook=$(tool mdbook "$MDBOOK_VERSION")
lychee=$(tool lychee "$LYCHEE_VERSION")

# Every page listed, and nothing listed that isn't a page.
pages=$(cd docs/user && ls -- *.md | grep -vx SUMMARY.md | sort)
listed=$(grep -oE '\]\([^)#]+\.md\)' docs/user/SUMMARY.md | sed -E 's/^\]\(//; s/\)$//' | sort -u)
if [ "$pages" != "$listed" ]; then
    echo "docs/user/SUMMARY.md doesn't list exactly the pages in docs/user/:" >&2
    diff <(echo "$pages") <(echo "$listed") | sed -n 's/^< /  not listed: /p; s/^> /  no such page: /p' >&2
    exit 1
fi

rm -rf "$SITE"
built=$("$mdbook" build docs/site 2>&1) || {
    echo "$built" >&2
    exit 1
}
if grep -E 'WARN|ERROR' <<<"$built" >&2; then
    echo "mdBook warned: fix the pages" >&2
    exit 1
fi

# The site is two levels below the repository, as docs/user/ is, so a link
# to `../../LICENSE` resolves on disk here and is a 404 once published.
if grep -rnoE '(href|src)="\.\./[^"]*"' "$SITE" --include='*.html' >&2; then
    echo "a link leaves the site: link to the repository by its GitHub URL" >&2
    exit 1
fi

# Offline: links between the pages and their anchors (those to GitHub and
# elsewhere are left out). mdBook's 404 page names the site's root
# (`/shipyard/`), which exists only once it's served.
"$lychee" --offline --include-fragments --no-progress \
    --exclude-path "$SITE/404.html" "$SITE"
echo "== the docs site: $SITE/index.html"

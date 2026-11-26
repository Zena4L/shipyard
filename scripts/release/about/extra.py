#!/usr/bin/env python3
"""What a release's notices hold beyond cargo-about's page of the crates in
a lockfile (step 15.6b3b, scripts/release/notices.sh), put into PAGE before
its </body>, on standard output:

  --std LIBRARY LIST  the Rust standard library built into every binary:
                 rust-src's library/, and LIST, the crates std is built from
                 for the shipped targets (`NAME VERSION` lines): the Rust
                 Project's own (library/'s), and each other one vendored there
                 with its licence and its own licence file's text;
  --musl FILE    musl's copyright (statically linked into the Linux binaries);
  --npm UI PAGE  the npm packages the app's page holds: each one its bundle's
                 sourcemaps name (PAGE, a `vite build --sourcemap`'s output)
                 and each production dependency (fonts are assets, not in a
                 map), with its licence and its own licence file's text.

A package or crate without a licence file fails: its notice can't be shipped.

usage: extra.py PAGE [--std LIBRARY LIST] [--musl FILE] [--npm UI BUILT]"""
import html
import json
import os
import re
import subprocess
import sys

LICENCE_FILES = ("LICENSE", "LICENCE", "COPYING", "OFL", "COPYRIGHT")

MIT = """Permission is hereby granted, free of charge, to any person obtaining a copy of
this software and associated documentation files (the "Software"), to deal in
the Software without restriction, including without limitation the rights to
use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software is furnished to do so,
subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE."""


def licence_text(path, what):
    """The first licence file in `path` (MIT's first, where there's a choice)."""
    names = sorted(
        (n for n in os.listdir(path) if n.upper().startswith(LICENCE_FILES)),
        key=lambda n: (0 if "MIT" in n.upper() else 1, n),
    )
    if not names:
        sys.exit(f"{what}: no licence file in {path}")
    with open(os.path.join(path, names[0]), encoding="utf-8", errors="replace") as file:
        return file.read()


def entry(title, text):
    return f"<h3>{html.escape(title)}</h3>\n<pre>{html.escape(text)}</pre>"


def std_section(library, listed):
    """The Rust Project's own crates, then each vendored one std uses."""
    parts = [
        "<h2>The Rust standard library</h2>",
        "<p>core, alloc, std and the rest of the Rust Project's library, built into "
        "every binary: MIT OR Apache-2.0 (Apache-2.0's text is above), copyright the "
        "Rust Project Contributors. The crates it's built from follow.</p>",
        f"<pre>Copyright (c) The Rust Project Contributors\n\n{html.escape(MIT)}</pre>",
    ]
    with open(listed, encoding="utf-8") as file:
        crates = sorted({tuple(line.split()[:2]) for line in file if line.strip()})
    vendor = os.path.join(library, "vendor")
    for crate, version in crates:
        path = os.path.join(vendor, f"{crate}-{version.lstrip('v')}")
        if not os.path.isdir(path):
            continue  # the Rust Project's own (library/'s): above
        name = f"{crate} {version.lstrip('v')}"
        with open(os.path.join(path, "Cargo.toml"), encoding="utf-8") as file:
            found = re.search(r'^license\s*=\s*"([^"]*)"', file.read(), re.M)
        licence = found.group(1) if found else "see its text"
        parts.append(entry(f"{name}: {licence}", licence_text(path, name)))
    return parts


def npm_section(ui, built):
    """Each npm package the page holds, once."""
    paths = set()
    for root, _, files in os.walk(built):
        for file in files:
            if not file.endswith(".map"):
                continue
            with open(os.path.join(root, file), encoding="utf-8") as mapped:
                sources = json.load(mapped).get("sources", [])
            for source in sources:
                found = re.search(r"node_modules/((?:@[^/]+/)?[^/]+)/", source)
                if found:
                    paths.add(os.path.join(ui, "node_modules", found.group(1)))
    listed = subprocess.run(
        ["npm", "ls", "--omit=dev", "--all", "--parseable"],
        cwd=ui, check=True, capture_output=True, text=True,
    ).stdout.splitlines()
    root = os.path.realpath(ui)
    paths.update(p for p in listed if os.path.realpath(p) != root)
    if not any("svelte" in p for p in paths):
        sys.exit("no svelte among the page's packages: was the page built with sourcemaps?")
    packages = {}
    for path in paths:
        with open(os.path.join(path, "package.json"), encoding="utf-8") as file:
            meta = json.load(file)
        what = f"{meta['name']} {meta['version']}"
        packages[what] = (meta.get("license", "see its text"), licence_text(path, what))
    parts = ["<h2>npm packages the app's page holds</h2>"]
    for what, (licence, text) in sorted(packages.items()):
        parts.append(entry(f"{what}: {licence}", text))
    return parts


def main(args):
    page, args = args[0], args[1:]
    sections = []
    while args:
        if args[0] == "--std":
            sections += std_section(args[1], args[2])
            args = args[3:]
        elif args[0] == "--musl":
            with open(args[1], encoding="utf-8", errors="replace") as file:
                sections += ["<h2>musl (the Linux binaries' C library, linked in)</h2>",
                             f"<pre>{html.escape(file.read())}</pre>"]
            args = args[2:]
        elif args[0] == "--npm":
            sections += npm_section(args[1], args[2])
            args = args[3:]
        else:
            sys.exit(__doc__)
    with open(page, encoding="utf-8") as file:
        crates = file.read()
    if crates.count("</body>") != 1:
        sys.exit(f"{page}: not one </body>")
    sys.stdout.write(crates.replace("</body>", "\n".join(sections) + "\n</body>"))


main(sys.argv[1:])

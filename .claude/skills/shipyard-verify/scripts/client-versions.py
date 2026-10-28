#!/usr/bin/env python3
"""The desktop app's client crate built alike in both workspaces (step 14.2).

`crates/shipyard-client` is tested in the main workspace and shipped from
`desktop/`'s, each with its own Cargo.lock: every crate of its dependency tree
that both resolve must be at the same version in both, or the app runs a client
the tests never ran. (Crates only one has are features Tauri turns on: fine.)
Prints each that differs, and exits non-zero if any does.
"""
import pathlib
import subprocess
import sys

REPO = pathlib.Path(__file__).resolve().parents[4]


def tree(cwd):
    out = subprocess.run(
        ["cargo", "tree", "-p", "shipyard-client", "-e", "normal", "--prefix", "none", "--target", "all"],
        cwd=cwd, check=True, capture_output=True, text=True,
    ).stdout
    versions = {}
    for line in out.splitlines():
        name, version = line.split()[:2]
        versions.setdefault(name, set()).add(version)
    return versions


main, desktop = tree(REPO), tree(REPO / "desktop")
differ = sorted(name for name in main if name in desktop and main[name] != desktop[name])
for name in differ:
    print(f"{name}: main {sorted(main[name])}, desktop {sorted(desktop[name])}")
if differ:
    print("pin desktop/Cargo.lock to the main one's (cargo update -p NAME@NEW --precise OLD in desktop/)")
    sys.exit(1)
print(f"the client's {len(main)} crates resolve alike in both workspaces")

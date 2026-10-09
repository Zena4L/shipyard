#!/usr/bin/env python3
"""Mutation checks for one Shipyard step. Copy this to the scratchpad, fill in
MUTANTS, run it (`python3 mutation-runner.py [NAME…]`), and wrap long runs in
`timeout`.

Each mutant gets a scratch copy of the repository and a cargo target dir of its
own (a shared one runs the wrong binary). `mac` mutants run their command here;
`tester`/`root` mutants run in the Linux test container, in the
`shipyard-mutation-target` volume, one subdirectory each. A mutant is CAUGHT
only by a failing test, SURVIVED only with "test result: ok" (anything else is
NO-RESULT: a build that never ran its tests is not a survivor). Each mutant's
copy and build are deleted once its result is in."""
import hashlib
import os
import shutil
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

# The repository the mutants are copies of (this file is meant to be copied
# out of it, to a scratch directory).
REPO = os.environ.get("SHIPYARD_REPO", os.path.expanduser("~/Desktop/shipyard"))
HERE = os.path.dirname(os.path.abspath(__file__))
WORK = f"{HERE}/mutants"
TARGETS = f"{HERE}/mutant-targets"

N = "crates/shipyard-network/src/"
C = "crates/shipyard-cli/src/"

# name: (where, file, the exact text (must occur once), its replacement, test command)
# where: "mac" (cargo here), "tester" or "root" (the Linux container).
MUTANTS = {
    # "example": ("root", N + "nft/ruleset.rs", "old text", "new text",
    #             "cargo test -p shipyard-cli --test firewall -- --include-ignored some_test"),
}


def image():
    digest = hashlib.sha256(open(f"{REPO}/scripts/test-image/Dockerfile", "rb").read())
    return f"shipyard-test:{digest.hexdigest()[:12]}"


def run(name):
    where, file, old, new, cmd = MUTANTS[name]
    work = f"{WORK}/{name}"
    shutil.rmtree(work, ignore_errors=True)
    shutil.copytree(REPO, work, ignore=shutil.ignore_patterns("target", ".git", "docs", "fuzz"))
    path = f"{work}/{file}"
    text = open(path).read()
    if text.count(old) != 1:
        shutil.rmtree(work, ignore_errors=True)
        return name, f"BAD-PATTERN ({text.count(old)})"
    open(path, "w").write(text.replace(old, new))
    if where == "mac":
        out = subprocess.run(["bash", "-c", cmd.replace("cargo test", "cargo test --offline")],
                             cwd=work, capture_output=True, text=True,
                             env={**os.environ, "CARGO_TARGET_DIR": f"{TARGETS}/{name}"})
    else:
        inner = f"""set -o pipefail
chown -R tester /target /usr/local/cargo /usr/local/rustup
mkdir -p /target/m-{name}; chown tester /target/m-{name}
mount -t tmpfs -o mode=1777 tmpfs /tmp
if [ {where} = tester ]; then
  setpriv --reuid tester --regid tester --init-groups env HOME=/home/tester PATH=$PATH \
CARGO_HOME=$CARGO_HOME RUSTUP_HOME=$RUSTUP_HOME CARGO_TARGET_DIR=/target/m-{name} bash -c '{cmd}'
else
  mount --make-rshared /; CARGO_TARGET_DIR=/target/m-{name} bash -c '{cmd}'
fi
status=$?
rm -rf /target/m-{name}
exit $status"""
        out = subprocess.run(
            ["timeout", "2400", "docker", "run", "--rm", "--privileged", "--init",
             "-v", f"{work}:/src:ro", "-v", "shipyard-mutation-target:/target",
             "-v", "shipyard-linux-cargo:/usr/local/cargo",
             "-v", "shipyard-linux-rustup:/usr/local/rustup",
             "-w", "/src", image(), "bash", "-c", inner],
            capture_output=True, text=True)
    log = out.stdout + out.stderr
    shutil.rmtree(f"{TARGETS}/{name}", ignore_errors=True)
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(WORK, exist_ok=True)
    open(f"{work}.log", "w").write(log)
    if out.returncode == 0:
        return name, "SURVIVED" if "test result: ok" in log else "NO-RESULT"
    caught = "test result: FAILED" in log or "panicked" in log
    return name, "CAUGHT" if caught else "NO-RESULT (see log)"


if __name__ == "__main__":
    names = sys.argv[1:] or list(MUTANTS)
    with ThreadPoolExecutor(3) as pool:
        for name, verdict in pool.map(run, names):
            print(f"{verdict:12} {name}", flush=True)

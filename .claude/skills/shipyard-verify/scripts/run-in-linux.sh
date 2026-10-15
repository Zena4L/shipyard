#!/usr/bin/env bash
# Runs a bash script in Shipyard's Linux test container, the way
# scripts/test-linux.sh runs the tests: for one-off checks, a single test file,
# a manual run, or a book chapter's "Try it" commands.
#
# usage: run-in-linux.sh tester|root SCRIPT
#
# SCRIPT is a file in SCRIPT_DIR (default: the directory SCRIPT is in), mounted
# at /scratch. The repository is /src (read-only), the build target /target
# (CARGO_TARGET_DIR), and /tmp a tmpfs (an image store must be there: the
# container's root is overlayfs). As root, `/` is made shared, and `tester`
# gets a delegated cgroup (user.slice/tester/session), as test-linux.sh gives
# it. Build the image first if its tag (the Dockerfile's hash) doesn't exist:
#   docker build --tag shipyard-test:<hash> - < scripts/test-image/Dockerfile
# Wrap the call in `timeout`: a hung container otherwise waits forever.
set -euo pipefail
who=$1
script=$2
repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
dir=${SCRIPT_DIR:-$(cd "$(dirname "$script")" && pwd)}
name=$(basename "$script")
hash=$(shasum -a 256 "$repo/scripts/test-image/Dockerfile")
image="${IMAGE:-shipyard-test:${hash:0:12}}"
exec docker run --rm --privileged --init \
    --volume "$repo:/src:ro" --volume "$dir:/scratch:ro" \
    --volume "${TARGET_VOLUME:-shipyard-linux-target}:/target" \
    --volume shipyard-linux-cargo:/usr/local/cargo \
    --volume shipyard-linux-rustup:/usr/local/rustup \
    --env CARGO_TARGET_DIR=/target --workdir /src "$image" bash -c "
set -euo pipefail
chown -R tester /target /usr/local/cargo /usr/local/rustup
mount -t tmpfs -o mode=1777 tmpfs /tmp
if [ $who = tester ]; then
  exec setpriv --reuid tester --regid tester --init-groups env HOME=/home/tester \
    PATH=\$PATH CARGO_HOME=\$CARGO_HOME RUSTUP_HOME=\$RUSTUP_HOME CARGO_TARGET_DIR=/target \
    bash /scratch/$name
fi
mount --make-rshared /
trap 'chown -R tester /target /usr/local/cargo /usr/local/rustup' EXIT
# tester's delegated cgroup, as test-linux.sh makes it (rootless --memory).
cg=/sys/fs/cgroup
if [ \"\$(cat /proc/self/cgroup)\" = '0::/' ]; then
  mkdir -p \$cg/init
  for _ in 1 2 3 4 5; do
    [ -z \"\$(cat \$cg/cgroup.procs)\" ] && break
    while read -r pid; do echo \"\$pid\" >\$cg/init/cgroup.procs 2>/dev/null || true; done <\$cg/cgroup.procs
  done
  mkdir -p \$cg/user.slice/tester/session
  for c in \$(cat \$cg/cgroup.controllers); do echo \"+\$c\" >\$cg/cgroup.subtree_control; done
  echo '+cpu +memory +pids' >\$cg/user.slice/cgroup.subtree_control
  echo '+cpu +memory +pids' >\$cg/user.slice/tester/cgroup.subtree_control
  for d in user.slice/tester user.slice/tester/session; do
    chown tester \$cg/\$d \$cg/\$d/cgroup.procs \$cg/\$d/cgroup.subtree_control \$cg/\$d/cgroup.threads
  done
fi
bash /scratch/$name"

#!/usr/bin/env bash
# Step 12.1's probe: what BuildKit needs in a container Shipyard runs.
# Run in the test container (`.claude/skills/shipyard-verify/scripts/run-in-linux.sh
# root|tester scripts/probes/buildkit.sh`), with a release `shipyard` built.
# Builds a small multi-stage Dockerfile with a cache mount through the
# `moby/buildkit` image's own `buildctl`, in a container with a user
# namespace (rootless: Shipyard's own; root: `--uidmap`), `CAP_SYS_ADMIN` in
# it, seccomp unconfined (the keyring), BuildKit's rootless mode with no
# process sandbox, its cache in a directory the namespace's root owns; then
# again after a restart (the cache kept). Findings: the stage-12 plan, 12.1.
# A record of that probe: root's map here, 100000, is the first user's own
# subordinate range on Ubuntu, Debian and Fedora (12.3's review); the engine's
# builder maps from 2100000000 (`build/builder.rs`, ROOT_FIRST). Don't copy it.
set -uo pipefail
B=${SHIPYARD:-/target/release/shipyard}
export SHIPYARD_IMAGE_STORE=${SHIPYARD_IMAGE_STORE:-/tmp/images-buildkit-probe}
timeout 300 "$B" pull moby/buildkit:latest >/dev/null
ctx=$(mktemp -d); chmod 755 "$ctx"
printf 'FROM alpine:3.20 AS base\nRUN echo built > /built\nFROM alpine:3.20\nRUN --mount=type=cache,target=/cache sh -c "date >> /cache/stamps"\nCOPY --from=base /built /built\n' >"$ctx/Dockerfile"
state=$(mktemp -d)
if [ "$(id -u)" = 0 ]; then
  maps=(--uidmap 0:100000:65536 --gidmap 0:100000:65536)
  chown 100000:100000 "$state"
else
  maps=()
fi
chmod 700 "$state"
"$B" run -d --name buildkit-probe "${maps[@]}" --cap-add SYS_ADMIN --security-opt seccomp=unconfined \
  -v "$ctx:/ctx:ro" -v "$state:/var/lib/buildkit" --entrypoint buildkitd moby/buildkit:latest \
  --oci-worker-no-process-sandbox >/dev/null
build() {
  for _ in $(seq 40); do "$B" exec buildkit-probe buildctl debug workers >/dev/null 2>&1 && break; sleep 0.5; done
  timeout 300 "$B" exec buildkit-probe buildctl build --frontend dockerfile.v0 --local context=/ctx \
    --local dockerfile=/ctx --output type=oci,dest=/tmp/out.tar 2>&1 | grep -cE '^#[0-9]+ CACHED'
}
echo "first build: $(build) steps cached"
"$B" stop buildkit-probe >/dev/null && "$B" start buildkit-probe >/dev/null
echo "after a restart: $(build) steps cached"
"$B" rm -f buildkit-probe >/dev/null
rm -rf "$ctx" "$state"

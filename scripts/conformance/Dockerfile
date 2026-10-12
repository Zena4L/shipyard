# The OCI runtime conformance suite (runtime-tools' `validation/`), on top of
# the test image: `scripts/test-linux.sh --conformance` builds it (once; it
# needs network) and runs `scripts/conformance/run.sh` in it (stage 8, H5).
#
# - runtime-tools, pinned by commit (`git rev-parse` checks it), its Go
#   modules checked by Go's checksum database, built here: the validation
#   programs (validation/*/*.t) and `runtimetest`, the static program each
#   test runs as the container's process to compare it with its config;
# - a root filesystem for the tests' bundles: they untar
#   `rootfs-$GOARCH.tar.gz` from the tree's top, and runtime-tools ships one
#   for amd64 and 386 only. Ours is a static busybox (`sh`, `touch`, `sleep`
#   and the rest), for any architecture. Its top is 0755: a test's bundle
#   directory takes that mode when it's untarred, and a container whose root
#   is another host user (a user namespace) must be able to enter it.
#
# BASE is the test image (shipyard-test:HASH).
ARG BASE
FROM ${BASE}

ARG RUNTIME_TOOLS_COMMIT=8a4db579f5c88af5a0d036fad34bddc9c1f703f3
# The commit pins the suite's sources; Go and busybox are Debian's, of the
# base image's release. Go uses its own toolchain, never one it downloads.
ENV GOTOOLCHAIN=local

RUN apt-get update \
    && apt-get install -y --no-install-recommends golang busybox-static git \
    && rm -rf /var/lib/apt/lists/*

RUN git clone --quiet https://github.com/opencontainers/runtime-tools /opt/runtime-tools \
    && cd /opt/runtime-tools \
    && git checkout --quiet "$RUNTIME_TOOLS_COMMIT" \
    && [ "$(git rev-parse HEAD)" = "$RUNTIME_TOOLS_COMMIT" ] \
    && make runtimetest validation-executables \
    && rm -rf /root/go/pkg/mod/cache

RUN set -e; \
    root=$(mktemp -d); \
    chmod 755 "$root"; \
    mkdir -p "$root/bin" "$root/etc" "$root/proc" "$root/sys" "$root/dev" "$root/tmp"; \
    cp /bin/busybox "$root/bin/busybox"; \
    for applet in $(/bin/busybox --list); do \
        [ "$applet" = busybox ] || ln -s busybox "$root/bin/$applet"; \
    done; \
    printf 'root:x:0:0:root:/root:/bin/sh\nnobody:x:65534:65534:nobody:/:/bin/sh\n' >"$root/etc/passwd"; \
    printf 'root:x:0:\nnobody:x:65534:\n' >"$root/etc/group"; \
    tar -C "$root" -czf "/opt/runtime-tools/rootfs-$(go env GOARCH).tar.gz" .; \
    rm -rf "$root"

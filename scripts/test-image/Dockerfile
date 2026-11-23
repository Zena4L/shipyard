# The image scripts/test-linux.sh runs in: the Rust image, plus what the
# user-namespace tests need (stage 4).
#
# - `uidmap`: `newuidmap` and `newgidmap`, the setuid helpers that let a user
#   map the subordinate IDs /etc/subuid gives them (`subuid(5)`);
# - `passt`: pasta, rootless containers' network (stage 7). Debian 13's: 12's
#   (2023) can't carry connections to a published port into a container;
# - `nftables`: `nft`, for the firewall tests to look at Shipyard's table and
#   to make another program's chains, and the differential tests (H5) to
#   rebuild it (Shipyard itself never runs it);
# - `iptables` (iptables-nft): to make Docker's firewall as Docker does, and
#   run the fix Shipyard prints for it (stage 8.6);
# - `iproute2` and `bind9-dnsutils`: `ip` and `dig`, for the differential
#   tests of Shipyard's own netlink and DNS server (H5, H8);
# - `docker`, Docker's own CLI alone (no daemon), static, pinned by its
#   tarball's sha256: the Docker API's acceptance tests drive Shipyard's API
#   with it (step 11.2a, D11.2-4). 24.0.9 speaks API 1.43, the one served;
# - Docker Compose v5.6.0, its CLI plugin, pinned by sha256: `shipyard
#   compose` and the compose acceptance tests run it (step 11.4a1, D27);
# - `tester`, the normal user the tests run as, with 65536 subordinate IDs
#   from 100000.
#
# Built once by test-linux.sh (which needs network then), and again only when
# this file changes.
# Pinned by digest (an index: both architectures), as the release builds and
# the macOS guest's build use it (F186): the same toolchain image everywhere.
# Rust's own version is rust-toolchain.toml's either way.
FROM rust:1.98-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546

RUN apt-get update \
    && apt-get install -y --no-install-recommends uidmap passt nftables iptables iproute2 bind9-dnsutils \
    && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    case "$(uname -m)" in \
      aarch64) sum=7e999590330a15469de20ac37051407d222ea73c71c10c05d61666d42c5922d1 ;; \
      x86_64) sum=692ecfc28333485d184f628b74c25b2894cee9495a51a5418ba60ef95bf733ca ;; \
      *) echo "no pinned docker CLI for $(uname -m)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL --connect-timeout 10 --retry 10 --retry-all-errors -o /tmp/docker.tgz "https://download.docker.com/linux/static/stable/$(uname -m)/docker-24.0.9.tgz"; \
    echo "$sum  /tmp/docker.tgz" | sha256sum -c -; \
    tar -xzf /tmp/docker.tgz -C /usr/local/bin --strip-components=1 docker/docker; \
    rm /tmp/docker.tgz; \
    docker --version

RUN set -eu; \
    case "$(uname -m)" in \
      aarch64) sum=733ec76717ceb59052a9609b9dadfb523b2df8eab57a54212872d10a58078ea2 ;; \
      x86_64) sum=40343e21ca777173e69cff5dbafeb37c6f81f3b0d57d9e597f036e95eb63e76a ;; \
      *) echo "no pinned docker compose for $(uname -m)" >&2; exit 1 ;; \
    esac; \
    mkdir -p /usr/local/lib/docker/cli-plugins; \
    # Each try goes on from what came (a slow link is cut now and then).
    n=0; until curl -fsSL -C - --connect-timeout 10 --speed-limit 1000 --speed-time 60 \
      -o /usr/local/lib/docker/cli-plugins/docker-compose \
      "https://github.com/docker/compose/releases/download/v5.6.0/docker-compose-linux-$(uname -m)"; \
      do n=$((n + 1)); [ "$n" -lt 50 ] || exit 1; sleep 2; done; \
    echo "$sum  /usr/local/lib/docker/cli-plugins/docker-compose" | sha256sum -c -; \
    chmod 755 /usr/local/lib/docker/cli-plugins/docker-compose; \
    docker compose version

# Docker's CLI 29 beside 24 (step 15.2g1, F207): it asks for API 1.44 at
# least, as Ubuntu 26.04's own does, so the API tests run a current client too.
RUN set -eu; \
    case "$(uname -m)" in \
      aarch64) sum=ce2cea8c740707d736ab356239f858d2272470d0734b5d14f20b950d1d6b952e ;; \
      x86_64) sum=e985c6dc5008b8d62d6bdf9b5894427d075b335a1391cacf24ee01a7a29a3728 ;; \
      *) echo "no pinned docker 29 CLI for $(uname -m)" >&2; exit 1 ;; \
    esac; \
    n=0; until curl -fsSL -C - --connect-timeout 10 --speed-limit 1000 --speed-time 60 \
      -o /tmp/docker29.tgz "https://download.docker.com/linux/static/stable/$(uname -m)/docker-29.4.2.tgz"; \
      do n=$((n + 1)); [ "$n" -lt 50 ] || exit 1; sleep 2; done; \
    echo "$sum  /tmp/docker29.tgz" | sha256sum -c -; \
    tar -xzf /tmp/docker29.tgz -C /tmp docker/docker; \
    install -m 0755 /tmp/docker/docker /usr/local/bin/docker29; \
    rm -rf /tmp/docker29.tgz /tmp/docker; \
    docker29 --version

# Node and Docker's devcontainer CLI (step 15.6a): the acceptance tests bring a
# devcontainer up against the API (D30). Each release tarball by its SHA-256
# (the CLI's is npm's own integrity, checked when pinned; it has no
# dependencies), nothing fetched by npm, no install scripts.
RUN set -eu; \
    case "$(uname -m)" in \
      aarch64) arch=arm64 sum=406fe3fbde1c054adff8b297c16fdb5af6659db327c1ac8acd46b56ef1af9a92 ;; \
      x86_64) arch=x64 sum=d28a96af6c38596f6d9d97c0ee531c13d238d757742387d1928de822d959e997 ;; \
      *) echo "no pinned Node for $(uname -m)" >&2; exit 1 ;; \
    esac; \
    n=0; until curl -fsSL -C - --connect-timeout 10 --speed-limit 1000 --speed-time 60 \
      -o /tmp/node.tar.xz "https://nodejs.org/dist/v26.13.0/node-v26.13.0-linux-$arch.tar.xz"; \
      do n=$((n + 1)); [ "$n" -lt 50 ] || exit 1; sleep 2; done; \
    echo "$sum  /tmp/node.tar.xz" | sha256sum -c -; \
    mkdir -p /opt/node; \
    tar -xJf /tmp/node.tar.xz -C /opt/node --strip-components=1; \
    ln -s /opt/node/bin/node /usr/local/bin/node; \
    n=0; until curl -fsSL -C - --connect-timeout 10 -o /tmp/devcontainers-cli.tgz \
      "https://registry.npmjs.org/@devcontainers/cli/-/cli-0.90.0.tgz"; \
      do n=$((n + 1)); [ "$n" -lt 50 ] || exit 1; sleep 2; done; \
    echo "b3d40fec9d274a303a29ea91a24852887cf287701bc8d340bfc1cf970542495a  /tmp/devcontainers-cli.tgz" \
      | sha256sum -c -; \
    mkdir -p /opt/devcontainers-cli; \
    tar -xzf /tmp/devcontainers-cli.tgz -C /opt/devcontainers-cli --strip-components=1; \
    printf '#!/bin/sh\nexec /usr/local/bin/node /opt/devcontainers-cli/devcontainer.js "$@"\n' \
      >/usr/local/bin/devcontainer; \
    chmod 755 /usr/local/bin/devcontainer; \
    rm -f /tmp/node.tar.xz /tmp/devcontainers-cli.tgz; \
    devcontainer --version

RUN useradd --create-home tester \
    && echo tester:100000:65536 > /etc/subuid \
    && echo tester:100000:65536 > /etc/subgid

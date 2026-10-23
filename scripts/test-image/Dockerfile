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
FROM rust:1-trixie

RUN apt-get update \
    && apt-get install -y --no-install-recommends uidmap passt nftables iptables iproute2 bind9-dnsutils \
    && rm -rf /var/lib/apt/lists/*

RUN set -eu; \
    case "$(uname -m)" in \
      aarch64) sum=7e999590330a15469de20ac37051407d222ea73c71c10c05d61666d42c5922d1 ;; \
      x86_64) sum=692ecfc28333485d184f628b74c25b2894cee9495a51a5418ba60ef95bf733ca ;; \
      *) echo "no pinned docker CLI for $(uname -m)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/docker.tgz "https://download.docker.com/linux/static/stable/$(uname -m)/docker-24.0.9.tgz"; \
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
    curl -fsSL -o /usr/local/lib/docker/cli-plugins/docker-compose \
      "https://github.com/docker/compose/releases/download/v5.6.0/docker-compose-linux-$(uname -m)"; \
    echo "$sum  /usr/local/lib/docker/cli-plugins/docker-compose" | sha256sum -c -; \
    chmod 755 /usr/local/lib/docker/cli-plugins/docker-compose; \
    docker compose version

RUN useradd --create-home tester \
    && echo tester:100000:65536 > /etc/subuid \
    && echo tester:100000:65536 > /etc/subgid

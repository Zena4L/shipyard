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
# - `iproute2` and `bind9-dnsutils`: `ip` and `dig`, for the differential
#   tests of Shipyard's own netlink and DNS server (H5, H8);
# - `tester`, the normal user the tests run as, with 65536 subordinate IDs
#   from 100000.
#
# Built once by test-linux.sh (which needs network then), and again only when
# this file changes.
FROM rust:1-trixie

RUN apt-get update \
    && apt-get install -y --no-install-recommends uidmap passt nftables iproute2 bind9-dnsutils \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home tester \
    && echo tester:100000:65536 > /etc/subuid \
    && echo tester:100000:65536 > /etc/subgid

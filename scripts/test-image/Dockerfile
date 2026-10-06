# The image scripts/test-linux.sh runs in: the Rust image, plus what the
# user-namespace tests need (stage 4).
#
# - `uidmap`: `newuidmap` and `newgidmap`, the setuid helpers that let a user
#   map the subordinate IDs /etc/subuid gives them (`subuid(5)`);
# - `tester`, the normal user the tests run as, with 65536 subordinate IDs
#   from 100000.
#
# Built once by test-linux.sh (which needs network then), and again only when
# this file changes.
FROM rust:1-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends uidmap \
    && rm -rf /var/lib/apt/lists/*

RUN useradd --create-home tester \
    && echo tester:100000:65536 > /etc/subuid \
    && echo tester:100000:65536 > /etc/subgid

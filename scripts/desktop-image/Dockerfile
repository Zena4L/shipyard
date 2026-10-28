# The desktop app's Linux image (step 14.7, D14.7-5): WebKitGTK and a
# virtual display (Xvfb), to build the real app and drive it with Tauri's
# WebDriver (`tauri-driver` over WebKitGTK's `WebKitWebDriver`). Debian
# trixie with the repository's toolchain, as the test image's base (rustup's
# own downloads fail here: their host answers on IPv6 first, and Docker's VM
# has no IPv6 route), and as the desktop VM's (scripts/macos/desktop-vm.sh,
# Debian 13): the app built here runs there. `shipyard` is built here too,
# and runs rootless as `tester` (uidmap, passt), its containers from a
# busybox image the test makes (busybox-static). Tagged by this file's hash
# (scripts/desktop-linux.sh): build it again after any change.
FROM rust:1.98-trixie

ENV CARGO_NET_RETRY=30 CARGO_HTTP_TIMEOUT=10
# (apt over IPv4, each fetch given up after 5 s and tried again, 30 times:
# where a share of new connections is dropped, a download that waits out
# apt's long default for each drop takes hours; this one, minutes.)
RUN printf '%s\n' 'Acquire::ForceIPv4 "true";' 'Acquire::Retries "30";' \
       'Acquire::http::Timeout "5";' 'Acquire::https::Timeout "5";' \
       > /etc/apt/apt.conf.d/99flaky \
    && for try in 1 2 3 4 5 6; do \
         apt-get update 2>&1 | tee /tmp/update.log; \
         grep -q '^[WE]: ' /tmp/update.log || break; \
         [ "$try" = 6 ] && { echo "apt's index never came whole" >&2; exit 1; }; \
       done \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
       libwebkit2gtk-4.1-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev \
       webkit2gtk-driver xvfb xauth dbus-x11 fonts-dejavu-core \
       uidmap passt iproute2 nftables procps busybox-static xz-utils \
    && rm -rf /var/lib/apt/lists/* \
    && rustc --version

# Node 26 (the page's `engines`), by its pinned tarball.
RUN set -eu; \
    case "$(uname -m)" in \
      aarch64) arch=arm64 sum=42aa04922591ee5d4dbc7740d452621acf28508239daa512f5ee8c47ee224fdb ;; \
      x86_64) arch=x64 sum=6170b2d474e1c626f43cff3adc118df9fe3b573258a4fc3d850c8dbc6b079bbd ;; \
      *) echo "no pinned node for $(uname -m)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL --connect-timeout 5 --retry 30 --retry-all-errors -o /tmp/node.tar.xz "https://nodejs.org/dist/v26.12.1/node-v26.12.1-linux-$arch.tar.xz"; \
    echo "$sum  /tmp/node.tar.xz" | sha256sum -c -; \
    tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1; \
    rm /tmp/node.tar.xz; \
    node --version

# Tauri's WebDriver (the W3C endpoint the UI test drives).
RUN cargo install --locked tauri-driver --version 2.1.0 \
    && rm -rf /usr/local/cargo/registry

RUN useradd --create-home tester \
    && echo tester:100000:65536 > /etc/subuid \
    && echo tester:100000:65536 > /etc/subgid

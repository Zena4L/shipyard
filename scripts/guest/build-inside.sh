#!/usr/bin/env bash
# The guest's build, inside the test container (`build.sh` runs it): /src the
# repository (read-only), /out the output, /target cargo's.
set -euo pipefail
. /src/scripts/guest/alpine.lock
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "== shipyard, static (musl)"
command -v musl-gcc >/dev/null || { apt-get update -qq && apt-get install -y -qq musl-tools >/dev/null; }
rustup target add aarch64-unknown-linux-musl >/dev/null 2>&1
CC_aarch64_unknown_linux_musl=musl-gcc cargo build --locked --release -q \
    --target aarch64-unknown-linux-musl -p shipyard-cli --bin shipyard
binary=/target/aarch64-unknown-linux-musl/release/shipyard

echo "== the minirootfs ($ALPINE_RELEASE)"
tarball=/out/cache/$MINIROOTFS
if ! echo "$MINIROOTFS_SHA256  $tarball" | sha256sum -c --status 2>/dev/null; then
  curl -fsS --retry 5 --max-time 600 -o "$tarball.part" \
      "https://dl-cdn.alpinelinux.org/alpine/$ALPINE_BRANCH/releases/aarch64/$MINIROOTFS"
  mv "$tarball.part" "$tarball"
fi
echo "$MINIROOTFS_SHA256  $tarball" | sha256sum -c --quiet
root=$work/root
mkdir "$root"
tar -xzf "$tarball" -C "$root"

echo "== packages: $PACKAGES $KERNEL_PACKAGE (the kernel from $KERNEL_BRANCH)"
cp /etc/resolv.conf "$root/etc/resolv.conf"
# Scripts not run: linux-virt's would make an initramfs of its own.
# (Three tries: a mirror that drops the index fetch once makes apk say the
# packages don't exist.)
for try in 1 2 3; do
  # shellcheck disable=SC2086
  chroot "$root" /sbin/apk add --no-cache --no-scripts --quiet \
      --repository "https://dl-cdn.alpinelinux.org/alpine/$KERNEL_BRANCH/main" \
      $PACKAGES $KERNEL_PACKAGE && break
  [ "$try" = 3 ] && exit 1
  echo "apk failed (try $try): again"; sleep 5
done
chroot "$root" /sbin/apk info -v >"$work/installed"
rm -f "$root/etc/resolv.conf"

echo "== assembling"
python3 /src/scripts/guest/assemble.py --root "$root" --binary "$binary" \
    --installed "$work/installed" --out /out --revision "$REVISION" \
    --alpine "$ALPINE_RELEASE" --minirootfs-sha256 "$MINIROOTFS_SHA256"
ls -la /out

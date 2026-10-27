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
# The mirror (SHIPYARD_GUEST_MIRROR, for a network that breaks Alpine's TLS:
# 13.2 met one that drops OpenSSL 3.5's post-quantum hello; plain HTTP is
# sound here, apk checks each package's signature with the minirootfs's own
# keys, and the minirootfs is pinned by its sha256 above).
mirror=${SHIPYARD_GUEST_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}
printf '%s\n' "$mirror/$ALPINE_BRANCH/main" "$mirror/$ALPINE_BRANCH/community" \
    >"$root/etc/apk/repositories"
# Scripts not run: linux-virt's would make an initramfs of its own.
# (Three tries: a mirror that drops the index fetch once makes apk say the
# packages don't exist.)
for try in 1 2 3; do
  # shellcheck disable=SC2086
  chroot "$root" /sbin/apk add --no-cache --no-scripts --quiet \
      --repository "$mirror/$KERNEL_BRANCH/main" \
      $PACKAGES $KERNEL_PACKAGE && break
  [ "$try" = 3 ] && exit 1
  echo "apk failed (try $try): again"; sleep 5
done
chroot "$root" /sbin/apk info -v >"$work/installed"
rm -f "$root/etc/resolv.conf"

# Every package installed, pinned (F92): its name, its version and its
# identity in apk's own database (`C:`, the SHA-1 of its signed control
# segment, which holds the SHA-256 of its data), against
# scripts/guest/alpine.installed. Alpine updating a dependency, or serving
# another build of one, fails the build, saying what differs.
# UPDATE_GUEST_LOCK=1 writes the lock from this build instead (after a
# re-pin of alpine.lock).
awk '/^P:/ {p=substr($0,3)} /^V:/ {v=substr($0,3)} /^C:/ {c=substr($0,3)}
     /^$/ {if (p != "") print p "-" v " " c; p=v=c=""}
     END {if (p != "") print p "-" v " " c}' "$root/lib/apk/db/installed" |
  LC_ALL=C sort >"$work/pinned"
lock=/src/scripts/guest/alpine.installed
if [ "${UPDATE_GUEST_LOCK:-}" = 1 ]; then
  # (The repository is read-only here: the lock goes to the output, and
  # build.sh copies it into the tree.)
  cp "$work/pinned" /out/alpine.installed
  echo "== the lock written: $(wc -l <"$work/pinned") packages"
elif ! diff -u "$lock" "$work/pinned" >"$work/pinned.diff"; then
  echo "the guest's packages differ from scripts/guest/alpine.installed (F92):" >&2
  cat "$work/pinned.diff" >&2
  echo "if that's a re-pin you meant: UPDATE_GUEST_LOCK=1 scripts/guest/build.sh" >&2
  exit 1
fi

echo "== assembling"
python3 /src/scripts/guest/assemble.py --root "$root" --binary "$binary" \
    --installed "$work/installed" --out /out --revision "$REVISION" \
    --alpine "$ALPINE_RELEASE" --minirootfs-sha256 "$MINIROOTFS_SHA256"
ls -la /out

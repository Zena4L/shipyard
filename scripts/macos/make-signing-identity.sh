#!/usr/bin/env bash
# Makes a code-signing identity of your own, "Shipyard Local Signing" (step
# 13.7, D13-3, F140): a self-signed certificate for code signing, and its key,
# in a keychain (your login keychain unless --keychain names another).
# scripts/macos/sign.sh then signs every build with it, so each rebuild keeps
# one identity: macOS asks once, not after every rebuild, before a VM that
# launchd started reads your Desktop, Documents or Downloads (its permission
# is the signature's designated requirement, which names this certificate).
#
# What it signs, only this Mac's keychain knows: it's not a Developer ID, and
# nothing it signs is for anyone else (notarization is scripts/macos/notarize.sh).
# Nothing is trusted system-wide: codesign signs with it as it is.
#
# It lets codesign use the key without asking each time (the key's partition
# list): that asks for the keychain's password once, here, at the terminal,
# so no build waits on a dialog later. Only this key's: every other key in
# the keychain is left as it is. (SHIPYARD_KEYCHAIN_PASSWORD gives it, for a
# throwaway keychain's tests only: it would show in `ps`.)
#
# Needs OpenSSL 3 or later (Homebrew's openssl@3): macOS's own openssl is
# LibreSSL, which writes no PKCS#12 macOS's `security` reads.
#
# usage: scripts/macos/make-signing-identity.sh [--keychain PATH]
#   Refuses if the keychain has one by that name already. Prints its SHA-1.
set -euo pipefail
name="Shipyard Local Signing"
usage() { echo "usage: $0 [--keychain PATH]" >&2; exit 2; }
keychain=$HOME/Library/Keychains/login.keychain-db
case "$#" in
0) ;;
2) [ "$1" = --keychain ] || usage; keychain=$2 ;;
*) usage ;;
esac

# An OpenSSL that writes what `security` reads.
openssl=
for candidate in openssl /opt/homebrew/opt/openssl@3/bin/openssl /usr/local/opt/openssl@3/bin/openssl; do
  if version=$("$candidate" version 2>/dev/null) && [[ "$version" =~ ^OpenSSL\ [3-9] ]]; then
    openssl=$candidate
    break
  fi
done
[ -n "$openssl" ] || {
  echo "make-signing-identity.sh: needs OpenSSL 3 or later (brew install openssl@3)" >&2
  exit 1
}

# (Into a variable first: under pipefail a `grep -q` that ends at its match
# could fail the writer, and the refusal be skipped.)
found=$(security find-identity -p codesigning "$keychain" 2>/dev/null || true)
if grep -q "\"$name\"" <<<"$found"; then
  echo "make-signing-identity.sh: \"$name\" is in $keychain already" >&2
  exit 1
fi

# The key and certificate only ever in a directory of this run's own,
# removed at the end whatever happens.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
chmod 700 "$work"
cat >"$work/req.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = $name
[ext]
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
basicConstraints = critical, CA:false
EOF
"$openssl" req -x509 -newkey rsa:3072 -nodes -days 3650 -quiet -config "$work/req.cnf" \
  -keyout "$work/key.pem" -out "$work/cert.pem"
# The PKCS#12's password through the environment to openssl; `security
# import` takes it only as `-P` (it opens a file in a directory only this user
# can enter, removed at the end, and is used once).
P12_PASS=$("$openssl" rand -hex 16)
export P12_PASS
# (-legacy: macOS's `security` reads only the older PKCS#12 ciphers. -name:
# the key's label in the keychain, by which its partition list is set below.)
"$openssl" pkcs12 -export -legacy -name "$name" -inkey "$work/key.pem" -in "$work/cert.pem" \
  -out "$work/id.p12" -passout env:P12_PASS
rm -f "$work/key.pem"
security import "$work/id.p12" -k "$keychain" -P "$P12_PASS" -T /usr/bin/codesign >/dev/null
unset P12_PASS
# codesign may use the key from now on without a dialog (asked here, once).
# This key alone, by its label and kind: `-s` by itself matches every key
# that can sign, a Developer ID's among them (stage 13's security review).
# (SHIPYARD_KEYCHAIN_PASSWORD is for a throwaway keychain's tests only: it
# goes on `security`'s command line, which `ps` shows.)
only_ours=(-l "$name" -t private -s)
if [ -n "${SHIPYARD_KEYCHAIN_PASSWORD:-}" ]; then
  changed=$(security set-key-partition-list -S apple-tool:,apple: "${only_ours[@]}" -k "$SHIPYARD_KEYCHAIN_PASSWORD" "$keychain")
else
  echo "the keychain's password, once, so codesign may use this key without asking:" >&2
  changed=$(security set-key-partition-list -S apple-tool:,apple: "${only_ours[@]}" "$keychain")
fi
# (`security` prints each key it changed, its label as attribute 1.)
keys=$(grep -c '0x00000001 <blob>=' <<<"$changed" || true)
echo "codesign may use it without asking: $keys key's partition list set"
sha1=$("$openssl" x509 -in "$work/cert.pem" -noout -fingerprint -sha1 | sed 's/.*=//; s/://g')
echo "made \"$name\" ($sha1) in $keychain"
echo "scripts/macos/sign.sh signs with it from now on (SHIPYARD_SIGN_IDENTITY names another)"

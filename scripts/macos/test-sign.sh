#!/usr/bin/env bash
# Checks scripts/macos/sign.sh's two ways (step 13.7), in a throwaway keychain
# (never the login one): with an identity made by make-signing-identity.sh,
# two different builds keep one designated requirement (what macOS's folder
# permissions key on: asked once) and carry the virtualization entitlement;
# ad hoc (the control), two builds' requirements differ.
# And the binary's minimum macOS is 13.0 (.cargo/config.toml's
# MACOSX_DEPLOYMENT_TARGET).
#
# usage: scripts/macos/test-sign.sh BINARY OTHER
#   BINARY and OTHER: two different Mach-O builds (as `shipyard` and
#   `shipyard-runtime`), each copied as `shipyard`: two builds of one program.
set -euo pipefail
binary=${1:?usage: $0 BINARY OTHER}
other=${2:?usage: $0 BINARY OTHER}
here=$(cd "$(dirname "$0")" && pwd)
# (The caller's choice of identity isn't this test's.)
unset SHIPYARD_SIGN_IDENTITY SHIPYARD_SIGN_VERBOSE
work=$(mktemp -d)
keychain=$work/test.keychain-db
# The user's keychain search list, put back whatever happens: a keychain made
# here must never be found by another process's signing.
searched=()
while IFS= read -r line; do
  line=${line#*\"}
  searched+=("${line%\"*}")
done < <(security list-keychains -d user)
cleanup() {
  security delete-keychain "$keychain" 2>/dev/null || true
  security list-keychains -d user -s ${searched[@]+"${searched[@]}"}
  rm -rf "$work"
}
trap cleanup EXIT
fail() { echo "test-sign.sh: $*" >&2; exit 1; }

security create-keychain -p test "$keychain"
security list-keychains -d user -s ${searched[@]+"${searched[@]}"}
security unlock-keychain -p test "$keychain"
# A signing key of another's beside it (a Developer ID's, say): the script
# must change this one's partition list alone, never every signing key's.
openssl=/opt/homebrew/opt/openssl@3/bin/openssl
[ -x "$openssl" ] || openssl=openssl
printf '[req]\ndistinguished_name = dn\nx509_extensions = ext\nprompt = no\n[dn]\nCN = Bystander\n[ext]\nkeyUsage = critical, digitalSignature\nextendedKeyUsage = critical, codeSigning\n' >"$work/bystander.cnf"
"$openssl" req -x509 -newkey rsa:2048 -nodes -days 1 -quiet -config "$work/bystander.cnf" \
  -keyout "$work/bystander.key" -out "$work/bystander.pem"
"$openssl" pkcs12 -export -legacy -name Bystander -inkey "$work/bystander.key" \
  -in "$work/bystander.pem" -out "$work/bystander.p12" -passout pass:bystander
security import "$work/bystander.p12" -k "$keychain" -P bystander -T /usr/bin/codesign >/dev/null
made=$(SHIPYARD_KEYCHAIN_PASSWORD=test "$here/make-signing-identity.sh" --keychain "$keychain")
grep -q "^codesign may use it without asking: 1 key's partition list set$" <<<"$made" ||
  fail "the partition list wasn't set on this key alone: $made"
again=$(SHIPYARD_KEYCHAIN_PASSWORD=test "$here/make-signing-identity.sh" --keychain "$keychain" 2>&1) &&
  fail "a second identity by the same name was made"
grep -q "is in .* already" <<<"$again" || fail "a second identity refused for another reason: $again"

# Two builds of one program (its identifier the file's name).
mkdir "$work/a" "$work/b"
cp "$binary" "$work/a/shipyard"
cp "$other" "$work/b/shipyard"
one=$work/a/shipyard
two=$work/b/shipyard
# (An ad-hoc signature's is implicit, printed as a comment: `# designated
# => cdhash …`. One read empty is a failure, never a match.)
requirement() {
  local said
  said=$(codesign -d -r- "$1" 2>&1 | sed -n 's/^#* *designated => //p')
  [ -n "$said" ] || fail "$1: no designated requirement read"
  echo "$said"
}

SHIPYARD_SIGN_KEYCHAIN=$keychain "$here/sign.sh" "$one" "$two"
for file in "$one" "$two"; do
  shown=$(codesign -dvv "$file" 2>&1)
  grep -q "Authority=Shipyard Local Signing" <<<"$shown" || fail "$file: not signed by the identity"
  # (A debugger must attach to a local build: no hardened runtime.)
  if grep -q "(runtime)" <<<"$shown"; then fail "$file: signed with the hardened runtime"; fi
  # (Into a variable first: `grep -q` ends at its match, and under pipefail
  # the writer's broken pipe would fail the check.)
  entitlements=$(codesign -d --entitlements - --xml "$file" 2>/dev/null)
  grep -q "com.apple.security.virtualization" <<<"$entitlements" || fail "$file: no entitlement"
done
# (Assigned first: a failure in `$(…)` inside a test would end only it.)
first=$(requirement "$one")
second=$(requirement "$two")
[ "$first" = "$second" ] || fail "two builds signed by the identity have two requirements"

# The control: ad hoc (no identity in an empty keychain's place).
SHIPYARD_SIGN_KEYCHAIN=$work/none.keychain-db "$here/sign.sh" "$one" "$two"
for file in "$one" "$two"; do
  shown=$(codesign -dvv "$file" 2>&1)
  grep -q "Signature=adhoc" <<<"$shown" || fail "$file: the control isn't ad hoc"
done
first=$(requirement "$one")
second=$(requirement "$two")
[ "$first" != "$second" ] || fail "two ad-hoc builds share a requirement: the check proves nothing"

minos=$(vtool -show-build "$binary" | awk '/minos/ {print $2}')
[ "$minos" = 13.0 ] || fail "$binary's minimum macOS is ${minos:-unknown}, not 13.0"
echo "test-sign.sh: ok (one identity across builds, its key's partition list alone set; ad hoc differs; minimum macOS $minos)"

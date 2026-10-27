#!/usr/bin/env bash
# Stage 13's milestone (v0.5) by hand: the parts that need the Mac's user (a
# sleep, a logout, a reboot, the signing identity's one prompt). Each
# subcommand checks and prints one line a check, "ok" or "FAILED: why"; its
# state between steps is kept in ~/Library/Application Support/Shipyard/milestone-13.
#
# usage: scripts/macos/milestone-13.sh STEP
#   identity       after scripts/macos/make-signing-identity.sh: a rebuild is
#                  signed by it (doctor's line), twice the same requirement
#   install        copies this build (binary and guest) outside your Desktop,
#                  installs it with --at-login, and starts a --restart always
#                  container: then log out and in (or reboot), then:
#   after-login    the VM up by itself, the container back (no command run first)
#   documents      a container reads ~/Documents through a launchd-started VM:
#                  answer macOS's prompt once; run it again after a rebuild
#                  (`identity` first): no second prompt
#   before-sleep   a container kept running (the VM mustn't stop idle asleep),
#                  the guest's clock against the Mac's; then sleep the Mac (a few
#                  minutes), wake it, and at once (run nothing else first):
#   after-wake     waits 12 s running nothing, then the host's own poll set the
#                  guest's clock (its console's line, stamped after the sleep),
#                  and the clock within 2 s of the Mac's
#   cleanup        the container removed, the agent uninstalled, the copies gone
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
state="$HOME/Library/Application Support/Shipyard/milestone-13"
mkdir -p "$state"
image=alpine:3.20
installed=$state/bin/shipyard
ok() { echo "ok      $*"; }
failed() { echo "FAILED  $*"; status=1; }
status=0
# The installed copy once `install` ran, else this tree's build.
shipyard() {
  if [ -x "$installed" ]; then
    env -u SHIPYARD_VM SHIPYARD_GUEST_DIR="$state/guest" "$installed" "$@"
  else
    env -u SHIPYARD_VM "$repo/target/debug/shipyard" "$@"
  fi
}
vm_dir="$HOME/Library/Application Support/Shipyard/vm"
clock_skew() {
  local mac guest
  guest=$(shipyard run --rm "$image" date +%s)
  mac=$(date +%s)
  echo $((mac > guest ? mac - guest : guest - mac))
}

case "${1:-}" in
identity)
  (cd "$repo" && cargo build -q -p shipyard-cli --bins) && "$repo/scripts/macos/sign.sh" "$repo/target/debug/shipyard"
  line=$(shipyard doctor 2>&1 | grep entitlement || true)
  case "$line" in
  *"one identity across rebuilds"*) ok "signed by your identity: $line" ;;
  *) failed "not signed by an identity: $line (make-signing-identity.sh first?)" ;;
  esac
  first=$(codesign -d -r- "$repo/target/debug/shipyard" 2>&1 | sed -n 's/^designated => //p')
  touch "$repo/crates/shipyard-cli/src/main.rs"
  (cd "$repo" && cargo build -q -p shipyard-cli --bins) && "$repo/scripts/macos/sign.sh" "$repo/target/debug/shipyard"
  second=$(codesign -d -r- "$repo/target/debug/shipyard" 2>&1 | sed -n 's/^designated => //p')
  [ -n "$first" ] && [ "$first" = "$second" ] && ok "a rebuild keeps the requirement: $first" ||
    failed "a rebuild changed the requirement ($first, then $second)"
  ;;
install)
  mkdir -p "$state/bin"
  cp "$repo/target/debug/shipyard" "$installed"
  "$repo/scripts/macos/sign.sh" "$installed"
  rm -rf "$state/guest" && cp -R "$repo/target/guest" "$state/guest"
  shipyard vm stop >/dev/null 2>&1 || true
  shipyard vm install --at-login && ok "installed with --at-login ($installed)"
  shipyard run -d --restart always --name milestone-back "$image" sleep 100000 >/dev/null &&
    ok "a --restart always container runs"
  for _ in $(seq 30); do
    shipyard vm status | grep -q "at login: starts the VM (1 container" && break
    sleep 1
  done
  shipyard vm status | grep "at login" || true
  shipyard vm stop && ok "the VM stopped: now log out and in (or reboot), then: $0 after-login"
  ;;
after-login)
  [ -x "$installed" ] || { failed "run '$0 install' first"; exit 1; }
  # `vm status` starts nothing: running means the login started it.
  if shipyard vm status | head -1 | grep -q '^running'; then
    ok "the VM is up, started at login"
  else
    failed "the VM isn't running: the login didn't start it (host.log: $(tail -3 "$HOME/Library/Application Support/Shipyard/vm/host.log" 2>/dev/null))"
  fi
  grep -q "at login: 1 container would come back" "$HOME/Library/Application Support/Shipyard/vm/host.log" 2>/dev/null &&
    ok "its log says the login started it" || failed "its log says nothing of the login"
  shipyard ps --format '{{.Names}} {{.Status}}' | grep -q '^milestone-back Up' &&
    ok "the container is back" || failed "the container isn't up"
  ;;
documents)
  [ -x "$installed" ] || { failed "run '$0 install' first"; exit 1; }
  echo "(macOS may ask whether shipyard may read Documents: allow it, once)"
  shipyard run --rm --volume "$HOME/Documents:/d" "$image" ls /d >/dev/null &&
    ok "a container reads ~/Documents" || failed "~/Documents isn't readable to the VM"
  ;;
before-sleep)
  shipyard rm -f milestone-clock >/dev/null 2>&1 || true
  shipyard run -d --name milestone-clock "$image" sleep 100000 >/dev/null &&
    ok "a container runs through the sleep (the VM stays up)"
  skew=$(clock_skew)
  date +%s >"$state/slept-at"
  ok "the guest's clock is $skew s from the Mac's: now sleep the Mac a few minutes, wake it, then at once: $0 after-wake"
  ;;
after-wake)
  [ -f "$state/slept-at" ] || { failed "run '$0 before-sleep' first"; exit 1; }
  slept=$(cat "$state/slept-at")
  # Every forwarded command carries the Mac's time and sets the clock
  # itself: nothing is run until the host's own poll (every 2 s) has had
  # its chance. Its line in the console says the Mac's time it set:
  # `the clock was OURS, the Mac's THEIRS: set to the Mac's` (the console
  # may have been moved aside to console.log.1 meanwhile).
  sleep 12
  # (`|| true`: console.log.1 is often missing, and pipefail would end here.)
  set_at=$({ cat "$vm_dir/console.log.1" "$vm_dir/console.log" 2>/dev/null || true; } |
    sed -n "s/.*the clock was [0-9]*, the Mac's \([0-9]*\): set to the Mac's.*/\1/p" |
    awk -v after="$slept" '$1 > after' | tail -1)
  [ -n "$set_at" ] && ok "the host set the guest's clock after the wake, to $set_at" ||
    failed "no 'the clock was …: set to the Mac's' line after $slept in $vm_dir/console.log (did the Mac sleep long enough for the guest's clock to fall behind?)"
  skew=$(clock_skew)
  [ "$skew" -le 2 ] && ok "the guest's clock is $skew s from the Mac's after the wake" ||
    failed "the guest's clock is $skew s off after the wake"
  shipyard rm -f milestone-clock >/dev/null 2>&1 || true
  rm -f "$state/slept-at"
  ;;
cleanup)
  shipyard rm -f milestone-back >/dev/null 2>&1 && ok "the container removed" || true
  shipyard rm -f milestone-clock >/dev/null 2>&1 || true
  shipyard vm uninstall >/dev/null 2>&1 && ok "the agent uninstalled" || true
  rm -rf "$state" && ok "the copies removed"
  ;;
*)
  sed -n 2,25p "$0" >&2
  exit 2
  ;;
esac
exit "$status"

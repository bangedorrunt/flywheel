#!/usr/bin/env bash
# bd-i4edn: after an install replaces a binary, launchd refuses to spawn the
# job until it is re-registered from its plist. restart_label must bootout and
# bootstrap a job that has a plist, and kickstart only one without a plist.
# Never touches live launchd: launchctl is a stub that records its arguments.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/stack-reregister-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/bin" "$TMP/home/Library/LaunchAgents"
export PATH="$TMP/bin:$PATH"
export HOME="$TMP/home"
export LAUNCHCTL_LOG="$TMP/launchctl.log"

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

cat >"$TMP/bin/launchctl" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$LAUNCHCTL_LOG"
exit 0
SH
chmod +x "$TMP/bin/launchctl"

PLIST="$HOME/Library/LaunchAgents/dev.flywheel.doorbell.demo.plist"
printf '<plist version="1.0"><dict/></plist>\n' >"$PLIST"

(
  # shellcheck disable=SC1091
  . "$ROOT/scripts/stack.sh"
  restart_label dev.flywheel.doorbell.demo
  restart_label dev.flywheel.noplist
) >/dev/null

uid="$(id -u)"
grep -qxF "bootout gui/$uid/dev.flywheel.doorbell.demo" "$LAUNCHCTL_LOG" \
  || fail "a job with a plist must be booted out: $(cat "$LAUNCHCTL_LOG")"
grep -qxF "bootstrap gui/$uid $PLIST" "$LAUNCHCTL_LOG" \
  || fail "a job with a plist must be bootstrapped from it: $(cat "$LAUNCHCTL_LOG")"
if grep -qF "kickstart -k gui/$uid/dev.flywheel.doorbell.demo" "$LAUNCHCTL_LOG"; then
  fail "kickstart cannot refresh a replaced binary's signature: $(cat "$LAUNCHCTL_LOG")"
fi
pass "a job with a plist is re-registered from it"

grep -qxF "kickstart -k gui/$uid/dev.flywheel.noplist" "$LAUNCHCTL_LOG" \
  || fail "a job without a plist falls back to kickstart: $(cat "$LAUNCHCTL_LOG")"
pass "a job without a plist is kickstarted"
echo ok

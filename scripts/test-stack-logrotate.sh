#!/usr/bin/env bash
# bd-9zw9u: install rewrites an invalid toron logrotate side plist, and status
# --check reports it. Sandboxed HOME/XDG, deny-shimmed launchctl, fake cargo.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/stack-logrotate.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

mkdir -p "$TMP/home/Library/LaunchAgents" "$TMP/state" "$TMP/config" \
  "$TMP/cargo" "$TMP/fakeshim" "$TMP/bin" "$TMP/src"
: >"$TMP/src/Cargo.toml"
PLIST="$TMP/home/Library/LaunchAgents/com.toron.logrotate.plist"
printf '<string>cp "$f" "$f.1" && : > "$f"</string>\n' >"$PLIST"

for tool in launchctl systemctl; do
  printf '#!/bin/sh\nexit 1\n' >"$TMP/fakeshim/$tool"
  chmod +x "$TMP/fakeshim/$tool"
done
printf '#!/bin/sh\nexit 0\n' >"$TMP/fakeshim/cargo"
chmod +x "$TMP/fakeshim/cargo"
printf '#!/bin/sh\nexit 0\n' >"$TMP/bin/flywheel"
chmod +x "$TMP/bin/flywheel"
cat >"$TMP/bin/toron" <<'SH'
#!/bin/sh
echo '{"build":{"restart_required":false}}'
exit 0
SH
chmod +x "$TMP/bin/toron"

PATH_VALUE="$TMP/fakeshim:$TMP/bin:/usr/bin:/bin"
status() {
  env -i HOME="$TMP/home" XDG_STATE_HOME="$TMP/state" XDG_CONFIG_HOME="$TMP/config" \
    CARGO_INSTALL_ROOT="$TMP/cargo" CARGO_CMD=cargo FLYWHEEL_SOURCE="$TMP/src" \
    PATH="$PATH_VALUE" bash "$ROOT/scripts/stack.sh" status --check 2>&1
}

set +e
before="$(status)"; before_rc=$?
set -e
[ "$before_rc" -ne 0 ] || fail "invalid plist must make status --check fail: $before"
printf '%s\n' "$before" | grep -q 'invalid plist' || fail "status must name the plist: $before"
pass "status --check reports an invalid side plist"

set +e
out="$(env -i HOME="$TMP/home" XDG_STATE_HOME="$TMP/state" XDG_CONFIG_HOME="$TMP/config" \
  CARGO_INSTALL_ROOT="$TMP/cargo" CARGO_CMD=cargo FLYWHEEL_SOURCE="$TMP/src" \
  PATH="$PATH_VALUE" bash "$ROOT/scripts/stack.sh" install --operator --no-restart \
  --no-codesign --skip-toron 2>&1)"
install_rc=$?
set -e
[ "$install_rc" -eq 0 ] || fail "install failed: $out"
plutil -lint "$PLIST" >/dev/null 2>&1 || fail "install left an invalid plist: $(cat "$PLIST")"
pass "install rewrote the plist and plutil -lint passes"

set +e
after="$(status)"; after_rc=$?
set -e
[ "$after_rc" -eq 0 ] || fail "status --check must pass after the rewrite: $after"
pass "status --check passes after install"
echo "ok"

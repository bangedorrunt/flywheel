#!/usr/bin/env bash
# Sandbox test for the captain-only gate in scripts/stack.sh install
# (bd-0uzrq.24). Never touches live launchd, live ~/.cargo/bin, or the real
# toron store: temp HOME/XDG, CARGO_INSTALL_ROOT, and a stubbed cargo.
#
# Usage: scripts/test-stack-guard.sh <flywheel-binary>
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FW="${1:?usage: test-stack-guard.sh <flywheel-binary>}"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/stack-guard-test.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

mkdir -p "$TMP/home" "$TMP/state" "$TMP/config" "$TMP/cargo" "$TMP/toron" \
  "$TMP/bin" "$TMP/src" "$TMP/fakeshim"
: >"$TMP/src/Cargo.toml"

# The test binary answers the guard; a stub `flywheel` simulates an installed
# binary too OLD to know the verb (fail closed) in its own case below.
ln -sf "$FW" "$TMP/bin/flywheel"

# Deny-shims: a guard test must never reach the live supervisor.
for tool in launchctl systemctl; do
  printf '#!/bin/sh\necho "%s $*" >> "%s"\nexit 1\n' "$tool" "$TMP/shim.log" \
    >"$TMP/fakeshim/$tool"
  chmod +x "$TMP/fakeshim/$tool"
done

# A cargo that records instead of building.
printf '#!/bin/sh\necho "cargo $*" >> "%s"\n' "$TMP/cargo.log" >"$TMP/fakeshim/cargo"
chmod +x "$TMP/fakeshim/cargo"

install_case() { # label, PATH, extra env..., args...
  local label="$1" path="$2"
  shift 2
  set +e
  out="$(
    env -i \
      HOME="$TMP/home" \
      XDG_STATE_HOME="$TMP/state" \
      XDG_CONFIG_HOME="$TMP/config" \
      TORON_DATA_DIR="$TMP/toron" \
      TORON_MCP_DATA_DIR="$TMP/toron" \
      CARGO_INSTALL_ROOT="$TMP/cargo" \
      CARGO_CMD=cargo \
      FLYWHEEL_SOURCE="$TMP/src" \
      PATH="$path" \
      SHIM_LOG="$TMP/shim.log" \
      "$@" \
      bash "$ROOT/scripts/stack.sh" install \
      --no-restart --no-codesign --skip-toron 2>&1
  )"
  rc=$?
  set -e
  echo "--- $label (rc=$rc)"
  printf '%s\n' "$out" | tail -5
  LAST_RC="$rc"
  LAST_OUT="$out"
}

# 1. A crew pane with a pin: refused, and cargo never ran.
install_case "crew pin" "$TMP/fakeshim:$TMP/bin:/usr/bin:/bin" \
  AGENT_NAME=CrewPin FLYWHEEL_PANE=1
[ "$LAST_RC" -eq 2 ] || fail "case1: expected rc 2, got $LAST_RC"
printf '%s\n' "$LAST_OUT" | grep -Fq "captain-only" || fail "case1: no refusal named: $LAST_OUT"
[ ! -s "$TMP/cargo.log" ] || fail "case1: cargo ran despite the refusal"

# 2. A crew pane that unset its pin: still refused.
install_case "crew pane, no pin" "$TMP/fakeshim:$TMP/bin:/usr/bin:/bin" \
  FLYWHEEL_PANE=1
[ "$LAST_RC" -eq 2 ] || fail "case2: expected rc 2, got $LAST_RC"
printf '%s\n' "$LAST_OUT" | grep -Fq "captain-only" || fail "case2: no refusal named: $LAST_OUT"

# 3. No flywheel binary to prove identity: fail closed.
install_case "no binary" "$TMP/fakeshim:/usr/bin:/bin" AGENT_NAME=CrewPin
[ "$LAST_RC" -eq 2 ] || fail "case3: expected rc 2, got $LAST_RC"
printf '%s\n' "$LAST_OUT" | grep -Fq "no flywheel binary" || fail "case3: missing fail-closed message: $LAST_OUT"

# 4. A binary too old to know the verb: fail closed (its refusal is not a pass).
mkdir -p "$TMP/oldbin"
cat >"$TMP/oldbin/flywheel" <<'SH'
#!/bin/sh
echo "error: unrecognized subcommand '__guard-fleet'" >&2
exit 2
SH
chmod +x "$TMP/oldbin/flywheel"
install_case "old binary" "$TMP/fakeshim:$TMP/oldbin:/usr/bin:/bin" AGENT_NAME=CrewPin
[ "$LAST_RC" -eq 2 ] || fail "case4: expected rc 2, got $LAST_RC"

# 5. The operator's own shell (no pin, no pane marker): install proceeds.
: >"$TMP/cargo.log"
install_case "operator shell" "$TMP/fakeshim:$TMP/bin:/usr/bin:/bin"
[ "$LAST_RC" -eq 0 ] || fail "case5: expected rc 0, got $LAST_RC: $LAST_OUT"
grep -Fq "install --path $TMP/src --force" "$TMP/cargo.log" \
  || fail "case5: the install never ran: $(cat "$TMP/cargo.log" 2>/dev/null)"
pass "operator shell installs"

# 6. --operator is the deliberate escape from a pane.
install_case "operator escape" "$TMP/fakeshim:$TMP/bin:/usr/bin:/bin" \
  AGENT_NAME=CrewPin FLYWHEEL_PANE=1
[ "$LAST_RC" -eq 2 ] || fail "case6 precondition: an unguarded call must be refused"

set +e
out="$(
  env -i HOME="$TMP/home" XDG_STATE_HOME="$TMP/state" XDG_CONFIG_HOME="$TMP/config" \
    TORON_DATA_DIR="$TMP/toron" TORON_MCP_DATA_DIR="$TMP/toron" \
    CARGO_INSTALL_ROOT="$TMP/cargo" CARGO_CMD=cargo FLYWHEEL_SOURCE="$TMP/src" \
    AGENT_NAME=CrewPin FLYWHEEL_PANE=1 \
    PATH="$TMP/fakeshim:$TMP/bin:/usr/bin:/bin" SHIM_LOG="$TMP/shim.log" \
    bash "$ROOT/scripts/stack.sh" install \
    --operator --no-restart --no-codesign --skip-toron 2>&1
)"
rc=$?
set -e
[ "$rc" -eq 0 ] || fail "case6: --operator must pass, got $rc: $out"
pass "--operator is the explicit escape"

[ ! -s "$TMP/shim.log" ] || fail "a supervisor tool was called: $(cat "$TMP/shim.log")"
pass "no live supervisor call"

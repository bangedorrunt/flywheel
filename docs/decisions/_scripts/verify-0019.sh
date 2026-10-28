#!/usr/bin/env bash
# verify-0019.sh — ADR-0019 Contract gate D0-D7 Phase 0. Fail-closed.
set -u
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
FAIL=0
pass(){ echo "PASS: $1"; }
fail(){ echo "FAIL: $1"; FAIL=1; }

echo "== ADR-0019 Phase 0 (Lane A Contract D0-D7) =="

# Constants pinned in captain_directory.rs
SRC="$ROOT/src/captain_directory.rs"
[ -f "$SRC" ] || { fail "src/captain_directory.rs missing"; echo "RESULT: FAIL"; exit 1; }
for kv in "PULL_FRESH_MS.*120.?000" "LOOP_LIMIT.*12" "LOOP_WINDOW_MS.*600.?000" "IN_FLIGHT_TTL_SECS.*90" "SESSION_FORGET_MS.*86_400_000" "SESSION_START_GRACE_MS.*7_200_000" "APPROVAL_WAIT_MS.*600_000" "DOING_MAX_CHARS.*200"; do
  grep -Eq "$kv" "$SRC" || fail "constant missing: $kv"
done
pass "constants present (failures listed above if any)"

# Required pure symbols, no second loop_ok copy
for sym in "pub enum Tier" "pub fn tier_of" "pub fn project_directory" "pub fn resolve_address" "pub enum Delivery" "pub fn delivery_of" "pub fn session_key" "pub fn pull_fresh" "pub fn should_forget" "pub fn truncate_doing"; do
  grep -Fq "$sym" "$SRC" || fail "captain_directory.rs missing: $sym"
done
pass "directory symbol surface present"
grep -Rq "fn loop_ok" "$ROOT/src/" && fail "flywheel must not define fn loop_ok (toron-core owns D8)"
[ "$FAIL" -eq 0 ] && pass "no flywheel loop_ok copy"

# Binding location + pull freshness wiring
grep -Fq "BindingLocation" "$ROOT/src/agent_ref.rs" || fail "agent_ref.rs missing BindingLocation"
grep -Fq "pull_fresh" "$ROOT/src/captain_slot.rs" || fail "captain_slot.rs missing pull_fresh"
pass "binding/freshness wiring present (failures listed above if any)"

# Grok stays unverified: hook None, steer unsupported
if grep -A6 'harness: "grok"' "$ROOT/src/harness_caps.rs" | grep -Eq 'hook: Some|steer: Capability::supported'; then
  fail "grok hook/steer changed (D11: stays unverified)"
else
  pass "grok hook stays unverified"
fi

# No new network/store deps for this feature
if grep -Eq '^\s*(iroh|rusqlite)\s*=' "$ROOT/Cargo.toml" "$ROOT"/crates/*/Cargo.toml 2>/dev/null; then
  fail "Cargo gains iroh or rusqlite"
else
  pass "no iroh/rusqlite deps"
fi
if grep -Rq "SqliteMailbox\|sqlite:.*mailbox\|rusqlite::" "$ROOT/src/" 2>/dev/null; then
  fail "new sqlite mailbox type under src/"
else
  pass "no sqlite mailbox type"
fi

# Behavior gate: module tests green
echo "-- behavior (cargo) --"
if command -v mbx >/dev/null 2>&1; then CARGO="mbx +nightly-aarch64-apple-darwin"; else CARGO="cargo"; fi
$CARGO test --bin flywheel captain_directory 2>&1 | tail -n 3
[ "${PIPESTATUS[0]}" -eq 0 ] || fail "captain_directory tests red"

if [ "$FAIL" -ne 0 ]; then echo "RESULT: FAIL"; exit 1; fi
echo "RESULT: PASS"

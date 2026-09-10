#!/usr/bin/env bash
# verify-0010a.sh — ADR-0010 Wave A (D1, D2, D3, D4, D14): delivery seam.
# Structural + CLI surface probes per ADR §13. Behavioral fixtures
# (kill-pane survival, crash-replay) live in the pinned cargo tests; this gate
# fails if the surface or its knobs disappear. Fail-closed.
set -u
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
FAIL=0
pass(){ echo "PASS: $1"; }
fail(){ echo "FAIL: $1"; FAIL=1; }

echo "== ADR-0010 Wave A (D1/D2/D3/D4/D14) =="

# D1 — re-ring ladder knobs + fire-and-forget opt-out.
grep -q "FLYWHEEL_DOORBELL_GRACE_SECS" "$ROOT/src/workspace.rs" \
  && pass "D1 grace knob (workspace.rs)" \
  || fail "D1 FLYWHEEL_DOORBELL_GRACE_SECS missing"
grep -q "FLYWHEEL_DOORBELL_RING_MAX" "$ROOT/src/workspace.rs" \
  && pass "D1 ring-max knob (workspace.rs)" \
  || fail "D1 FLYWHEEL_DOORBELL_RING_MAX missing"
"$ROOT/target/debug/flywheel" send --help 2>/dev/null | grep -q -- "--fire-and-forget" \
  && pass "D1 --fire-and-forget on send" \
  || { BIN="$(command -v flywheel || true)"; \
       if [ -n "$BIN" ] && "$BIN" send --help 2>/dev/null | grep -q -- "--fire-and-forget"; then
         pass "D1 --fire-and-forget on send (installed binary)"
       else
         fail "D1 --fire-and-forget not on send surface"
       fi; }

# D2 — generation-bound wake ack watermark.
grep -q "WakeAck" "$ROOT/src/workspace.rs" \
  && pass "D2 WakeAck in tick model (workspace.rs)" \
  || fail "D2 WakeAck missing from workspace.rs"

# D3 — semantic verdicts; Unknown never promoted.
grep -q "Unknown" "$ROOT/src/workspace.rs" \
  && pass "D3 Unknown verdict present (workspace.rs)" \
  || fail "D3 Unknown verdict missing"
grep -qE "needs-decision|paused:|blocked:" "$ROOT/integrations/peer-aware.md.tpl" \
  && pass "D3 kind-9 sparse verbs in peer-aware template" \
  || fail "D3 kind-9 verbs missing from peer-aware.md.tpl"

# D4 — write-liveness + inspect cadence knobs.
[ -f "$ROOT/src/write_liveness.rs" ] \
  && pass "D4 write_liveness.rs exists" \
  || fail "D4 src/write_liveness.rs missing"
for knob in FLYWHEEL_STALE_ESCALATE_SECS FLYWHEEL_BUSY_TURN_MAX_SECS FLYWHEEL_WEDGE_INSPECT_COUNT; do
  grep -rq "$knob" "$ROOT/src/" && pass "D4 knob $knob" || fail "D4 knob $knob missing"
done

# D14 — captain-facing turn-end guard.
grep -rq "FLYWHEEL_GUARD_GRACE_SECS" "$ROOT/src/" \
  && pass "D14 guard-grace knob present" \
  || fail "D14 FLYWHEEL_GUARD_GRACE_SECS missing"

if [ "$FAIL" -eq 0 ]; then echo "RESULT: PASS"; exit 0; else echo "RESULT: FAIL"; exit 1; fi

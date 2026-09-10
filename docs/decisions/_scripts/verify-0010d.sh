#!/usr/bin/env bash
# verify-0010d.sh — ADR-0010 Wave D (D10, D11, D12): surfaces.
# D11's beads-side acceptance is the beads captain's act (ADR-0005); this gate
# probes the shipped surfaces only. Fail-closed.
set -u
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
FAIL=0
pass(){ echo "PASS: $1"; }
fail(){ echo "FAIL: $1"; FAIL=1; }

echo "== ADR-0010 Wave D (D10/D11/D12) =="

# D10 — one snapshot contract + helm drain.
[ -f "$ROOT/src/snapshot.rs" ] \
  && pass "D10 src/snapshot.rs exists" \
  || fail "D10 src/snapshot.rs missing"
BIN="${BIN:-$(command -v flywheel || echo "$ROOT/target/debug/flywheel")}"
SNAP_PROJECT="${SNAP_PROJECT:-toron}"
"$BIN" snapshot "$SNAP_PROJECT" 2>/dev/null | grep -q "flywheel-snapshot.v1" \
  && pass "D10 snapshot emits flywheel-snapshot.v1" \
  || fail "D10 snapshot output missing schema name"
"$BIN" helm --help 2>/dev/null | grep -q "Session-start drain" \
  && pass "D10 helm is the session-start drain" \
  || fail "D10 helm surface missing"

# D11 — beads successor surfaces (deliverable + promotes). Acceptance of
# beads ADR-0005 itself is the beads captain's separate act (pending).
BR_BIN="$(command -v br || true)"
if [ -n "$BR_BIN" ]; then
  "$BR_BIN" create --help 2>/dev/null | grep -q -- "--deliverable" \
    && "$BR_BIN" create --help 2>/dev/null | grep -q -- "--promotes" \
    && pass "D11 beads deliverable + promotes surfaces live" \
    || fail "D11 beads --deliverable/--promotes missing"
else
  fail "br binary not found for D11 probe"
fi

# D12 — tiered learnings decay + bounded GATE projection.
grep -rq "FLYWHEEL_GATE_BUDGET" "$ROOT/src/" \
  && pass "D12 GATE budget knob" \
  || fail "D12 FLYWHEEL_GATE_BUDGET missing"
grep -rqE "reinforced|tier" "$ROOT/src/stages.rs" \
  && pass "D12 tier/reinforced handling in stages" \
  || fail "D12 tier/reinforced handling missing from stages.rs"

if [ "$FAIL" -eq 0 ]; then echo "RESULT: PASS"; exit 0; else echo "RESULT: FAIL"; exit 1; fi

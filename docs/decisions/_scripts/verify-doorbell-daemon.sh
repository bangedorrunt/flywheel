#!/usr/bin/env bash
# verify-doorbell-daemon.sh — bd-asr2.2 acceptance gate. Fail-closed.
set -u
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="$ROOT/target/debug/doorbell"
STATE="$(mktemp -d "${TMPDIR:-/tmp}/doorbell-verify.XXXXXX")"
PORT="${DOORBELL_TEST_PORT:-18981}"
export DOORBELL_STATE_DIR="$STATE" DOORBELL_PORT="$PORT"
FAIL=0
pass(){ echo "PASS: $1"; }
fail(){ echo "FAIL: $1"; FAIL=1; }
cleanup(){ "$BIN" stop --state-dir "$STATE" --port "$PORT" >/dev/null 2>&1; rm -rf "$STATE"; }
trap cleanup EXIT

echo "== bd-asr2.2 daemon lifecycle =="
[ -x "$BIN" ] || { fail "doorbell binary missing (build first)"; echo "RESULT: FAIL"; exit 1; }

"$BIN" start --state-dir "$STATE" --port "$PORT" >/dev/null 2>&1 \
  || { fail "start exited non-zero"; echo "RESULT: FAIL"; exit 1; }
pass "start verified (exit 0 means /health answered)"

HEALTH="$(curl -sf "http://127.0.0.1:$PORT/health" 2>/dev/null)" \
  || { fail "/health unreachable"; echo "RESULT: FAIL"; exit 1; }
for key in protocol core_v journal_v uptime_s watch sweep subscribers degraded journal owner; do
  echo "$HEALTH" | grep -Fq "\"$key\"" || { fail "/health missing key: $key"; }
done
pass "/health schema present"

STATUS="$("$BIN" status --state-dir "$STATE" --port "$PORT" 2>/dev/null)"
echo "$STATUS" | grep -Fq '"alive":true' || fail "status does not report alive"
pass "status reports alive"
# Unmanaged-holder refusal: foreign pidfile + live port, no --force.
cp "$STATE/doorbell.pid" "$STATE/doorbell.pid.real"
echo 99999999 > "$STATE/doorbell.pid"
if "$BIN" stop --state-dir "$STATE" --port "$PORT" >/dev/null 2>&1; then
  fail "stop over unmanaged holder should refuse without --force"
else
  pass "unmanaged-holder refusal without --force"
fi
mv "$STATE/doorbell.pid.real" "$STATE/doorbell.pid"

"$BIN" stop --state-dir "$STATE" --port "$PORT" >/dev/null 2>&1 \
  || fail "stop exited non-zero"
sleep 1
if curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
  fail "port still answers after stop"
else
  pass "port dead after stop (5s grace honored)"
fi
echo "-- E13-lite: split-brain fence --"
"$BIN" start --state-dir "$STATE" --port "$PORT" >/dev/null 2>&1 \
  || { fail "restart for fence tests"; echo "RESULT: FAIL"; exit 1; }
# Second lifecycle start over the live peer exits 0 (no respawn war).
"$BIN" start --state-dir "$STATE" --port "$PORT" >/dev/null 2>&1 \
  || fail "second start over healthy peer must exit 0"
pass "double-start clean exit 0"
# No lease + healthy peer: raw serve must exit 0 via the bind fence
# (delete the lease file; the daemon still holds the port).
rm -f "$STATE/doorbell/owner.json"
"$BIN" serve --state-dir "$STATE" --port "$PORT" >/dev/null 2>&1 &
SRV=$!
wait "$SRV"
[ "$?" -eq 0 ] || fail "leaseless serve over healthy peer must exit 0"
pass "bind fence clean exit 0 over healthy peer"
# Stale lease takeover: kill -9 leaves a dead-pid lease AND frees the port,
# so the next serve takes over (owner becomes the new pid).
APID="$(cat "$STATE/doorbell.pid")"
kill -9 "$APID" 2>/dev/null
sleep 1
"$BIN" serve --state-dir "$STATE" --port "$PORT" >/dev/null 2>&1 &
SRV=$!
sleep 2
OWNER="$(curl -sf "http://127.0.0.1:$PORT/health" 2>/dev/null | grep -o '"pid":[0-9]*' | head -n 1)"
[ "$OWNER" = "\"pid\":$SRV" ] || fail "takeover owner should be $SRV, got $OWNER"
pass "stale-lease takeover serves"
kill "$SRV" 2>/dev/null
wait "$SRV" 2>/dev/null
sleep 1
# Non-doorbell squatter: plain HTTP on another port must hard-fail naming it.
SQUAT=18982
python3 -m http.server --bind 127.0.0.1 "$SQUAT" >/dev/null 2>&1 &
SQ=$!
sleep 1
if "$BIN" start --state-dir "$STATE" --port "$SQUAT" >/dev/null 2>&1; then
  fail "start over non-doorbell squatter must hard-fail"
else
  pass "non-doorbell squatter hard fail"
fi
kill "$SQ" 2>/dev/null
"$BIN" stop --state-dir "$STATE" --port "$PORT" >/dev/null 2>&1 \
  || fail "final stop exited non-zero"
sleep 1
curl -sf "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 \
  && fail "port still answers after final stop" \
  || pass "final stop leaves port dead"

if [ "$FAIL" -eq 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; fi
exit "$FAIL"

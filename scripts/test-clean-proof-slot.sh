#!/usr/bin/env bash
# Sandbox test for clean-proof slot serialization (bd-jp9z1.1).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/clean-proof-slot-test.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

export CLEAN_PROOF_LOCK_FILE="$TMP/clean-proof.lock"
MARKER="$TMP/markers.txt"
export CLEAN_PROOF_LOCK_TEST_MARKER="$MARKER"

# --- Case 1: two concurrent invocations never overlap ---
: >"$MARKER"
bash "$ROOT/scripts/clean-proof.sh" --lock-hold-test 1 &
p1=$!
bash "$ROOT/scripts/clean-proof.sh" --lock-hold-test 1 &
p2=$!
wait "$p1"
wait "$p2"

python3 - "$MARKER" <<'PY'
import sys
path = sys.argv[1]
events = []
for line in open(path):
    kind, pid, ts = line.split()
    events.append((kind, pid, int(ts)))
by = {}
order = []
for kind, pid, ts in events:
    if pid not in by:
        by[pid] = {}
        order.append(pid)
    by[pid][kind] = ts
if len(order) != 2:
    raise SystemExit(f"expected 2 runs, got {len(order)}: {events}")
a, b = order[0], order[1]
a_ok = by[a]['end'] <= by[b]['start']
b_ok = by[b]['end'] <= by[a]['start']
if not (a_ok or b_ok):
    raise SystemExit(f"overlap: {by}")
print("no-overlap")
PY
pass "concurrent invocations do not overlap"

# --- Case 2: killed holder frees the slot ---
HOLDER_LOG="$TMP/holder.log"
bash "$ROOT/scripts/clean-proof.sh" --lock-hold-test 30 >"$HOLDER_LOG" 2>&1 &
holder=$!
for _ in $(seq 1 50); do
  grep -q '^start' "$MARKER" 2>/dev/null && break
  sleep 0.1
done
grep -q '^start' "$MARKER" || fail "holder did not start"
kill -9 "$holder" 2>/dev/null || true
wait "$holder" 2>/dev/null || true

start_ts=$(python3 -c 'import time; print(time.time())')
MARKER2="$TMP/marker2.txt"
CLEAN_PROOF_LOCK_TEST_MARKER="$MARKER2" bash "$ROOT/scripts/clean-proof.sh" --lock-hold-test 0
end_ts=$(python3 -c 'import time; print(time.time())')
python3 - "$start_ts" "$end_ts" <<'PY'
import sys
start, end = float(sys.argv[1]), float(sys.argv[2])
if end - start > 5:
    raise SystemExit(f"took too long after kill: {end-start}s")
print("prompt")
PY
pass "killed holder frees slot promptly"

echo "ok"

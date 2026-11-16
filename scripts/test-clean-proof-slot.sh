#!/usr/bin/env bash
# Sandbox test for clean-proof slot serialization + snapshotting (bd-jp9z1.1).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/clean-proof-slot-test.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

export CLEAN_PROOF_LOCK_FILE="$TMP/clean-proof.lock"
# Inside an outer proof CLEAN_PROOF_SLOTTED is already 1, so the nested
# invocations would skip the flock re-exec and this test would see overlap.
unset CLEAN_PROOF_SLOTTED
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
: >"$MARKER"
bash "$ROOT/scripts/clean-proof.sh" --lock-hold-test 30 >/dev/null 2>&1 &
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

# --- Case 3: edit source script mid-run; run completes on snapshot copy ---
FAKE="$TMP/fakerepo"
mkdir -p "$FAKE/scripts"
cp "$ROOT/scripts/clean-proof.sh" "$FAKE/scripts/clean-proof.sh"
cp "$ROOT/scripts/proof-lib.sh" "$FAKE/scripts/proof-lib.sh"
chmod +x "$FAKE/scripts/clean-proof.sh"

git -C "$FAKE" init -q
git -C "$FAKE" config user.email test@test
git -C "$FAKE" config user.name test
git -C "$FAKE" add scripts/clean-proof.sh scripts/proof-lib.sh
git -C "$FAKE" commit -qm init

: >"$TMP/marker3.txt"
CLEAN_PROOF_LOCK_FILE="$TMP/clean-proof3.lock" \
CLEAN_PROOF_LOCK_TEST_MARKER="$TMP/marker3.txt" \
bash "$FAKE/scripts/clean-proof.sh" --lock-hold-test 2 >"$TMP/edit.log" 2>&1 &
ep=$!
for _ in $(seq 1 50); do
  grep -q '^start' "$TMP/marker3.txt" 2>/dev/null && break
  sleep 0.1
done
grep -q '^start' "$TMP/marker3.txt" || fail "mid-run edit holder did not start"
printf '\nthis_should_break_if_read_from_tree () { :; }\nSYNTAX_BOMB(((\n' >>"$FAKE/scripts/clean-proof.sh"
wait "$ep"
rc=$?
[ "$rc" -eq 0 ] || fail "mid-run edit case: expected exit 0, got $rc (log: $(cat "$TMP/edit.log"))"
pass "mid-run source edit does not break running proof (snapshot copy)"

echo "ok"

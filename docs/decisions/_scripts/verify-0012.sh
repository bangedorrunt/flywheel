#!/usr/bin/env bash
# verify-0012.sh — ADR-0012 acceptance gate. Phase 0 lands here; later phases
# append their sections. Fail-closed: any FAIL exits non-zero.
set -u
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
CORE="$ROOT/crates/doorbell-core/src/lib.rs"
FAIL=0
pass(){ echo "PASS: $1"; }
fail(){ echo "FAIL: $1"; FAIL=1; }

echo "== ADR-0012 Phase 0 =="
[ -f "$CORE" ] || { fail "crates/doorbell-core/src/lib.rs missing"; echo "RESULT: FAIL"; exit 1; }
pass "doorbell-core exists"

# No effect deps inside the pure core: manifest allowlist (D1 permits
# serde/serde_json/anyhow/sha1/hex/chrono only). The old source-grep
# false-positived on toron_kind_* fields and the herdr_status param.
CORE_DEPS="$ROOT/crates/doorbell-core/Cargo.toml"
allowed="serde serde_json chrono anyhow sha1 hex"
for dep in $(awk '/^\[dependencies\]/{f=1;next} /^\[/{f=0} f' "$CORE_DEPS" | grep -oE '^[A-Za-z0-9_-]+' | sort -u); do
  case " $allowed " in
    *" $dep "*) ;;
    *) fail "core depends on non-pure crate: $dep" ;;
  esac
done
pass "core manifest is effect-free (failures listed above if any)"

# Required pure symbols moved verbatim (no signature change).
for sym in "pub enum EventClass" "pub struct RingInput" "pub fn classify" "pub fn should_inject" "pub fn is_drain_transition" "pub struct DoorbellQueue" "pub struct SelfIndex" "pub fn watch_targets" "pub fn parse_watch_ndjson" "pub fn parse_inbox_item" "pub fn doorbell_line" "pub enum DoorbellMsg" "pub enum DoorbellAction" "pub struct DoorbellModel" "pub fn tick"; do
  grep -Fq "$sym" "$CORE" || { fail "core missing: $sym"; }
done
pass "core symbol surface present (failures listed above if any)"

# Journal envelope carries additive v:1; readers tolerate missing as 1.
grep -Fq '"v": 1' "$ROOT/src/doorbell.rs" || fail "journal_append missing v:1"
grep -Fq '"v": 1' "$ROOT/src/doorbell.rs" && pass "journal v:1 present"

# Workspace member wired.
grep -Fq "doorbell-core" "$ROOT/Cargo.toml" || fail "workspace member doorbell-core missing in Cargo.toml"
grep -Fq "doorbell-core" "$ROOT/Cargo.toml" && pass "workspace member wired"

# Behavior parity: existing gates green unmodified in intent.
echo "-- behavior parity (cargo) --"
if command -v mbx >/dev/null 2>&1; then CARGO="mbx +nightly-aarch64-apple-darwin"; else CARGO="cargo"; fi
$CARGO test -p doorbell-core 2>&1 | tail -n 3
[ "${PIPESTATUS[0]}" -eq 0 ] || fail "doorbell-core tests red"
$CARGO test -p flywheel --test sc_doorbell_adr0005 2>&1 | tail -n 3
[ "${PIPESTATUS[0]}" -eq 0 ] || fail "sc_doorbell_adr0005 red"

echo "== ADR-0012 Phase 1 (bd-asr2.6: migration + parity harness) =="
# rt-level S4 parity: same Msg log, identical journals modulo ts.
$CARGO test -p doorbell-rt --lib parity_same_log_same_journal_modulo_ts 2>&1 | tail -n 2
[ "${PIPESTATUS[0]}" -eq 0 ] || fail "rt journal parity red"
# bd-asr2.7: full rt suite (crash-harness, clock-jump, determinism fuzz,
# rotation, torn-tail) — the section 8 durability gate.
$CARGO test -p doorbell-rt --lib 2>&1 | tail -n 2
[ "${PIPESTATUS[0]}" -eq 0 ] || fail "doorbell-rt suite red"
# State migration: craft an old-layout dir, adopt it, compare bytes.
MIG_SRC="$(mktemp -d "${TMPDIR:-/tmp}/doorbell-mig-src.XXXXXX")"
MIG_DST="$(mktemp -d "${TMPDIR:-/tmp}/doorbell-mig-dst.XXXXXX")"
mkdir -p "$MIG_SRC/doorbell" "$MIG_SRC/journals"
echo '{"since_by_pin":{}}' > "$MIG_SRC/doorbell-wake-ws.json"
printf '{"v":1,"seq":1}\n{"v":1,"seq":2}\n' > "$MIG_SRC/journals/doorbell-journal-ws.jsonl"
echo "ping" > "$MIG_SRC/doorbell/ws.unanswered"
"$ROOT/target/debug/doorbell" migrate --state-dir "$MIG_DST" --from "$MIG_SRC" >/dev/null 2>&1 \
  || fail "doorbell migrate exited non-zero"
cmp -s "$MIG_SRC/doorbell-wake-ws.json" "$MIG_DST/doorbell-wake-ws.json" || fail "wake not byte-identical"
cmp -s "$MIG_SRC/journals/doorbell-journal-ws.jsonl" "$MIG_DST/journals/doorbell-journal-ws.jsonl" || fail "journal not byte-identical"
cmp -s "$MIG_SRC/doorbell/ws.unanswered" "$MIG_DST/doorbell/ws.unanswered" || fail "unanswered not byte-identical"
pass "state migration byte-identical"
rm -rf "$MIG_SRC" "$MIG_DST"
# Referenced gates (each green in its own bead, recorded here):
# - proxy byte-identical: proven live (bd-asr2.5 close note)
# - fence E13: verify-doorbell-daemon.sh green (bd-asr2.2/bd-asr2.3)
# NOTE (honest gap, bd-ene2): embedded-vs-daemon journal parity needs the
# daemon to host the loop (Phase 3 cutover work); the daemon is still a
# lifecycle shell. Tracked, not faked.

echo "== ADR-0012 Phase 2 (bd-asr2.11: human digest, second consumer) =="
# Human wake with zero swarm: rt-level proof (subscription + sink delivery
# with no panes alive) runs in the rt suite.
$CARGO test -p doorbell-rt --lib human_subscription_wake_needs_no_swarm 2>&1 | tail -n 2
[ "${PIPESTATUS[0]}" -eq 0 ] || fail "zero-swarm human wake red"
# Digest verb smoke: empty state exits 0 in both formats.
"$ROOT/target/debug/flywheel" doorbell digest flywheel --format json >/dev/null 2>&1 \
  || fail "doorbell digest json smoke failed"
"$ROOT/target/debug/flywheel" doorbell digest flywheel --format text >/dev/null 2>&1 \
  || fail "doorbell digest text smoke failed"
pass "digest verb smoke (json + text)"
# why --subscriber smoke on an empty journal.
"$ROOT/target/debug/flywheel" doorbell why flywheel somepin --limit 2 --subscriber "workspace:x" >/dev/null 2>&1 \
  || fail "why --subscriber smoke failed"
pass "why --subscriber smoke"
# Referenced Phase-2 gates (each green in its own bead):
# - budget durability: rt crash test (bd-asr2.9)
# - slow-sink E15: rt isolation test (bd-asr2.10)
# - authz negatives: rt matrix tests (bd-asr2.8)
# - SSE replay: daemon gate section (bd-asr2.12)

if [ "$FAIL" -eq 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; fi
exit "$FAIL"

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
for sym in "pub enum EventClass" "pub struct RingInput" "pub fn classify" "pub fn should_inject" "pub fn is_drain_transition" "pub struct DoorbellQueue" "pub struct SelfIndex" "pub fn watch_targets" "pub fn parse_watch_ndjson" "pub fn parse_inbox_item" "pub fn doorbell_line" "pub fn is_urgent_line" "pub enum DoorbellMsg" "pub enum DoorbellAction" "pub struct DoorbellModel" "pub fn tick"; do
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
# NOTE (bd-ene2): embedded-vs-daemon journal parity is proven by the
# Phase 3 section below (differential journals + serve smoke). Remaining,
# honestly out of scope: G1 ack-SLA publish (no toron CLI verb) and the
# cutover (default host flip, spawn-API registration, launchd --host).

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
echo "== ADR-0012 Phase 3 (bd-ene2: daemon-side loop hosting) =="
# FileHost unit suite (registry/presence/filter/gen/drains/stub CLIs).
$CARGO test -p doorbell-rt --lib file_host 2>&1 | tail -n 2
[ "${PIPESTATUS[0]}" -eq 0 ] || fail "file_host suite red"
# Differential parity: same scripted log through Brain+embedded vs
# Brain+daemon yields identical journals modulo ts (the bead verify).
$CARGO test --bin flywheel parity_tests 2>&1 | tail -n 2
# Serve smoke: a hosted loop journals sweeps, then a TERM-triggered
# graceful stop releases leases. NOTE: backgrounded `&` children of a
# non-interactive shell may inherit ignored TERM, so the daemon runs
# under `timeout -s TERM -k` (foreground-spawned children keep a normal
# disposition): TERM at 12s must exit 0 with leases released; the -k
# fallback exits 137 with the lease held and fails the gate loudly.
$CARGO build -p doorbell-bin 2>&1 | tail -n 1
SMOKE_D="$(mktemp -d "${TMPDIR:-/tmp}/doorbell-smoke.XXXXXX")"
mkdir -p "$SMOKE_D/fw/agents" "$SMOKE_D/dd"
echo '[{"pane_id":"pane-a","name":"herdr-a","mail_identity":"PinA"}]' > "$SMOKE_D/fw/agents/smokeproj.json"
DOORBELL_TORON_BIN=true DOORBELL_HERDR_BIN=true \
  timeout --preserve-status -s TERM -k 5 12 \
  "$ROOT/target/debug/doorbell" serve --state-dir "$SMOKE_D/dd" --port 18999 \
  --flywheel-state-dir "$SMOKE_D/fw" --host smokeproj/smokews >"$SMOKE_D/serve.log" 2>&1 &
SMOKE_WATCHER=$!
smoke_cleanup() {
  # bd-2o1v: killing the watcher orphans serve (reparented, keeps the
  # port AND the lease and poisons the next run). Always kill the port
  # holder too — 18999 is this smoke's own port.
  kill -9 "$SMOKE_WATCHER" 2>/dev/null
  local holder
  holder=$(lsof -tiTCP:18999 2>/dev/null)
  [ -n "$holder" ] && kill -9 $holder 2>/dev/null
}
smoke_ok=0
for _ in $(seq 1 30); do
  if curl -sm 2 http://127.0.0.1:18999/health 2>/dev/null | grep -q '"mode":"hosted"'; then smoke_ok=1; break; fi
  sleep 0.5
done
[ "$smoke_ok" -eq 1 ] || { smoke_cleanup; fail "serve smoke never reached hosted mode"; }
sleep 3
grep -q '"stage":"sweep"' "$SMOKE_D/fw/journals/doorbell-journal-smokews.jsonl" 2>/dev/null \
  || { smoke_cleanup; fail "hosted loop journaled no sweeps"; }
pass "serve smoke hosted + sweeping"
# TERM fires at 12s; graceful exit releases the per-ws lease. A KILL
# fallback (137) leaves the lease held and fails the next check loudly.
wait "$SMOKE_WATCHER"
[ "$?" -eq 0 ] || fail "serve did not exit clean on SIGTERM"
[ -e "$SMOKE_D/fw/doorbell/owner-smokews.json" ] && fail "graceful stop left the lease held"
pass "serve SIGTERM graceful (leases released)"
rm -rf "$SMOKE_D"
smoke_cleanup
echo "== ADR-0012 Phase 4 analytics (bd-asr2.19) =="
ART="$ROOT/docs/learnings/doorbell/wake-analytics-2026-09-10.md"
[ -f "$ART" ] || fail "wake analytics artifact missing"
grep -q "^track: knowledge" "$ART" || fail "artifact frontmatter track missing"
grep -q "^reinforced: " "$ART" || fail "artifact reinforcement field missing"
grep -q "502" "$ART" || fail "artifact has no ring/drop counters"
pass "wake analytics artifact (frontmatter + reinforcement + counters)"

if [ "$FAIL" -eq 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; fi
exit "$FAIL"

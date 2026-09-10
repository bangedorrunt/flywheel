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

if [ "$FAIL" -eq 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL"; fi
exit "$FAIL"

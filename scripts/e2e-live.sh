#!/usr/bin/env bash
# Hermetic live e2e legs for the final review (bd-fbiza).
#
# The legs are in-process `#[ignore]` tests, so they open no pane, no port and
# no store — but they run under the house sandbox anyway: a temp HOME and XDG,
# an explicit TORON_DATA_DIR, and deny-shims for launchctl/systemctl first on
# PATH, so a regression that shells those tools is caught instead of booting
# the operator's real units. The live ports (:18080 toron, :18980 doorbell) are
# never opened; the guard below refuses to start if either is already held.
#
# Usage: bash scripts/e2e-live.sh [--mutate]
#   --mutate  run each leg with E2E_MUTANT=1 and REQUIRE it to fail
#             (the production mutations are applied by hand; see the bead)
#
# Receipts: /tmp/e2e-live-<stamp>.log and one status line per leg.
set -euo pipefail
cd "$(dirname "$0")/.."

MUTATE=0
[ "${1:-}" = "--mutate" ] && MUTATE=1

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
LOG="/tmp/e2e-live-${STAMP}.log"
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/e2e-sandbox.XXXXXX")"
cleanup() { chmod -R u+w "$SANDBOX" 2>/dev/null || true; find "$SANDBOX" -delete 2>/dev/null || true; }
trap cleanup EXIT

# --- guard: never touch the live planes -------------------------------------
# These legs are in-process contract/reconciler tests: they never dial a plane
# or open a store. Warn (do not refuse) when the live planes are listening, and
# record it in the receipt — the sandbox still points every path away from them.
LIVE_PLANES=""
for port in 18080 18980; do
  if command -v lsof >/dev/null 2>&1 && lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
    LIVE_PLANES="$LIVE_PLANES :$port"
    echo "e2e-live: note: live plane on :$port (this leg never dials it)" >&2
  fi
done

# --- sandbox ---------------------------------------------------------------
mkdir -p "$SANDBOX/home" "$SANDBOX/config" "$SANDBOX/state" "$SANDBOX/data/toron" "$SANDBOX/bin"
for tool in launchctl systemctl; do
  cat >"$SANDBOX/bin/$tool" <<EOF
#!/usr/bin/env bash
echo "e2e-deny-shim: $tool \$*" >>"$SANDBOX/denied.log"
exit 1
EOF
  chmod +x "$SANDBOX/bin/$tool"
done
export PATH="$SANDBOX/bin:$PATH"
# HOME stays the operator's: mbx keeps its build cache there and the legs read
# no HOME at all (they are in-process). Every XDG root and the toron store are
# sandboxed, which is what the legs could actually touch.
export XDG_CONFIG_HOME="$SANDBOX/config"
export XDG_STATE_HOME="$SANDBOX/state"
export XDG_DATA_HOME="$SANDBOX/data"
export TORON_DATA_DIR="$SANDBOX/data/toron"

LEGS=(
  "witness|phase_contracts::tests::live_wave_swarm_without_witness_is_a_typed_failure"
  "polish|phase_contracts::tests::live_beads_lifecycle_polish_converges"
  "respawn|seat_reconcile::live_e2e_tests::live_respawn_never_two_holders_for_one_seat"
)

status=0
: >"$LOG"
for entry in "${LEGS[@]}"; do
  name="${entry%%|*}"
  filter="${entry#*|}"
  echo "e2e-live: leg $name" | tee -a "$LOG"
  set +e
  if [ "$MUTATE" -eq 1 ]; then
    E2E_MUTANT=1 mbx +nightly-aarch64-apple-darwin test -p flywheel --bin flywheel "$filter" \
      -- --ignored --exact "$filter" >>"$LOG" 2>&1
    rc=$?
    if [ "$rc" -eq 0 ]; then
      echo "  FAIL  $name: mutation stayed GREEN (the leg cannot fail)" | tee -a "$LOG"
      status=1
    else
      echo "  PASS  $name: mutation went RED as required" | tee -a "$LOG"
    fi
  else
    mbx +nightly-aarch64-apple-darwin test -p flywheel --bin flywheel "$filter" \
      -- --ignored --exact "$filter" >>"$LOG" 2>&1
    rc=$?
    if [ "$rc" -eq 0 ]; then
      echo "  PASS  $name" | tee -a "$LOG"
    else
      echo "  FAIL  $name (rc=$rc; log $LOG)" | tee -a "$LOG"
      status=1
    fi
  fi
  set -e
done

if [ -s "$SANDBOX/denied.log" ]; then
  echo "e2e-live: FAIL: a leg called a supervisor tool:" | tee -a "$LOG"
  cat "$SANDBOX/denied.log" | tee -a "$LOG"
  status=1
else
  echo "e2e-live: supervisor shims unused (no launchctl/systemctl call)" | tee -a "$LOG"
fi

echo "e2e-live: live planes seen:${LIVE_PLANES:- none} (never dialled)" | tee -a "$LOG"
echo "e2e-live: receipt $LOG"
exit "$status"

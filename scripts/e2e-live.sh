#!/usr/bin/env bash
# Sandboxed run of the final-review legs (bd-fbiza).
# Usage: bash scripts/e2e-live.sh
# Exit: 0 all legs green, 1 a leg failed, 2 sandbox refused.
set -euo pipefail
cd "$(dirname "$0")/.."

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
LOG="/tmp/e2e-live-${STAMP}.log"
SANDBOX="$(mktemp -d "${TMPDIR:-/tmp}/e2e-sandbox.XXXXXX")"
cleanup() { chmod -R u+w "$SANDBOX" 2>/dev/null || true; find "$SANDBOX" -delete 2>/dev/null || true; }
trap cleanup EXIT

mkdir -p "$SANDBOX/bin" "$SANDBOX/home" "$SANDBOX/config" "$SANDBOX/state" "$SANDBOX/data/toron"
for tool in launchctl systemctl; do
  printf '#!/usr/bin/env bash\necho "denied: %s $*" >>"%s/denied.log"\nexit 1\n' "$tool" "$SANDBOX" >"$SANDBOX/bin/$tool"
  chmod +x "$SANDBOX/bin/$tool"
done
export PATH="$SANDBOX/bin:$PATH"
# HOME is sandboxed; the toolchain caches are re-pointed at the real ones so a
# build does not populate (or re-link) anything inside the sandbox.
export HOME="$SANDBOX/home"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export MBX_CMD="${MBX_CMD:-mbx +nightly-aarch64-apple-darwin}"
export XDG_CONFIG_HOME="$SANDBOX/config"
export XDG_STATE_HOME="$SANDBOX/state"
export XDG_DATA_HOME="$SANDBOX/data"
export TORON_DATA_DIR="$SANDBOX/data/toron"
export CARGO_TARGET_DIR="$SANDBOX/target"

# These legs are in-process; the live plane may be up and is never dialled.

status=0
: >"$LOG"
run_leg() {
  local name="$1" filter="$2"
  echo "e2e-live: $name" | tee -a "$LOG"
  set +e
  $MBX_CMD test -p flywheel --bin flywheel "$filter" -- --exact "$filter" >>"$LOG" 2>&1
  local rc=$?
  set -e
  if [ "$rc" -eq 0 ]; then
    echo "  PASS  $name" | tee -a "$LOG"
  else
    echo "  FAIL  $name (rc=$rc, log $LOG)" | tee -a "$LOG"
    status=1
  fi
}

run_leg witness phase_contracts::tests::wave_swarm_without_witness_is_a_typed_failure
run_leg polish phase_contracts::tests::beads_lifecycle_polish_converges
run_leg respawn seat_reconcile::live_e2e_tests::respawn_never_two_holders_for_one_seat

if [ -s "$SANDBOX/denied.log" ]; then
  echo "e2e-live: FAIL: a leg called a supervisor tool" | tee -a "$LOG"
  status=1
fi

echo "e2e-live: receipt $LOG"
exit "$status"

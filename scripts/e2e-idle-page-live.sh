#!/usr/bin/env bash
# Live POSITIVE proof for bd-iscat: a crew pane that is WORKING, holds an
# unread captain brief, and goes IDLE without the ring must page the captain
# watch, naming the pin and the brief.
#
# Why this is not part of scripts/e2e-live.sh: a sandboxed pane cannot be made
# genuinely working — the harness needs an authenticated agent in front, and no
# sandbox has one. The sandbox leg proves the other half (no false page); this
# script is the real-fleet half, run against a live crew on purpose.
#
# Usage: scripts/e2e-idle-page-live.sh [project] [pin] [timeout_secs]
#   Arm it while the pin is working (or prompt the pin while it is armed), then
#   let the pin go idle. Exits 0 only when the watch exits naming that pin.
# Receipt: /tmp/e2e-idle-page-live.<stamp>.log
set -euo pipefail
cd "$(dirname "$0")/.."

PROJECT="${1:-flywheel}"
PIN="${2:-${AGENT_NAME:-}}"
TIMEOUT="${3:-180}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
LOG="/tmp/e2e-idle-page-live.${STAMP}.log"

[ -n "$PIN" ] || { echo "usage: $0 [project] [pin] — or set AGENT_NAME" >&2; exit 2; }

echo "e2e-idle-page-live: project=$PROJECT pin=$PIN timeout=${TIMEOUT}s receipt=$LOG"

# The watch is the product's one-shot poll-exit door: arm it, let the pane work
# and then idle, and require it to exit with an idle page instead of a stall.
(
  flywheel captain watch "$PROJECT" --interval 2 --json >"$LOG" 2>&1
  echo "watch_exit=$?" >>"$LOG"
) &
WATCH_PID=$!

deadline=$(( $(date +%s) + TIMEOUT ))
while kill -0 "$WATCH_PID" 2>/dev/null; do
  if [ "$(date +%s)" -ge "$deadline" ]; then
    kill "$WATCH_PID" 2>/dev/null || true
    wait "$WATCH_PID" 2>/dev/null || true
    printf '  FAIL  live positive: the watch never paged within %ss (receipt %s)\n' \
      "$TIMEOUT" "$LOG" >&2
    sed -n '1,20p' "$LOG" >&2 || true
    exit 1
  fi
  sleep 2
done
wait "$WATCH_PID" 2>/dev/null || true

if grep -q '"reason": *"idle"' "$LOG" && grep -q "\"$PIN\"" "$LOG"; then
  printf '  PASS  live positive: the watch exited naming %s with its unread brief\n' "$PIN"
  sed -n '1,20p' "$LOG"
  exit 0
fi

printf '  FAIL  live positive: the watch exited without an idle page for %s (receipt %s)\n' \
  "$PIN" "$LOG" >&2
sed -n '1,20p' "$LOG" >&2 || true
exit 1

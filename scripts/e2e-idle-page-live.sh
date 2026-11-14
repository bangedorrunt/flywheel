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

# The watch is the product's one-shot poll-exit door. It is armed repeatedly:
# any unrelated new mail legitimately ends it (reason "mail"), including crew
# mail that arrives while we wait for the transition, so the leg re-arms and
# keeps its deadline. Only an idle page naming the pin is the answer.
deadline=$(( $(date +%s) + TIMEOUT ))
attempt=0
armed_pages=0
while [ "$(date +%s)" -lt "$deadline" ]; do
  attempt=$((attempt + 1))
  run="$LOG.arm$attempt"
  flywheel captain watch "$PROJECT" --interval 2 --json >"$run" 2>&1 &
  WATCH_PID=$!
  while kill -0 "$WATCH_PID" 2>/dev/null; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
      kill "$WATCH_PID" 2>/dev/null || true
      wait "$WATCH_PID" 2>/dev/null || true
      printf '  FAIL  live positive: no idle page for %s in %ss (%s arms, %s unrelated pages)\n' \
        "$PIN" "$TIMEOUT" "$attempt" "$armed_pages" >&2
      sed -n '1,10p' "$run" >&2 || true
      exit 1
    fi
    sleep 2
  done
  wait "$WATCH_PID" 2>/dev/null || true

  if grep -q '"reason": *"idle"' "$run" && grep -q "\"$PIN\"" "$run"; then
    printf '  PASS  live positive: the watch exited naming %s with its unread brief (arm %s)\n' \
      "$PIN" "$attempt"
    sed -n '1,20p' "$run"
    cp "$run" "$LOG" 2>/dev/null || true
    exit 0
  fi
  if grep -q '"reason": *"mail"' "$run"; then
    armed_pages=$((armed_pages + 1))
    continue
  fi
  printf '  FAIL  live positive: the watch exited for a reason that is not an idle page (%s)\n' \
    "$run" >&2
  sed -n '1,20p' "$run" >&2 || true
  exit 1
done

printf '  FAIL  live positive: deadline passed with no idle page for %s\n' "$PIN" >&2
exit 1

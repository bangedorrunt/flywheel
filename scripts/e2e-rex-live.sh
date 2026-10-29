#!/usr/bin/env bash
# Live rex-backend e2e (bd-rex-live-e2e): the operator-run proof that the
# rex pane plane dispatches end to end — spawn a pane, deliver one raw task
# through `flywheel send`, and require the agent to actually RUN it and
# report the marker. Not part of `cargo test`: it mutates the operator's real
# rex workspace, so it is a script, like the other e2e-* doors.
#
# Usage:  scripts/e2e-rex-live.sh [project]
# Env:    AGENT_NAME (required — a send with no pin is refused)
#         E2E_TIMEOUT_SECS (default 180)
#
# Exit: 0 only when the marker appears in the pane's output.
set -euo pipefail
cd "$(dirname "$0")/.."

PROJECT="${1:-flywheel}"
TIMEOUT="${E2E_TIMEOUT_SECS:-180}"
MARK="REX-LIVE-OK"

: "${AGENT_NAME:?set AGENT_NAME to your pin — flywheel send refuses without a sender}"

say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAIL: %s\n' "$*" >&2; exit 1; }

say "plane"
flywheel backend list --json 2>/dev/null \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(" active:", [b["name"] for b in d["backends"] if b.get("active")])' \
  || true

say "spawn on the selected plane (rex by default)"
SPAWN_OUT=$(flywheel spawn "$PROJECT" --kind opencode=1 --retire-orphans --yes 2>&1) \
  || die "spawn failed: $SPAWN_OUT"
printf '%s\n' "$SPAWN_OUT" | grep -qE 'block:|:[pw][0-9]+' || die "spawn reported no pane: $SPAWN_OUT"

say "send one raw task (waits for the landing receipt)"
HEAD_SHA=$(git rev-parse --short HEAD)
SEND_OUT=$(flywheel send "$PROJECT" --to all --raw \
  "Live rex-backend test. Do exactly one thing: run the shell command \
'git rev-parse --short HEAD' in $(pwd), then reply with the single line: \
$MARK $HEAD_SHA" --wait --timeout-ms 120000 2>&1) || true
printf '%s\n' "$SEND_OUT" | tail -n 20

say "assert the agent ran it (marker in the pane output)"
# rex capture takes the SHORT block id (`58cd49`), not the registry's
# `block:<uuid>` spelling — accept either by resolving through rex itself.
SESSION=$(rex list 2>/dev/null | awk -v l="flywheel-$PROJECT" '$1==l {print $2; exit}')
[ -n "$SESSION" ] || die "no rex session labelled flywheel-$PROJECT"

deadline=$(( $(date +%s) + TIMEOUT ))
while [ "$(date +%s)" -lt "$deadline" ]; do
  for b in $(rex block list -s "$SESSION" 2>/dev/null | awk 'NR>1 {print $2}'); do
    if rex capture -s "$SESSION" -b "$b" 2>/dev/null | grep -q "$MARK $HEAD_SHA"; then
      say "PASS: $MARK $HEAD_SHA observed in pane $b"
      exit 0
    fi
  done
  sleep 5
done

die "marker '$MARK $HEAD_SHA' never appeared within ${TIMEOUT}s — the prompt did not dispatch, or the agent did not run it"

#!/usr/bin/env bash
# captain-reconcile.sh — stale toron binding reconcile on close.
# Diffs the last snapshot against live panes: vanished captain panes get a
# gone presence row and leave the snapshot. Identity rows are never
# mutated (no such agent CLI exists); the presence row IS the binding
# signal the presence projection reads. Never dispatches, closes, or writes beads.
# Always exits 0.
set -u

HERDR="${HERDR_BIN_PATH:-herdr}"
STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-/tmp}"
SNAP="$STATE_DIR/captain-pane-snapshot.json"

[ -f "$SNAP" ] || exit 0
LIVE="$("$HERDR" agent list 2>/dev/null | jq -r '.result.agents[]?.pane_id' 2>/dev/null)" || exit 0

GONE="$(jq -r --argjson live "$(printf '%s\n' "$LIVE" | jq -R . | jq -s .)" \
  '.panes[]? | select(.pane_id as $p | $live | index($p) | not) | "\(.pane_id)	\(.pin)	\(.project // "flywheel")"' "$SNAP" 2>/dev/null)"

if [ -z "$GONE" ]; then exit 0; fi

FAILED=""
while IFS='	' read -r pane pin project; do
  [ -n "$pane" ] || continue
  if toron presence publish --project "$project" --as "$pin" \
      --state working --status gone 2>/dev/null; then
    echo "captain-reconcile: $pin ($pane) -> gone"
  else
    # Unpublished stays tracked so the next close retries it. Dropping it
    # would silently re-open the black hole this edge exists to close.
    echo "captain-reconcile: publish $pin=gone failed, keeping for retry" >&2
    FAILED="$FAILED $pane"
  fi
done <<< "$GONE"

# Prune the marked-gone from the snapshot; keep the live plus any
# unpublished (retry on the next close).
FAILED_JSON="$(printf '%s\n' $FAILED | jq -R . | jq -s .)"
jq --argjson live "$(printf '%s\n' "$LIVE" | jq -R . | jq -s .)" \
  --argjson failed "$FAILED_JSON" \
  '.panes |= map(select(.pane_id as $p | ($live | index($p)) or ($failed | index($p))))' \
  "$SNAP" > "$SNAP.tmp" 2>/dev/null && mv "$SNAP.tmp" "$SNAP"
exit 0

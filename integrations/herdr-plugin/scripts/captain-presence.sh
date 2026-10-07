#!/usr/bin/env bash
# captain-presence.sh — upsert captain-pane presence on every
# lifecycle edge. Read-and-notify only: publishes toron presence rows,
# writes the last snapshot. Never dispatches, closes, or writes beads.
# Always exits 0 — a hook must never break herdr's event loop.
set -u

HERDR="${HERDR_BIN_PATH:-herdr}"
STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-/tmp}"
SNAP="$STATE_DIR/captain-pane-snapshot.json"
EVENT="${HERDR_PLUGIN_EVENT:-unknown}"
EVENT_JSON="${HERDR_PLUGIN_EVENT_JSON:-{}}"

# The event pane: payload first, ambient pane id second, empty last.
PANE_ID="$(printf '%s' "$EVENT_JSON" | jq -r '.pane_id // empty' 2>/dev/null)"
if [ -z "$PANE_ID" ]; then
  PANE_ID="${HERDR_PANE_ID:-}"
fi

# Captain panes: herdr agents carrying a toron_pin token (pin + project).
AGENTS="$("$HERDR" agent list 2>/dev/null)" || { echo "captain-presence: agent list failed" >&2; exit 0; }
# No mapfile: stock macOS ships bash 3.2 and the manifest declares
# macos support. Rows carry real tabs (jq interprets \t); cut -f
# splits on TAB by definition.
CAPTAIN_ROWS="$(printf '%s' "$AGENTS" | jq -r '.result.agents[]? | select(.tokens.toron_pin != null) | "\(.pane_id)\t\((.tokens.toron_pin))\t\((.tokens.toron_project // "flywheel"))\t\(.agent_status // "unknown")"' 2>/dev/null)"

gone_now() {
  case "$EVENT" in
    pane.closed|pane.exited) return 0 ;;
    *) return 1 ;;
  esac
}

printf '%s\n' "$CAPTAIN_ROWS" | while IFS= read -r row; do
  pane="$(printf '%s' "$row" | cut -f1)"
  pin="$(printf '%s' "$row" | cut -f2)"
  project="$(printf '%s' "$row" | cut -f3)"
  [ "$pane" = "$PANE_ID" ] || continue
  if gone_now; then state="gone"; else state="live"; fi
  # toron_pin token + live/gone rides the presence row the captain reads.
  # Env (incl. TORON_MCP_URL) inherits untouched; toron failure warns only.
  if ! toron presence publish \
      --project "$project" --as "$pin" --state working --status "$state" 2>/dev/null; then
    echo "captain-presence: publish $pin=$state failed (toron unreachable?)" >&2
  else
    echo "captain-presence: $pin ($pane) -> $state"
  fi
done

# Last snapshot: full captain-pane map for the reconcile edge.
{
  printf '%s' "$AGENTS" | jq '{ts: now | floor, panes: [.result.agents[]? | select(.tokens.toron_pin != null) | {pane_id, pin: .tokens.toron_pin, project: (.tokens.toron_project // "flywheel"), status: (.agent_status // "unknown")}]}' 2>/dev/null > "$SNAP.tmp" \
    && mv "$SNAP.tmp" "$SNAP" \
    || echo "captain-presence: snapshot write failed" >&2
} 2>/dev/null
exit 0

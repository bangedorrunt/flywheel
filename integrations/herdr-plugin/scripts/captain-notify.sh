#!/usr/bin/env bash
# captain-notify.sh — toast on urgent captain-pane transitions.
# Hooked to pane.agent_status_changed only. A blocked captain pane gets
# `notification.show` with sound=request (mention/urgent proxy: lifecycle
# edges carry no message content, so a blocked status IS the urgent
# signal). Anything else exits silent. Always exits 0.
set -u

HERDR="${HERDR_BIN_PATH:-herdr}"
EVENT_JSON="${HERDR_PLUGIN_EVENT_JSON:-{}}"

PANE_ID="$(printf '%s' "$EVENT_JSON" | jq -r '.pane_id // empty' 2>/dev/null)"
[ -n "$PANE_ID" ] || PANE_ID="${HERDR_PANE_ID:-}"
[ -n "$PANE_ID" ] || exit 0

# New status: payload first, live query fallback.
STATUS="$(printf '%s' "$EVENT_JSON" | jq -r '.agent_status // .status // .to // empty' 2>/dev/null)"
if [ -z "$STATUS" ]; then
  STATUS="$("$HERDR" agent list 2>/dev/null | jq -r --arg p "$PANE_ID" '.result.agents[]? | select(.pane_id == $p) | (.agent_status // empty)' 2>/dev/null | head -n 1)"
fi
[ "$STATUS" = "blocked" ] || exit 0

PIN="$("$HERDR" agent list 2>/dev/null | jq -r --arg p "$PANE_ID" '.result.agents[]? | select(.pane_id == $p and .tokens.toron_pin != null) | .tokens.toron_pin' 2>/dev/null | head -n 1)"
[ -n "$PIN" ] || exit 0

"$HERDR" notification show "Captain pane $PIN blocked" \
  --body "pane $PANE_ID needs the captain" \
  --sound request 2>/dev/null \
  && echo "captain-notify: toasted $PIN ($PANE_ID)" \
  || echo "captain-notify: toast failed for $PIN (headless?)" >&2
exit 0

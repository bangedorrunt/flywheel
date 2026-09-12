#!/usr/bin/env bash
# GS-043: single peer-aware SoT gate — spawn inject is canonical; prompts/
# chrome must stay thin and point at it. Refuses a second full body.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SOT="$ROOT/integrations/peer-aware.md.tpl"
CHROME="$ROOT/integrations/prompts/peer-aware.md.tpl"

test -f "$SOT" || { echo "missing SoT: $SOT"; exit 1; }
test -f "$CHROME" || { echo "missing chrome: $CHROME"; exit 1; }

sot_bytes=$(wc -c < "$SOT" | tr -d ' ')
chrome_bytes=$(wc -c < "$CHROME" | tr -d ' ')
sot_hash=$(sha256sum "$SOT" | awk '{print $1}')

echo "peer-aware SoT: $sot_bytes bytes sha256=$sot_hash"
echo "peer-aware chrome: $chrome_bytes bytes"

if (( chrome_bytes * 2 >= sot_bytes )); then
  echo "FAIL: prompts/peer-aware.md.tpl is not thin chrome ($chrome_bytes vs SoT $sot_bytes)"
  exit 1
fi

if grep -q 'OPERATING CONTRACT' "$CHROME"; then
  echo "FAIL: chrome must not embed OPERATING CONTRACT (SoT body)"
  exit 1
fi

if ! grep -q 'integrations/peer-aware.md.tpl' "$CHROME"; then
  echo "FAIL: chrome must name the SoT path"
  exit 1
fi

# Code paths that inject/spawn must reference the SoT path (not only prompts/).
if ! grep -q 'integrations/peer-aware.md.tpl' "$ROOT/src/main.rs"; then
  echo "FAIL: main.rs must reference integrations/peer-aware.md.tpl"
  exit 1
fi

echo "OK: peer-aware single SoT gate"

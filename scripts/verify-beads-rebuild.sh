#!/usr/bin/env bash
set -euo pipefail

# D6/R3 proof: verify that flushing .beads/issues.jsonl and rebuilding a fresh
# SQLite DB from JSONL + config.yaml produces the identical `br ready --json` ID set.
# Runs hermetically in a temp copy; never mutates the source repository.

REPO_DIR="${1:-.}"
REPO_DIR="$(cd "$REPO_DIR" && pwd)"

if [[ ! -d "$REPO_DIR/.beads" ]]; then
  echo "error: no .beads directory in $REPO_DIR" >&2
  exit 2
fi

TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/verify-beads-rebuild.XXXXXX")"
trap 'rm -rf "$TMP_ROOT"' EXIT

mkdir -p "$TMP_ROOT/src" "$TMP_ROOT/rebuilt/.beads"
cp -R "$REPO_DIR/.beads" "$TMP_ROOT/src/.beads"

(
  cd "$TMP_ROOT/src"
  br sync --flush-only >/dev/null
)

cp "$TMP_ROOT/src/.beads/issues.jsonl" "$TMP_ROOT/rebuilt/.beads/issues.jsonl"
if [[ -f "$TMP_ROOT/src/.beads/config.yaml" ]]; then
  cp "$TMP_ROOT/src/.beads/config.yaml" "$TMP_ROOT/rebuilt/.beads/config.yaml"
fi

(
  cd "$TMP_ROOT/rebuilt"
  br init --quiet >/dev/null 2>&1 || br init >/dev/null
  br sync --import-only >/dev/null
)

extract_ready_ids() {
  local dir="$1"
  (
    cd "$dir"
    br ready --json
  ) | python3 -c '
import json, sys
data = json.load(sys.stdin)
items = data if isinstance(data, list) else data.get("issues", data.get("items", []))
ids = sorted(str(item.get("id", "")) for item in items if isinstance(item, dict) and item.get("id"))
print("\n".join(ids))
'
}

SRC_IDS="$(extract_ready_ids "$TMP_ROOT/src")"
REBUILT_IDS="$(extract_ready_ids "$TMP_ROOT/rebuilt")"

if [[ "$SRC_IDS" != "$REBUILT_IDS" ]]; then
  echo "FAIL: rebuilt br ready --json ID set differs from source" >&2
  diff -u <(printf '%s\n' "$SRC_IDS") <(printf '%s\n' "$REBUILT_IDS") >&2 || true
  exit 1
fi

COUNT="$(printf '%s\n' "$REBUILT_IDS" | grep -c . || true)"
echo "PASS: br sync --flush-only -> --import-only rebuilt identical ready ID set ($COUNT issues)"

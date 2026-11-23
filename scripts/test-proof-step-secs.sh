#!/usr/bin/env bash
# bd-p7oim: the receipt must carry per-step wall time. No cargo, no daemon —
# a fake mutation file and a direct proof_write_receipt call are enough.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=proof-lib.sh
. "$ROOT/scripts/proof-lib.sh"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/proof-step-secs.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
: >"$TMP/mutations.txt"

receipt="$TMP/receipt.json"
proof_write_receipt "$receipt" 0 deadbeef dead 20260101T000000Z head "$TMP" "" \
  "build"$'\n'"tests" "0"$'\n'"0" "" "$TMP/mutations.txt" $'12\n345\n' >/dev/null
python3 - "$receipt" <<'PY'
import json, sys
d = json.load(open(sys.argv[1], encoding="utf-8"))
got = [(s["name"], s["secs"]) for s in d["steps"]]
assert got == [("build", 12), ("tests", 345)], got
PY
echo "PASS: each receipt step carries its own seconds"

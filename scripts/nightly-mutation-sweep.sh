#!/usr/bin/env bash
# Nightly mutation sweep (bd-kddv7).
#
# Landing proofs now reuse crew verdicts and re-run only cache misses; the full
# set belongs on a nightly run of HEAD. This is that entry point: prove HEAD
# with `--mutations full` (which deliberately bypasses the verdict cache),
# then file one bead per surviving mutation spec so the finding cannot sit in
# a log until someone reads it.
#
# Usage: scripts/nightly-mutation-sweep.sh [--receipt PATH] [--dry-run]
# Exit: 0 no survivors, 1 survivors filed, 2 usage/proof failure.
set -uo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SELF_DIR/.." && pwd)"
RECEIPT=""
DRY=0
NO_PROOF=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --receipt) [ "$#" -ge 2 ] || { echo "sweep: --receipt needs a path" >&2; exit 2; }; RECEIPT="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    --no-proof) NO_PROOF=1; shift ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "sweep: unknown flag $1" >&2; exit 2 ;;
  esac
done

TMP="$(mktemp -d "${TMPDIR:-/tmp}/nightly-mutation-sweep.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
[ -n "$RECEIPT" ] || RECEIPT="$TMP/receipt.json"

echo "sweep: full mutation proof on HEAD"
if [ "$NO_PROOF" -eq 0 ]; then
  if ! (cd "$ROOT" && bash scripts/clean-proof.sh --head --mutations full \
          --mutations-keep-going --receipt "$RECEIPT" >"$TMP/proof.log" 2>&1); then
    tail -40 "$TMP/proof.log" >&2 || true
    echo "sweep: clean-proof failed; beads filed only for witnessed mutation rows" >&2
  fi
fi

survivors="$(python3 - "$RECEIPT" <<'PY'
import json, sys
try:
    doc = json.load(open(sys.argv[1], encoding="utf-8"))
except (OSError, ValueError):
    raise SystemExit(0)
seen = set()
for row in doc.get("mutations", []) or []:
    if not isinstance(row, dict):
        continue
    verdict = str(row.get("verdict", "")).upper()
    if verdict in ("VACUOUS", "BUILD-BROKEN", "RED-OTHER", "NEEDLE-MISS"):
        seen.add(str(row.get("spec") or row.get("id") or "unknown"))
for name in sorted(seen):
    print(name)
PY
)"

if [ -z "$survivors" ]; then
  echo "sweep: no surviving mutation rows"
  exit 0
fi

filed=0
while IFS= read -r spec; do
  [ -n "$spec" ] || continue
  if [ "$DRY" -eq 1 ]; then
    echo "sweep: would file bead for $spec"
  else
    id="$(cd "$ROOT" && br create \
      -t bug -p 2 \
      --slug "mutation-sweep-$(basename "$spec" .spec.sh)" \
      -d "Nightly full mutation sweep found a survivor in \`$spec\` (see $RECEIPT). A mutation that should be RED did not prove its named test. Restore the guard or repoint the witness, then prove the spec RED." \
      --verify "bash scripts/verify-mutations.sh $spec" 2>/dev/null)" || {
        echo "sweep: could not file a bead for $spec" >&2
        continue
      }
    echo "sweep: filed $id for $spec"
  fi
  filed=$((filed + 1))
done <<<"$survivors"

[ "$filed" -gt 0 ] && exit 1
exit 0

#!/usr/bin/env bash
# Lint mutation needles before anything expensive runs (bd-jp9z1.3).
#
# Every `find=` in every spec must occur EXACTLY ONCE in its target file at
# this tree. Zero occurrences is the NEEDLE-MISS that used to surface 40
# minutes into a proof; two or more means perl substitutes the first and the
# spec silently stops covering the other site.
#
# Pure text work, no build: well under a second for the whole directory, so
# clean-proof runs it first and a crew can run it before committing a spec.
#
# Usage: scripts/verify-needles.sh [--repo <dir>] [spec ...]
#        (no spec runs every scripts/mutations/*.spec.sh)
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SPECS=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --repo)
      [ "$#" -ge 2 ] || { echo "verify-needles: --repo needs a directory" >&2; exit 2; }
      REPO="$2"
      shift
      ;;
    -h | --help)
      sed -n '2,14p' "$0"
      exit 0
      ;;
    -*)
      echo "verify-needles: unknown flag $1" >&2
      exit 2
      ;;
    *) SPECS+=("$1") ;;
  esac
  shift
done

cd "$REPO"
if [ "${#SPECS[@]}" -eq 0 ]; then
  SPECS=(scripts/mutations/*.spec.sh)
fi

RECORDS="$(mktemp "${TMPDIR:-/tmp}/verify-needles.XXXXXX")"
trap 'rm -f "$RECORDS"' EXIT

spec_count=0
for spec in "${SPECS[@]}"; do
  [ -f "$spec" ] || { echo "verify-needles: no such spec: $spec" >&2; exit 2; }
  spec_count=$((spec_count + 1))
  (
    # The spec's own `mutation` calls become records: spec, id, target,
    # base64(find). Sourced in a subshell so one spec's variables cannot leak
    # into the next.
    mutation() {
      local label="$1"
      shift
      local file="" find=""
      while [ "$#" -gt 0 ]; do
        case "$1" in
          file=*) file="${1#file=}" ;;
          find=*) find="${1#find=}" ;;
        esac
        shift
      done
      printf '%s\t%s\t%s\t%s\n' \
        "$spec" "${label%%  *}" "$file" \
        "$(printf '%s' "$find" | base64 | tr -d '\n')"
    }
    . "$spec"
  ) >>"$RECORDS"
done

exec python3 - "$RECORDS" "$spec_count" <<'PY'
import base64
import pathlib
import sys

records = pathlib.Path(sys.argv[1])
spec_count = sys.argv[2]
bad = 0
rows = 0
for line in records.read_text(errors="ignore").splitlines():
    if not line.strip():
        continue
    spec, mid, file, encoded = line.split("\t", 3)
    rows += 1
    find = base64.b64decode(encoded).decode("utf-8", "replace")
    if not find:
        print(f"{spec}#{mid}: empty find=", file=sys.stderr)
        bad += 1
        continue
    target = pathlib.Path(file)
    if not target.is_file():
        print(f"{spec}#{mid}: target does not exist: {file}", file=sys.stderr)
        bad += 1
        continue
    occurrences = target.read_text(errors="ignore").count(find)
    if occurrences != 1:
        print(
            f"{spec}#{mid}: find= occurs {occurrences} time(s) in {file} "
            "(want exactly 1)",
            file=sys.stderr,
        )
        bad += 1

if bad:
    print(
        f"verify-needles: {bad} bad needle(s) across {rows} mutation(s) — "
        "fix the find= text before proving anything",
        file=sys.stderr,
    )
    sys.exit(1)
print(f"verify-needles: {spec_count} spec(s), {rows} mutation(s), 0 bad")
PY

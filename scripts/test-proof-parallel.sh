#!/usr/bin/env bash
# bd-p7oim: after the build, the sibling suites and the flywheel suite must
# run at the same time, not one after the other. No cargo, no daemon.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/proof-parallel.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

python3 - "$ROOT/scripts/clean-proof.sh" "$TMP/fn.sh" <<'PY'
import sys
src = open(sys.argv[1], encoding="utf-8").read()
parts = []
for marker in ("\nrun_parallel_step() {", "\ncollect_parallel() {"):
    start = src.index(marker) + 1
    end = src.index("\n}\n", start) + 3
    parts.append(src[start:end])
open(sys.argv[2], "w", encoding="utf-8").write("\n".join(parts))
PY

STEP_NAMES=""
STEP_STATUSES=""
STEP_SECS=""
FAILED_STEP=""
PARALLEL_NAMES=""
# shellcheck source=/dev/null
. "$TMP/fn.sh"

started="$(date +%s)"
run_parallel_step alpha sleep 2
run_parallel_step beta sleep 2
collect_parallel
elapsed=$(( $(date +%s) - started ))

case "$STEP_NAMES" in
  *alpha*beta*) ;;
  *) echo "FAIL: both parallel steps must be recorded: $STEP_NAMES" >&2; exit 1 ;;
esac
[ "$elapsed" -lt 4 ] || {
  echo "FAIL: the two 2s suites took ${elapsed}s — they ran serially" >&2
  exit 1
}
echo "PASS: independent suites overlap (${elapsed}s for two 2s steps)"

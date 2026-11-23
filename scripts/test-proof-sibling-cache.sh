#!/usr/bin/env bash
# bd-p7oim: a green sibling suite at rev X is a cache key. A later proof that
# pins X must skip the (multi-minute) suite but still record the step.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/proof-sibling-cache.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

python3 - "$ROOT/scripts/clean-proof.sh" "$TMP/fn.sh" <<'PY'
import sys
src = open(sys.argv[1], encoding="utf-8").read()
start = src.index("\nsibling_tests() {") + 1
end = src.index("\n}\n", start) + 3
open(sys.argv[2], "w", encoding="utf-8").write(src[start:end])
PY

mkdir -p "$TMP/toron" "$TMP/cache"
printf '[package]\nname = "toron"\n' >"$TMP/toron/Cargo.toml"
: >"$TMP/cache/toron-abc123.green"

MBX=(echo)
STEP_NAMES=""
STEP_STATUSES=""
STEP_SECS=""
FAILED_STEP=""
SIBLING_TEST_KEYS=$'toron\tabc123\n'
SIBLING_TEST_RAN=""
SIBLING_TEST_CACHE="$TMP/cache"
RAN="$TMP/ran.log"
: >"$RAN"
run_step() { printf '%s\n' "$1" >>"$RAN"; }
# shellcheck source=/dev/null
. "$TMP/fn.sh"

sibling_tests <<<'toron'
if [ -s "$RAN" ]; then
  echo "FAIL: a cached sibling suite was run again" >&2
  exit 1
fi
case "$STEP_NAMES" in
  *sibling-toron-tests*) ;;
  *) echo "FAIL: the cached step was not recorded: $STEP_NAMES" >&2; exit 1 ;;
esac
# A different rev must NOT hit the cache.
SIBLING_TEST_KEYS=$'toron\tdef456\n'
sibling_tests <<<'toron'
if ! grep -q 'sibling-toron-tests' "$RAN"; then
  echo "FAIL: a new sibling rev reused the old cache" >&2
  exit 1
fi
echo "PASS: sibling suite cached by rev"

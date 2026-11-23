#!/usr/bin/env bash
# bd-p7oim: tests in changed files must run before the long suite, and the
# first red one must stop the proof. No cargo, no daemon.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/proof-changed-tests.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

python3 - "$ROOT/scripts/clean-proof.sh" "$TMP/fn.sh" <<'PY'
import sys
src = open(sys.argv[1], encoding="utf-8").read()
start = src.index("\nrun_changed_file_tests() {") + 1
end = src.index("\n}\n", start) + 3
open(sys.argv[2], "w", encoding="utf-8").write(src[start:end])
PY

mkdir -p "$TMP/flywheel/src"
cat >"$TMP/flywheel/src/x.rs" <<'RS'
#[test]
fn alpha_test() {}
#[tokio::test]
async fn beta_test() {}
RS
CHANGED_RS=$'src/x.rs\n'
MBX=(echo)
RAN="$TMP/ran.log"
: >"$RAN"
run_step() { printf '%s\n' "$1" >>"$RAN"; }
# shellcheck source=/dev/null
. "$TMP/fn.sh"

run_changed_file_tests
if ! grep -q 'changed-alpha_test' "$RAN" || ! grep -q 'changed-beta_test' "$RAN"; then
  echo "FAIL: changed-file tests were not run: $(cat "$RAN")" >&2
  exit 1
fi
echo "PASS: changed-file tests run before the suite"

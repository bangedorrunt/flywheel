#!/usr/bin/env bash
# bd-jp9z1.5: scripts/verify-mutations.sh must never edit the tree it was
# started from. A concurrent build in another lane compiled a mutant that ran
# the real launchctl, so the harness now runs in its own scratch export.
#
# Hermetic: a throwaway git repo, a fake cargo that reports RED only when the
# mutation is visible in its own tree, and no real build.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/verify-mutations-isolation.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

mkdir -p "$TMP/repo/src" "$TMP/bin"
cd "$TMP/repo"
git init -q .
git config user.email isolation@localhost
git config user.name isolation
printf 'original\n' >src/a.rs
cat >spec.sh <<'SPEC'
CARGO_ARGS=(--lib)
TEST_FILTER=(some_test)
mutation "M1  the guard is gone" \
    file=src/a.rs \
    expect=some_test \
    find='original' \
    replace='MUTATED'
SPEC
git add src/a.rs spec.sh
git commit -qm init

cat >"$TMP/bin/fakecargo" <<'STUB'
#!/usr/bin/env bash
# bd-proof-toron-bin: record what the isolated child saw, so the caller can
# prove the built sibling CLI survived the re-exec as an absolute path.
[ -n "${SEE_TORON_FILE:-}" ] && printf '%s\n' "${TORON_BIN:-}" >"$SEE_TORON_FILE"
if grep -q MUTATED src/a.rs 2>/dev/null; then
  printf 'running 1 test\ntest result: FAILED. 0 passed; 1 failed\nfailures:\n    some_test\n'
  exit 101
fi
printf 'running 1 test\ntest result: ok. 1 passed; 0 failed\n'
STUB
chmod +x "$TMP/bin/fakecargo"

sum_of() { shasum -a 1 src/a.rs | awk '{print $1}'; }
mtime_of() { stat -f %m src/a.rs 2>/dev/null || stat -c %Y src/a.rs; }

sum_before="$(sum_of)"
mtime_before="$(mtime_of)"
sleep 1

mkdir -p "$TMP/fake-toron"
printf '#!/usr/bin/env bash\nexit 0\n' >"$TMP/fake-toron/toron"
chmod +x "$TMP/fake-toron/toron"
out="$(TORON_BIN="$TMP/fake-toron/toron" SEE_TORON_FILE="$TMP/seen-toron" \
  bash "$ROOT/scripts/verify-mutations.sh" spec.sh --cargo "$TMP/bin/fakecargo" 2>&1)" \
  || { printf '%s\n' "$out" | tail -20 >&2; fail "the harness exited non-zero"; }

[ "$(sum_of)" = "$sum_before" ] || fail "the harness changed src/a.rs in the caller's tree"
[ "$(mtime_of)" = "$mtime_before" ] || fail "the harness touched src/a.rs in the caller's tree (mtime moved)"
pass "the caller's tracked file is byte- and mtime-identical after the run"

printf '%s\n' "$out" | grep -q "scratch export" \
  || fail "the harness did not report a scratch export: $(printf '%s\n' "$out" | head -5)"
pass "the run happened in its own scratch export"

printf '%s\n' "$out" | grep -q "RED" \
  || fail "the mutation did not prove RED in the export: $(printf '%s\n' "$out" | tail -8)"
pass "the mutation still proves RED inside the export"

seen="$(cat "$TMP/seen-toron" 2>/dev/null || true)"
[ "$seen" = "$TMP/fake-toron/toron" ] \
  || fail "the isolated child saw TORON_BIN='${seen}', not the caller's absolute build"
pass "the scratch export kept the caller's absolute TORON_BIN"

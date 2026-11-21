#!/usr/bin/env bash
# bd-r7852: a mutation run over five mutants uses ONE scratch tree, leaves no
# per-mutant export behind, and reports the peak build disk. The cargo command
# is a fake so the test is fast and needs no toolchain.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/mutation-build-test.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT
# bd-08t3o: a proof's shared scratch tree must not leak into a witness that
# verifies the harness owns its OWN export (otherwise the witness is a no-op).
unset VERIFY_MUTATIONS_BUILD_DIR VERIFY_MUTATIONS_EXPORT_ROOT VERIFY_MUTATIONS_IN_EXPORT VERIFY_MUTATIONS_TARGET_DIR

REPO="$TMP/repo"
mkdir -p "$REPO/scripts/mutations" "$REPO/src" "$TMP/tmp"
for script in verify-mutations.sh proof-lib.sh; do
    cp "$ROOT/scripts/$script" "$REPO/scripts/$script"
done
printf 'fn demo() { let guard = 1; let _ = guard; }\n' >"$REPO/src/lib.rs"
{
    printf '# shellcheck shell=bash\nCARGO_ARGS=(--bin demo)\nTEST_FILTER=(demo_test)\n\n'
    for i in 1 2 3 4 5; do
        printf 'mutation "M%s  guard %s" \\\n    file=src/lib.rs \\\n    expect=demo_test \\\n    find="let guard = 1;" \\\n    replace="let guard = 0;"\n\n' "$i" "$i"
    done
} >"$REPO/scripts/mutations/five.spec.sh"

FAKE="$TMP/fake-cargo"
cat >"$FAKE" <<'EOF'
#!/usr/bin/env bash
if grep -q 'let guard = 1;' src/lib.rs; then
    printf 'test result: ok. 1 passed; 0 failed\n'
else
    printf 'test result: FAILED. 0 passed; 1 failed\n\nfailures:\n    demo_test\n'
fi
EOF
chmod +x "$FAKE"

git -C "$REPO" init -q
git -C "$REPO" config user.email witness@example.com
git -C "$REPO" config user.name Witness
git -C "$REPO" add -A
git -C "$REPO" commit -qm base

BUILD="$TMP/build"
(
    cd "$REPO"
    TMPDIR="$TMP/tmp" VERIFY_MUTATIONS_BUILD_DIR="$BUILD" \
        bash scripts/verify-mutations.sh scripts/mutations/five.spec.sh \
        --cargo "$FAKE" --witness-out "$TMP/witness.json" >"$TMP/run.log" 2>&1
) || {
    echo "test-mutation-witnesses: run failed" >&2
    cat "$TMP/run.log" >&2
    exit 1
}

grep -q '5/5 mutations accounted' "$TMP/run.log" \
    || { echo "test-mutation-witnesses: not all five mutants proved" >&2; tail -20 "$TMP/run.log" >&2; exit 1; }
grep -q 'peak disk' "$TMP/run.log" \
    || { echo "test-mutation-witnesses: peak disk not reported" >&2; exit 1; }
grep -q '"source": "verify-mutations.sh"' "$TMP/witness.json" \
    || { echo "test-mutation-witnesses: harness witness file missing" >&2; cat "$TMP/witness.json" >&2; exit 1; }
grep -q '"verdict": "RED"' "$TMP/witness.json" \
    || { echo "test-mutation-witnesses: harness witness not RED" >&2; exit 1; }
[ -f "$BUILD/repo/scripts/verify-mutations.sh" ] \
    || {
        echo "test-mutation-witnesses: no scratch tree" >&2
        echo "run log:" >&2
        cat "$TMP/run.log" >&2 || true
        echo "build dir: $BUILD" >&2
        find "$BUILD" -maxdepth 3 -type f 2>/dev/null | head -10 || true
        echo "tmp:" >&2
        find "$TMP/tmp" -maxdepth 2 2>/dev/null | head -10 >&2
        exit 1
    }
leftovers="$(find "$TMP/tmp" -maxdepth 1 -name 'verify-mutations-export.*' | wc -l | tr -d ' ')"
[ "$leftovers" = 0 ] \
    || { echo "test-mutation-witnesses: $leftovers per-run exports left behind" >&2; exit 1; }
echo "test-mutation-witnesses: five mutants, one scratch tree, none left behind, peak disk recorded"

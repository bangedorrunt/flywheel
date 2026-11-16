#!/usr/bin/env bash
# bd-jp9z1.4: a test that spawns the flywheel/toron/doorbell binary must build
# that Command through src/test_sandbox.rs. Cheap signal: the file names the
# built binary and does not mention the helper.
#
# Usage: scripts/check-test-sandbox.sh [--self-test]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Files that predate the helper: debt, not permission. A new test cannot be
# added here to get green — remove a line when its file migrates.
BASELINE="$ROOT/scripts/test-sandbox-baseline.txt"

offends() {
    local file="$1"
    grep -q 'CARGO_BIN_EXE_\(flywheel\|toron\|doorbell\)' "$file" || return 1
    grep -q 'test_sandbox' "$file" && return 1
    echo "test-sandbox: $file spawns a built binary without src/test_sandbox.rs" >&2
    return 0
}

scan() {
    local bad=0 file
    while IFS= read -r file; do
        rel="${file#"$ROOT"/}"
        [ "$rel" = "src/test_sandbox.rs" ] && continue
        if [ -f "$BASELINE" ] && grep -qxF "$rel" "$BASELINE"; then continue; fi
        if offends "$file"; then bad=1; fi
    done < <(find "$ROOT/tests" "$ROOT/src" -name '*.rs' -type f)
    return "$bad"
}

self_test() {
    local tmp ok bad
    tmp="$(mktemp -d "${TMPDIR:-/tmp}/test-sandbox-lint.XXXXXX")"
    trap 'rm -rf "$tmp"' RETURN
    printf 'fn f(){ let _ = env!("CARGO_BIN_EXE_flywheel"); }\n' >"$tmp/bad.rs"
    printf 'mod test_sandbox;\nfn f(){ let _ = env!("CARGO_BIN_EXE_flywheel"); }\n' >"$tmp/ok.rs"
    if offends "$tmp/bad.rs" >/dev/null 2>&1; then bad=yes; else bad=no; fi
    if offends "$tmp/ok.rs" >/dev/null 2>&1; then ok=no; else ok=yes; fi
    [ "$bad" = yes ] || { echo "self-test: an offender was not caught" >&2; exit 1; }
    [ "$ok" = yes ] || { echo "self-test: a compliant file was refused" >&2; exit 1; }
    echo "self-test ok"
}

case "${1:-}" in
    --self-test) self_test; exit 0 ;;
    "") scan ;;
    *) echo "usage: $0 [--self-test]" >&2; exit 2 ;;
esac

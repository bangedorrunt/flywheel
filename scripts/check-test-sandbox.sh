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
    echo "test-sandbox: $file spawns a built binary without the test sandbox" >&2
    return 0
}

# The ONE carve-out (bd-jp9z1.6): `live_*.rs`, and only when nothing in it can
# run by default - every #[test] is #[ignore]d and the file is gated on
# FLYWHEEL_LIVE_E2E. A live test measures the real plane, which a sandboxed
# child cannot reach; anything else that spawns must be sandboxed.
live_carve_out() {
    local file="$1"
    case "$(basename "$file")" in
        live_*.rs) ;;
        *) return 1 ;;
    esac
    grep -q 'FLYWHEEL_LIVE_E2E' "$file" || return 1
    local tests ignores
    tests="$(grep -c '^#\[test\]' "$file" || true)"
    ignores="$(grep -c '^#\[ignore' "$file" || true)"
    [ "${tests:-0}" -gt 0 ] || return 1
    [ "${ignores:-0}" -ge "${tests:-0}" ] || return 1
    return 0
}

scan() {
    local bad=0 file
    while IFS= read -r file; do
        rel="${file#"$ROOT"/}"
        [ "$rel" = "src/test_sandbox.rs" ] && continue
        if [ -f "$BASELINE" ] && grep -qxF "$rel" "$BASELINE"; then continue; fi
        live_carve_out "$file" && continue
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

    # The live_*.rs carve-out, in both directions.
    printf '#[ignore]\n#[test]\nfn f(){ let _ = env!("CARGO_BIN_EXE_flywheel"); }\n// FLYWHEEL_LIVE_E2E\n' >"$tmp/live_ok.rs"
    printf '#[test]\nfn f(){ let _ = env!("CARGO_BIN_EXE_flywheel"); }\n// FLYWHEEL_LIVE_E2E\n' >"$tmp/live_unignored.rs"
    printf '#[ignore]\n#[test]\nfn f(){ let _ = env!("CARGO_BIN_EXE_flywheel"); }\n// FLYWHEEL_LIVE_E2E\n' >"$tmp/not_live_name.rs"
    if live_carve_out "$tmp/live_ok.rs"; then ok=yes; else ok=no; fi
    if live_carve_out "$tmp/live_unignored.rs"; then unignored=no; else unignored=yes; fi
    if live_carve_out "$tmp/not_live_name.rs"; then misfiled=no; else misfiled=yes; fi
    [ "$ok" = yes ] || { echo "self-test: an ignored, gated live file was refused" >&2; exit 1; }
    [ "$unignored" = yes ] || { echo "self-test: a live file with a non-ignored test was allowed" >&2; exit 1; }
    [ "$misfiled" = yes ] || { echo "self-test: the carve-out ignored the live_*.rs name rule" >&2; exit 1; }
    echo "self-test ok"
}

case "${1:-}" in
    --self-test) self_test; exit 0 ;;
    "") scan ;;
    *) echo "usage: $0 [--self-test]" >&2; exit 2 ;;
esac

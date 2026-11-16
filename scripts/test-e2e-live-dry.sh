#!/usr/bin/env bash
# bd-4z7mb follow-up: `e2e-live.sh --five --dry-run` must PRINT the plan and
# touch nothing. Fixtures only: a fake rex and cargo on PATH, a temp HOME/XDG,
# deny-shims so nothing can reach the operator's launchd/systemd even by mistake.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/e2e-live-dry.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

mkdir -p "$TMP/bin" "$TMP/home" "$TMP/xdg"
# Deny-shims first, so a dry run that tried to boot anything is caught loudly.
for shim in launchctl systemctl; do
    printf '#!/usr/bin/env bash\necho "SHIM %s $*" >>"$E2E_DRY_SHIM_LOG"\nexit 1\n' "$shim" >"$TMP/bin/$shim"
    chmod +x "$TMP/bin/$shim"
done
# Fake rex: records every call. A dry run must produce an EMPTY record.
printf '#!/usr/bin/env bash\necho "rex $*" >>"%s"\nexit 0\n' "$TMP/rex.log" >"$TMP/bin/rex"
chmod +x "$TMP/bin/rex"
printf '#!/usr/bin/env bash\necho "cargo $*" >>"%s"\nexit 0\n' "$TMP/cargo.log" >"$TMP/bin/cargo"
chmod +x "$TMP/bin/cargo"

out="$(HOME="$TMP/home" XDG_CONFIG_HOME="$TMP/xdg/config" XDG_DATA_HOME="$TMP/xdg/data" \
    XDG_STATE_HOME="$TMP/xdg/state" XDG_RUNTIME_DIR="$TMP/xdg/run" \
    E2E_DRY_SHIM_LOG="$TMP/shims.log" \
    PATH="$TMP/bin:$PATH" \
    FLYWHEEL_LIVE_SESSION="dry-session" FLYWHEEL_LIVE_PIN="DryPin" \
    bash "$ROOT/scripts/e2e-live.sh" --five --dry-run 2>&1)" || fail "dry run must exit 0: $out"

for needle in "dry-session" "tests/live_adr0027_toron_flags.rs" "tests/live_collab_patterns.rs" \
    "tests/live_doorbell_operator_guard.rs" "tests/live_event_poll_parity.rs" \
    "tests/live_serialize_worktree.rs" "FLYWHEEL_LIVE_E2E=1" "receipt:"; do
    case "$out" in
        *"$needle"*) ;;
        *) fail "the dry run must print $needle: $out" ;;
    esac
done
pass "the dry run prints the session, the five suites, the env and the receipt"

[ ! -s "$TMP/rex.log" ] || fail "a dry run must never call rex: $(cat "$TMP/rex.log")"
pass "a dry run creates no session (rex was never called)"

[ ! -s "$TMP/shims.log" ] || fail "a dry run reached launchctl/systemctl: $(cat "$TMP/shims.log")"
pass "a dry run never reaches the supervisor tools"

echo "ok"

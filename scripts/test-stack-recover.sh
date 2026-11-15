#!/usr/bin/env bash
# Sandbox test for scripts/stack.sh recover_stale_hosts (bd-0uzrq.9).
# Never touches live launchd, live /health, or operator binaries.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/stack-recover-test.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT

mkdir -p "$TMP/bin" "$TMP/cargo/bin" "$TMP/state" "$TMP/home"
export PATH="$TMP/bin:$PATH"
export HOME="$TMP/home"
export CARGO_INSTALL_ROOT="$TMP/cargo"
export DOORBELL_URL="http://127.0.0.1:18980"
export RECOVER_STALE_TIMEOUT=4

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

# --- Case 1: legacy daemon absent (curl fails) => exit 0 with message ---
cat >"$TMP/bin/curl" <<'SH'
#!/usr/bin/env bash
exit 22
SH
chmod +x "$TMP/bin/curl"

set +e
out="$(
  # shellcheck disable=SC1091
  . "$ROOT/scripts/stack.sh"
  recover_stale_hosts 2>&1
)"
rc=$?
set -e
[ "$rc" -eq 0 ] || fail "case1: expected rc 0, got $rc: $out"
printf '%s\n' "$out" | grep -Fq "no legacy doorbell daemon; nothing to replace" \
  || fail "case1: missing expected message: $out"
pass "absent legacy daemon returns 0 with message"

# --- Case 2: reachable daemon reports stale host never replaced => exit 1 ---
# Real-looking flywheel binary whose first 8KB hash is known.
python3 - "$TMP/cargo/bin/flywheel" <<'PY'
import sys
path = sys.argv[1]
with open(path, 'wb') as f:
    f.write(b'flywheel-new-binary\n' + b'\0' * 9000)
PY
chmod +x "$TMP/cargo/bin/flywheel"
hash="$(dd if="$TMP/cargo/bin/flywheel" bs=8192 count=1 2>/dev/null | shasum -a 1 | awk '{print $1}')"

cat >"$TMP/bin/curl" <<SH
#!/usr/bin/env bash
# Always report the same stale live host on old generation; never replaced.
cat <<'JSON'
{"hosts":[{"pid":4242,"project":"p","workspace":"/ws","state":"live","generation":{"binary_hash":"oldhash"},"last_seen_age_s":1}]}
JSON
SH
chmod +x "$TMP/bin/curl"

# recover_stale_hosts requires jq and uses it to parse health.
if ! command -v jq >/dev/null 2>&1; then
  fail "jq required for case2"
fi

cat >"$TMP/bin/ps" <<'SH'
#!/usr/bin/env bash
# ps -p <pid> -o command=
if [ "${1:-}" = "-p" ]; then
  echo "flywheel __doorbell --project p"
  exit 0
fi
exec /bin/ps "$@"
SH
chmod +x "$TMP/bin/ps"

cat >"$TMP/bin/launchctl" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "$TMP/bin/launchctl"

cat >"$TMP/bin/sleep" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "$TMP/bin/sleep"

set +e
out="$(
  # kill is a builtin; override so stale_host_rows treats the stub pid as alive.
  kill() {
    if [ "${1:-}" = "-0" ]; then return 0; fi
    return 0
  }
  . "$ROOT/scripts/stack.sh"
  recover_stale_hosts 2>&1
)"
rc=$?
set -e
[ "$rc" -ne 0 ] || fail "case2: expected nonzero rc, got 0: $out"
printf '%s\n' "$out" | grep -Fq "stale hosts not replaced after" \
  || fail "case2: missing failure message: $out"
printf '%s\n' "$out" | grep -Fq "stopping for replacement" \
  || fail "case2: missing stop message: $out"
pass "reachable daemon with unreplaced stale host returns failure"

echo "ok"

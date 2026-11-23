#!/usr/bin/env bash
# bd-2gxup: a stale committed Cargo.lock must fail clean-proof at the BUILD
# step, naming Cargo.lock. Fixtures only: a tiny git repo, a fake MBX, temp XDG.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/proof-lock-stale.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

REPO="$TMP/repo"
mkdir -p "$REPO/scripts" "$TMP/bin"
cd "$REPO"
git init -q .
git config user.email proof@test
git config user.name proof
cp "$ROOT/scripts/clean-proof.sh" "$ROOT/scripts/proof-lib.sh" scripts/
printf '#!/usr/bin/env bash\nexit 0\n' >scripts/verify-needles.sh
chmod +x scripts/verify-needles.sh
printf 'STALE LOCK MARKER\n' >Cargo.lock
git add .
git commit -qm stale-lock
SHA="$(git rev-parse HEAD)"

cat >"$TMP/bin/fakecargo" <<'STUB'
#!/usr/bin/env bash
args="$*"
printf '%s\n' "$args" >>"${FAKE_CARGO_LOG:?}"
if [[ "$args" == *build* && "$args" == *--locked* ]] && grep -q STALE Cargo.lock 2>/dev/null; then
  echo "error: the lock file Cargo.lock needs to be updated but --locked was passed" >&2
  exit 1
fi
exit 0
STUB
chmod +x "$TMP/bin/fakecargo"

# bd-b3j7x: never inherit the operator's identity or daemon route. The Rust
# witness also hands us a fake `toron` first on PATH via TestSandbox; assert
# the red mail went there, never to the live daemon.
: "${TORON_FAKE_LOG:?bd-b3j7x: run this witness under TestSandbox with a fake toron}"
out="$(env -u AGENT_NAME -u TORON_MCP_URL \
  HOME="$TMP/home" XDG_CONFIG_HOME="$TMP/xdg/config" XDG_STATE_HOME="$TMP/xdg/state" \
  XDG_CACHE_HOME="$TMP/xdg/cache" XDG_RUNTIME_DIR="$TMP/xdg/run" \
  CLEAN_PROOF_REPO_ROOT="$REPO" CLEAN_PROOF_LOCK_FILE="$TMP/proof.lock" \
  FAKE_CARGO_LOG="$TMP/cargo.log" MBX_CMD="$TMP/bin/fakecargo" \
  bash "$ROOT/scripts/clean-proof.sh" --sha "$SHA" --receipt "$TMP/receipt.json" 2>&1)" || true

case "$out" in
  *Cargo.lock*) ;;
  *) fail "the build failure must name Cargo.lock: $out" ;;
esac
grep -qF 'build --workspace --all-targets --locked' "$TMP/cargo.log" \
  || fail "the build must pass --locked: $(cat "$TMP/cargo.log")"
python3 - "$TMP/receipt.json" <<'PY' || fail "the receipt must name build as the failed step"
import json, sys
r = json.load(open(sys.argv[1]))
assert r["ok"] is False, r
assert r["failed_step"] == "build", r
PY
grep -qF 'mail send' "$TORON_FAKE_LOG" \
  || fail "the red mail never reached the fake toron: $(cat "$TORON_FAKE_LOG")"
grep -qF '[clean-proof]' "$TORON_FAKE_LOG" \
  || fail "the fake toron did not see the clean-proof red-row subject: $(cat "$TORON_FAKE_LOG")"
pass "a stale Cargo.lock fails the build step; red mail went to the fake toron"
echo "ok"

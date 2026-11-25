#!/usr/bin/env bash
# clean-proof mails red rows only as a real pin; with none it says so and sends nothing.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/clean-proof-mail-test.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

mkdir -p "$TMP/bin" "$TMP/repo"
cat >"$TMP/bin/toron" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$TORON_CALLS"
EOF
cat >"$TMP/bin/br" <<'EOF'
#!/usr/bin/env bash
echo '{}'
EOF
chmod +x "$TMP/bin/toron" "$TMP/bin/br"
git -C "$TMP/repo" init -q
export PATH="$TMP/bin:$PATH" TORON_CALLS="$TMP/calls.txt"
unset AGENT_NAME FLYWHEEL_MAIL_AS
printf '%s' '{"ok": false, "failed_step": "build", "logs": "/logs", "mutations": []}' >"$TMP/receipt.json"

eval "$(awk '/^mail_red_owners\(\) \{/{f=1} f{print} f&&/^\}$/{exit}' "$ROOT/scripts/clean-proof.sh")"
declare -F mail_red_owners >/dev/null || fail "mail_red_owners not found in clean-proof.sh"

: >"$TORON_CALLS"
note="$(mail_red_owners "$TMP/receipt.json" "$TMP/repo" flywheel "" 2>&1 >/dev/null)"
[ ! -s "$TORON_CALLS" ] || fail "sent mail with no pin: $(cat "$TORON_CALLS")"
case "$note" in *"not mailed"*) ;; *) fail "no note when skipping: $note" ;; esac
pass "no pin: nothing sent, note printed"

: >"$TORON_CALLS"
mail_red_owners "$TMP/receipt.json" "$TMP/repo" flywheel CalmRiver >/dev/null 2>&1
grep -q -- "--as CalmRiver" "$TORON_CALLS" || fail "pin not used as sender: $(cat "$TORON_CALLS")"
pass "pin: mail sent as that pin"

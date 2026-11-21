#!/usr/bin/env bash
# bd-9os20: prove the shipped plugin loads from its installed path under a
# throwaway HOME, and that stack.sh copies it to the path spawn will pass via
# --plugin-dir. No live seat, no operator config, no token.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/claude-plugin-load.XXXXXX")"
cleanup() { rm -rf "$TMP"; }
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

PLUGIN="$ROOT/plugins/claude-flywheel"
[ -f "$PLUGIN/hooks/hooks.json" ] || fail "no hooks.json in the shipped plugin"
[ -f "$PLUGIN/hooks/register.ts" ] || fail "no register.ts in the shipped plugin"
python3 - "$PLUGIN/hooks/hooks.json" <<'PY' || fail "hooks.json does not declare the module"
import json, sys
assert "./register.ts" in json.load(open(sys.argv[1]))["modules"]
PY
pass "plugin manifest declares the register module"

export HOME="$TMP/home"
export XDG_CONFIG_HOME="$TMP/home/.config"
export XDG_DATA_HOME="$TMP/data"
export CLAUDE_CONFIG_DIR="$TMP/home/.claude"
mkdir -p "$HOME" "$XDG_DATA_HOME"

if command -v claude >/dev/null 2>&1; then
  out="$(HOME="$HOME" CLAUDE_CONFIG_DIR="$CLAUDE_CONFIG_DIR" \
    claude --plugin-dir "$PLUGIN" plugin details flywheel 2>&1)" \
    || fail "claude plugin details failed: $out"
  printf '%s\n' "$out" | grep -q 'flywheel 0.1.0' || fail "plugin did not load: $out"
  printf '%s\n' "$out" | grep -q 'Source: flywheel@inline' || fail "unexpected source: $out"
  pass "claude --plugin-dir loads the shipped plugin under a temp HOME"
else
  pass "claude not installed here; install-time copy is still checked below"
fi

out="$(bash "$ROOT/scripts/stack.sh" install --dry-run --operator --skip-toron \
  --no-codesign --no-restart 2>&1)" || fail "stack.sh dry-run failed: $out"
want="$XDG_DATA_HOME/flywheel/plugins/claude-flywheel"
printf '%s\n' "$out" | grep -qF "installing Claude wake plugin into $want" \
  || fail "dry-run did not name the installed plugin path: $out"
printf '%s\n' "$out" | grep -qF "dry-run: cp -R $PLUGIN $want" \
  || fail "dry-run did not copy the plugin from the source tree: $out"
[ ! -e "$want" ] || fail "dry-run actually created the plugin dir"
pass "stack.sh installs the plugin to the --plugin-dir path, dry-run is inert"

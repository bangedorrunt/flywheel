#!/usr/bin/env bash
# Capture the per-harness pane-state corpus for bd-0uzrq.3.
#
# Every capture is real: a throwaway rex session, one block per harness, driven
# only through that block. The live crew/captain panes are never touched, and
# the session is killed on every exit path.
#
# Per harness it captures, in this order:
#   idle-fresh        the prompt before anything is typed
#   partial-input     a typed line with NO Enter (the operator mid-typing)
#   idle-cleared      the same pane after the line is cleared (ctrl+u)
#   working           one real turn in flight
#   idle-after-turn   the same pane when the turn is done
#   modal             a dialog/palette (harness-dependent)
#
# Usage: scripts/capture-pane-corpus.sh [out-dir] [harness...]
#   default harnesses: opencode claude gemini pi grok
set -uo pipefail

OUT="${1:-tests/fixtures/pane-screens}"
shift || true
HARNESSES=("$@")
[ "${#HARNESSES[@]}" -gt 0 ] || HARNESSES=(opencode claude gemini pi grok)

SESSION="bd0uzrq3-capture-$$"
say() { printf '\n=== %s\n' "$*"; }
cleanup() { rex kill "$SESSION" >/dev/null 2>&1; }
trap cleanup EXIT INT TERM HUP

bin_of() { command -v "$1" 2>/dev/null; }

rex kill "$SESSION" >/dev/null 2>&1
rex new "$SESSION" >/dev/null 2>&1 || { echo "cannot create $SESSION" >&2; exit 2; }

cap() { # harness block label
  local h="$1" b="$2" label="$3"
  mkdir -p "$OUT/$h"
  rex capture -s "$SESSION" --block "$b" >"$OUT/$h/$label.txt" 2>/dev/null || true
  printf '%s\n' "$label" >"$OUT/$h/$label.label"
  printf '  captured %-28s %s bytes\n' "$label" "$(wc -c <"$OUT/$h/$label.txt" | tr -d ' ')"
}

clear_line() { rex send-key -s "$SESSION" --block "$1" ctrl+u >/dev/null 2>&1; }

for h in "${HARNESSES[@]}"; do
  bin="$(bin_of "$h")"
  if [ -z "$bin" ]; then
    say "$h: not installed — no captures (recorded as a gap)"
    continue
  fi
  say "$h ($bin)"
  b="$h"
  rex run -s "$SESSION" --label "$b" --shell /bin/zsh -- "$bin" >/dev/null 2>&1 || {
    echo "  launch failed" >&2
    continue
  }
  sleep 12
  cap "$h" "$b" idle-fresh

  rex send -s "$SESSION" --block "$b" "half typed line" >/dev/null 2>&1
  sleep 2
  cap "$h" "$b" partial-input

  clear_line "$b"
  sleep 1
  cap "$h" "$b" idle-cleared

  rex send -s "$SESSION" --block "$b" "Reply with exactly: OK" >/dev/null 2>&1
  rex send-key -s "$SESSION" --block "$b" enter >/dev/null 2>&1
  sleep 3
  cap "$h" "$b" working
  sleep 25
  clear_line "$b"
  cap "$h" "$b" idle-after-turn

  # A harness-specific dialog. opencode/claude/codex: the command palette;
  # pi/gemini/grok: the same key or a model picker, whichever opens a box.
  rex send-key -s "$SESSION" --block "$b" ctrl+p >/dev/null 2>&1
  sleep 2
  cap "$h" "$b" modal
  rex send-key -s "$SESSION" --block "$b" esc >/dev/null 2>&1
  sleep 1
done

say "captures under $OUT"
find "$OUT" -name '*.txt' | sed "s|$OUT/||" | sort

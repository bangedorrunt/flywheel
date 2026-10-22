#!/bin/bash
# verify-operator-state-clean — bd-3dge: a suite run must not write into the
# operator's state dir.
#
# Why the gate exists: the operator's ~/.local/state/flywheel is not a test
# fixture. A subscription written there is a REAL row — the live doorbell
# hosts it, and every host holds a wake socket on the toron daemon — so a
# test that writes there spends the operator's descriptors on rows nobody
# asked for. Measured: 20 "flywheel-fw-gate-<pid>" rows in the operator's
# store, one set per gate run, and the daemon refusing LIVE subscribers at
# 73 rows.
#
# What it compares, and why only that:
#   - the FILE LIST of the state dir, minus the paths the live daemons own
#     (their owner/wake/journal files, their run rows, their pid and log): a
#     new file is a new artifact;
#   - the BYTES of doorbell/subscriptions.json: the store the daemon hosts
#     from.
# The rest of the dir is written by the LIVE doorbell and flywheel daemon
# (journals, heartbeats, run rows), so its content changes on the wrong day —
# comparing it would make this gate fail for the wrong reason.
#
# Usage: scripts/verify-operator-state-clean.sh [test command...]
#   default: mbx +nightly-aarch64-apple-darwin test -p flywheel --bin flywheel
set -u

# The live daemons write into this dir while the suite runs, so their files
# are excluded from the file-list comparison: a daemon-owned path that moves
# is the daemon's, not the suite's. Everything else counts.
daemon_owned() {
  case "$1" in
    ./doorbell/owner-*|./doorbell-wake-*|./doorbell-journal-*) return 0 ;;
    ./journals/doorbell-journal-*) return 0 ;;
    ./runs/*|./last_sent/*) return 0 ;;
    ./doorbell.pid|./doorbell.log|./loop-pids.json) return 0 ;;
  esac
  return 1
}

filter_list() {
  while IFS= read -r f; do
    if ! daemon_owned "$f"; then echo "$f"; fi
  done
}

STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/flywheel"
if [ ! -d "$STATE_DIR" ]; then
  echo "no state dir at $STATE_DIR — nothing to compare"
  exit 0
fi

BEFORE="$(mktemp -d)"
AFTER="$(mktemp -d)"
( cd "$STATE_DIR" && find . -type f | sort ) | filter_list > "$BEFORE/files.txt"
if [ -f "$STATE_DIR/doorbell/subscriptions.json" ]; then
  cp "$STATE_DIR/doorbell/subscriptions.json" "$BEFORE/subscriptions.json"
fi

if [ "$#" -gt 0 ]; then
  "$@"
else
  mbx +nightly-aarch64-apple-darwin test -p flywheel --bin flywheel
fi
suite_rc=$?

( cd "$STATE_DIR" && find . -type f | sort ) | filter_list > "$AFTER/files.txt"
if [ -f "$STATE_DIR/doorbell/subscriptions.json" ]; then
  cp "$STATE_DIR/doorbell/subscriptions.json" "$AFTER/subscriptions.json"
fi

rc=0
new="$(comm -13 "$BEFORE/files.txt" "$AFTER/files.txt")"
gone="$(comm -23 "$BEFORE/files.txt" "$AFTER/files.txt")"
if [ -n "$new" ] || [ -n "$gone" ]; then
  echo "FAIL: the suite changed the operator's state dir file list" >&2
  if [ -n "$new" ]; then echo "  new: $new" >&2; fi
  if [ -n "$gone" ]; then echo "  gone: $gone" >&2; fi
  rc=1
fi
if [ -f "$BEFORE/subscriptions.json" ] && [ -f "$AFTER/subscriptions.json" ]; then
  if ! diff -q "$BEFORE/subscriptions.json" "$AFTER/subscriptions.json" >/dev/null; then
    echo "FAIL: the suite changed the operator's subscription store" >&2
    diff "$BEFORE/subscriptions.json" "$AFTER/subscriptions.json" >&2
    rc=1
  fi
fi
# The state dir's verdict prints on its own: a suite that fails for another
# reason must not read as "the state dir moved" (or as "it did not").
if [ "$rc" -eq 0 ]; then
  echo "PASS: the operator's state dir is untouched"
fi
if [ "$suite_rc" -ne 0 ]; then
  echo "FAIL: the suite itself failed (exit $suite_rc)" >&2
  rc=1
fi
exit "$rc"

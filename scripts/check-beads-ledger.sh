#!/usr/bin/env bash
# bd-0vzl: fail closed on a beads-ledger row-set regression.
#
# Why this exists: `.beads/issues.jsonl` is a FULL-SNAPSHOT export. Any
# worktree that writes `.beads/` from its own (older) sqlite rewrites the whole
# file and silently drops the rows other agents closed in parallel. bd-bpj1
# came back as `in_progress` exactly that way (`ac2d521` closes it, an ancestor
# of HEAD, yet the ledger at HEAD read `in_progress`), which is what makes
# "stale in_progress" indistinguishable from real work.
#
# Two passes over the SAME comparison (`<base>:<ledger>` -> `<ledger>`):
#   1. every row that exists at base still exists, and
#   2. no row that was `closed` at base reads anything else now.
# Pass 3 (`--history`, opt-in) additionally requires that every bead a commit
# CLAIMS to close still reads `closed`. A claim is a structured trailer -
# `Closes: bd-x` / `Closed: bd-x` in the message body - never a bead id that
# merely appears in prose: "close the duplicates against bd-3vkpe" names a
# survivor and a bead under work, it does not claim either is closed. Reading
# prose made the gate cry wolf, and a guard everyone force-passes is not a
# guard (bd-i4fqc follow-up).
#
# Passes 1-2 in CI (`--base origin/<base_ref>`) are what catches a branch that
# publishes a snapshot from a stale sqlite: locally the worktree equals HEAD, so
# a HEAD-vs-worktree run is trivially green.
#
# Usage:
#   scripts/check-beads-ledger.sh [--base <rev> | --base-ledger <path>]
#                                 [--ledger <path>] [--history] [--force <reason>]
#
# Exit: 0 clean, 1 regression, 2 usage / environment error.
#
# `--force <reason>` publishes a regression on purpose (a deliberate `br
# reopen`, or a bead genuinely deleted). It is never the default: the whole
# point is that the loss is loud.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BASE="HEAD"
BASE_LEDGER=""
LEDGER=".beads/issues.jsonl"
HISTORY=0
FORCE_REASON=""

# The ids a commit CLAIMS to close: `Closes:` or `Closed:` trailers in the
# message body, comma/space separated. Pure text -> ids, so the self-test can
# pin it without a repo.
history_claims_text() {
  grep -oE '^(Closes|Closed):[[:space:]].*$' \
    | grep -oE 'bd-[A-Za-z0-9._-]+' || true
}

history_claims() {
  git log --format=%B "$1" | history_claims_text | sort -u
}

SELF_TEST=0
while [ $# -gt 0 ]; do
  case "$1" in
    --self-test)   SELF_TEST=1; shift ;;
    --base)        BASE="${2:-}"; shift 2 ;;
    --base-ledger) BASE_LEDGER="${2:-}"; shift 2 ;;
    --ledger)      LEDGER="${2:-}"; shift 2 ;;
    --history)     HISTORY=1; shift ;;
    --force)       FORCE_REASON="${2:-}"; shift 2 ;;
    -h|--help)     sed -n '2,32p' "$0"; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [ "$SELF_TEST" -eq 1 ]; then
  # A prose mention is NOT a claim; a trailer is. Fixture messages, no repo.
  prose="$(printf 'chore(beads): close the duplicates against bd-alpha (bd-beta)\n' | history_claims_text)"
  trailer="$(printf 'fix: the thing\n\nCloses: bd-alpha, bd-beta\n' | history_claims_text)"
  [ -z "$prose" ] || { echo "self-test: prose produced a claim: $prose" >&2; exit 1; }
  [ "$trailer" = "$(printf 'bd-alpha\nbd-beta')" ] || {
    echo "self-test: trailer claims were $trailer" >&2
    exit 1
  }
  echo "self-test ok"
  exit 0
fi

[ -n "$BASE" ] || { echo "--base needs a revision" >&2; exit 2; }
cd "$ROOT"

if [ ! -f "$LEDGER" ]; then
  echo "missing ledger: $LEDGER" >&2
  exit 2
fi

BASE_FILE="$(mktemp)"
trap 'rm -f "$BASE_FILE"' EXIT
if [ -n "$BASE_LEDGER" ]; then
  # Test seam / CI: read the reference straight from a file.
  if ! cp "$BASE_LEDGER" "$BASE_FILE" 2>/dev/null; then
    echo "cannot read --base-ledger $BASE_LEDGER" >&2
    exit 2
  fi
elif ! git show "$BASE:$LEDGER" > "$BASE_FILE" 2>/dev/null; then
  echo "cannot read $LEDGER at $BASE (not a commit, or the file is new)" >&2
  exit 2
fi

HISTORY_IDS=""
if [ "$HISTORY" -eq 1 ]; then
  # Fail loud rather than degrade: a shallow clone sees one commit, so the
  # scan would quietly check nothing and report OK. Silent degradation is the
  # exact failure mode this gate exists to stop.
  if [ "$(git rev-parse --is-shallow-repository 2>/dev/null || echo false)" = "true" ]; then
    echo "--history needs full history, but this is a shallow clone" >&2
    echo "(re-run with \`git fetch --unshallow\`, or drop --history)" >&2
    exit 2
  fi
  # Structured trailers only (see the header): a subject that mentions an id
  # is prose, and prose is not a claim. Only ids that HAVE a row are reported:
  # an id with no row was never in this ledger (a foreign lane's bead), which
  # is absence, not a regression.
  HISTORY_IDS="$(history_claims "$BASE")"
fi

RC=0
BASE_FILE="$BASE_FILE" LEDGER="$LEDGER" HISTORY_IDS="$HISTORY_IDS" \
python3 - <<'PY' || RC=$?
import json, os, sys

def rows(path):
    out = {}
    with open(path) as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            out[rec["id"]] = rec.get("status")
    return out

base = rows(os.environ["BASE_FILE"])
now = rows(os.environ["LEDGER"])

regressions = []
for bead, status in base.items():
    if bead not in now:
        regressions.append((bead, f"row dropped (was {status})"))
    elif status == "closed" and now[bead] != "closed":
        regressions.append((bead, f"closed -> {now[bead]}"))

for bead in os.environ.get("HISTORY_IDS", "").split():
    if bead in now and now[bead] != "closed":
        regressions.append((bead, f"a commit claims to close it, ledger reads {now[bead]!r}"))

for bead, what in regressions:
    print(f"REGRESSION {bead}: {what}")
sys.exit(1 if regressions else 0)
PY
if [ "$RC" -eq 0 ]; then
  echo "OK: beads ledger has no row-set regression vs $BASE"
  exit 0
fi

if [ -n "$FORCE_REASON" ]; then
  echo "FORCED past ${RC} regression(s) vs $BASE — reason: $FORCE_REASON"
  exit 0
fi

REF="$BASE"
[ -n "$BASE_LEDGER" ] && REF="$BASE_LEDGER"
cat >&2 <<EOF
FAIL: the ledger at $LEDGER dropped or reopened rows that $REF has closed.
      (this is the full-snapshot export clobbering a parallel lane's closes)
      Fix: re-apply the closes (br close <id> --commit-sha <sha>) and re-export
      with \`br sync --flush-only\`, then re-run. To publish anyway:
        $0 --base $BASE --force "<why>"
EOF
exit 1

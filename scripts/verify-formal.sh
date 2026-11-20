#!/usr/bin/env bash
# Hand-written TLA+ protocol models checked by everystate (ADR-0021 D5.4,
# ADR-0022 V11). everystate is a native TLA+ model checker with TLC's .cfg
# format and exit codes; there is no Java anywhere in this check.
#
# Positive models must pass; every negative fixture (a config that removes one
# fence) must FAIL with the named invariant. Exit 0 only when both expectations
# hold.
#
#   bash scripts/verify-formal.sh
#
# Install the pinned checker once:
#   cargo install everystate --version 0.4.1 --locked
# Override the binary with FLYWHEEL_EVERYSTATE=/path/to/everystate.
set -euo pipefail
cd "$(dirname "$0")/.."

EVERYSTATE_VERSION="0.4.1"
MODELS=".agents/skills/verify-protocol-tla/models"
SELF="$0"

die() {
  echo "verify-formal: $*" >&2
  exit 1
}

CHECKER="${FLYWHEEL_EVERYSTATE:-$(command -v everystate || true)}"
[ -n "$CHECKER" ] && [ -x "$CHECKER" ] ||
  die "everystate not found. Install it: cargo install everystate --version ${EVERYSTATE_VERSION} --locked"
got_version="$("$CHECKER" -version 2>&1 | awk '{print $2; exit}')"
[ "$got_version" = "$EVERYSTATE_VERSION" ] ||
  die "everystate $got_version at $CHECKER, want the pinned ${EVERYSTATE_VERSION}: cargo install everystate --version ${EVERYSTATE_VERSION} --locked --force"
echo "verify-formal: everystate ${EVERYSTATE_VERSION} ($CHECKER)"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/flywheel-tla.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# Each run checks a fresh copy in the temp dir, so a check never writes into
# the repo (bd-wum95). -deadlock: these models stop legitimately at their bounds.
check() { # model cfg -> prints the JSON verdict line
  local model="$1" cfg="$2" dir="$WORK/$1-$2"
  mkdir -p "$dir"
  cp "$MODELS/$model.tla" "$MODELS/$cfg.cfg" "$dir/"
  ( cd "$dir" && "$CHECKER" -json -deadlock -config "$cfg.cfg" "$model.tla" ) >"$dir/out.log" 2>&1 || true
  tail -1 "$dir/out.log"
}

passed=0
failed=0

expect_pass() { # model cfg
  local model="$1" cfg="$2" verdict status
  verdict="$(check "$model" "$cfg")"
  status="$(printf '%s' "$verdict" | jq -r '.status // "unparsed"' 2>/dev/null || echo unparsed)"
  if [ "$status" = ok ]; then
    echo "  PASS  $model ($cfg)"
    passed=$((passed + 1))
  else
    echo "  FAIL  $model ($cfg) — expected a clean run, got $status" >&2
    tail -20 "$WORK/$model-$cfg/out.log" >&2 || true
    failed=$((failed + 1))
  fi
}

expect_violation() { # model cfg invariant
  local model="$1" cfg="$2" invariant="$3" verdict status name
  verdict="$(check "$model" "$cfg")"
  status="$(printf '%s' "$verdict" | jq -r '.status // "unparsed"' 2>/dev/null || echo unparsed)"
  name="$(printf '%s' "$verdict" | jq -r '.name // ""' 2>/dev/null || true)"
  if [ "$status" = invariant ] && [ "$name" = "$invariant" ]; then
    echo "  PASS  $model ($cfg) — $invariant violated as expected"
    passed=$((passed + 1))
  else
    echo "  FAIL  $model ($cfg) — expected $invariant to be violated, got $status ${name}" >&2
    tail -20 "$WORK/$model-$cfg/out.log" >&2 || true
    failed=$((failed + 1))
  fi
}

echo "verify-formal: positive models"
expect_pass JobLease JobLease
expect_pass BindBeforePrompt BindBeforePrompt
expect_pass PlanApply PlanApply
expect_pass ClaimExclusion ClaimExclusion
expect_pass Respawn Respawn
expect_pass DeferredHandoff DeferredHandoff
expect_pass P2pDelivery P2pDelivery
expect_pass DeferredQueue DeferredQueue
expect_pass DeferredQueueLanes DeferredQueueLanes
expect_pass DoorbellMailPush DoorbellMailPush
expect_pass DispatcherReap DispatcherReap
expect_pass JournalAppend JournalAppend
expect_pass JournalRotate JournalRotate
expect_pass SupervisorRetire SupervisorRetire
expect_pass PeerContention PeerContention
expect_pass ReservationBoard ReservationBoard
expect_pass TrackerCas TrackerCas
expect_pass EventsDedupe EventsDedupe

echo "verify-formal: negative fixtures (must fail)"
expect_violation JobLease JobLease.broken-steal NoSteal
expect_violation JobLease JobLease.broken-stale-settle NoStaleSettle
expect_violation JobLease JobLease.broken-revive-terminal TerminalStable
expect_violation Respawn Respawn.broken-claim-before-free OneLiveHolder
expect_violation Respawn Respawn.broken-drop-reservations ReservationsKept
expect_violation ClaimExclusion ClaimExclusion.broken-allow-author NoSelfReview
expect_violation PlanApply PlanApply.broken-clobber EveryAppendLands
expect_violation BindBeforePrompt BindBeforePrompt.broken-prompt-first PromptedImpliesBound
expect_violation DeferredHandoff DeferredHandoff.broken-memory-only NoLostWake
expect_violation P2pDelivery P2pDelivery.broken-drop-on-busy EveryDirectiveReachesTargetOrIsJournaledPending
expect_violation DeferredQueue DeferredQueue.broken-ring-twice AtMostOnceRung
expect_violation DeferredQueue DeferredQueue.broken-strand EveryWakeAccounted
expect_violation DeferredQueue DeferredQueue.broken-preempt-any OnlyActNowPreempts
expect_violation DeferredQueue DeferredQueue.broken-preempt-unrecorded AtMostOnceRung
expect_violation DeferredQueueLanes DeferredQueueLanes.broken-drain-unreceived EveryWakeAccounted
expect_violation DoorbellMailPush DoorbellMailPush.broken-thread-rung EveryWakeAccounted
expect_violation DispatcherReap DispatcherReap.broken-fallback-reap NeverReapOnFallback
expect_violation DispatcherReap DispatcherReap.broken-reap-live-session NeverReapLiveSession
expect_violation DispatcherReap DispatcherReap.broken-label-dropped LabelOnlyDoorKept
expect_violation JournalAppend JournalAppend.broken-interleave NoTornRow
expect_violation JournalRotate JournalRotate.broken-unlocked NoLostRow
expect_violation SupervisorRetire SupervisorRetire.broken-delete-on-failure PlistOnlyWhenGone
expect_violation PeerContention PeerContention.broken-no-tiebreak AtMostOneHolder
expect_violation ReservationBoard ReservationBoard.broken-truncated-grants NoGrantWithoutCompleteBoard
expect_violation ReservationBoard ReservationBoard.broken-no-refresh AtMostOneHolder
expect_violation TrackerCas TrackerCas.broken-stale-splice NoClosedRowLost
expect_violation EventsDedupe EventsDedupe.broken-ring-twice AtMostOnceRing

# Every model in the directory must be exercised: a new .tla that nobody added
# an expectation for would otherwise pass silently as "not run".
for model in "$MODELS"/*.tla; do
  name="$(basename "$model" .tla)"
  grep -q "expect_pass $name $name$" "$SELF" || die "model $name has no positive expectation"
done

echo "verify-formal: ${passed} passed, ${failed} failed"
[ "$failed" -eq 0 ] || die "protocol models did not match their expectations"

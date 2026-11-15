#!/usr/bin/env bash
# Hand-written TLA+ protocol models run by TLC (ADR-0021 D5.4, ADR-0022 V11).
#
# Positive models must pass; every negative fixture (a config that removes one
# fence) must FAIL with the named invariant. Exit 0 only when both expectations
# hold. TLC and Java are pinned; neither is committed.
#
#   bash scripts/verify-formal.sh            # verify
#   bash scripts/verify-formal.sh --fetch    # fetch the pinned TLC jar first
#
# Overrides: FLYWHEEL_JAVA (java binary), FLYWHEEL_TLA_JAR (jar path).
set -euo pipefail
cd "$(dirname "$0")/.."

TLA_VERSION="1.8.0"
TLA_SHA256="e1f11b75c4b5bc4478ba0ac5cb6ae28e40a01b0016bc33e0aa97da22dfdbcd7f"
TLA_URL="https://github.com/tlaplus/tlaplus/releases/download/v${TLA_VERSION}/tla2tools.jar"
CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/flywheel/tla"
JAR="${FLYWHEEL_TLA_JAR:-$CACHE/tla2tools-${TLA_VERSION}.jar}"
MODELS=".agents/skills/verify-protocol-tla/models"
SELF="$0"

die() {
  echo "verify-formal: $*" >&2
  exit 1
}

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    sha256sum "$1" | cut -d' ' -f1
  fi
}

resolve_java() {
  if [ -n "${FLYWHEEL_JAVA:-}" ]; then
    [ -x "$FLYWHEEL_JAVA" ] || die "FLYWHEEL_JAVA=$FLYWHEEL_JAVA is not executable"
    printf '%s\n' "$FLYWHEEL_JAVA"
    return
  fi
  if command -v java >/dev/null 2>&1 && java -version >/dev/null 2>&1; then
    command -v java
    return
  fi
  local candidate
  for candidate in "$HOME"/.local/share/mise/installs/java/*/bin/java; do
    if [ -x "$candidate" ]; then
      printf '%s\n' "$candidate"
      return
    fi
  done
  die "no Java runtime found. Install a JDK (mise install java@17, or brew install openjdk@17) and re-run, or set FLYWHEEL_JAVA=/path/to/java"
}

fetch_jar() {
  mkdir -p "$CACHE"
  local tmp="$JAR.download"
  echo "verify-formal: fetching TLC ${TLA_VERSION} into $JAR" >&2
  curl -fsSL -o "$tmp" "$TLA_URL" || die "download failed: $TLA_URL"
  local got
  got="$(sha256_of "$tmp")"
  if [ "$got" != "$TLA_SHA256" ]; then
    rm -f "$tmp"
    die "tla2tools ${TLA_VERSION} sha256 mismatch: got $got, want $TLA_SHA256"
  fi
  mv "$tmp" "$JAR"
}

if [ "${1:-}" = "--fetch" ]; then
  fetch_jar
fi

[ -f "$JAR" ] || die "pinned TLC ${TLA_VERSION} missing at $JAR (want sha256 $TLA_SHA256). Run: bash scripts/verify-formal.sh --fetch"
[ "$(sha256_of "$JAR")" = "$TLA_SHA256" ] || die "TLC jar at $JAR does not match the pinned sha256 $TLA_SHA256 — delete it and re-run with --fetch"

JAVA="$(resolve_java)"
echo "verify-formal: java $JAVA"
echo "verify-formal: TLC ${TLA_VERSION} ($JAR)"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/flywheel-tla.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# TLC writes trace files beside the spec, so each run gets a fresh copy.
run_tlc() { # model cfg
  local model="$1" cfg="$2" dir="$WORK/$1-$2"
  mkdir -p "$dir"
  cp "$MODELS/$model.tla" "$MODELS/$cfg.cfg" "$dir/"
  # -metadir keeps TLC's states/ under the temp dir; the run also happens there,
  # so a check never writes into the repo (bd-wum95).
  ( cd "$dir" && "$JAVA" -cp "$JAR" tlc2.TLC -workers 1 -deadlock \
      -metadir "$dir/meta" -config "$cfg.cfg" "$model.tla" ) >"$dir/out.log" 2>&1
}

passed=0
failed=0

expect_pass() { # model cfg
  local model="$1" cfg="$2"
  if run_tlc "$model" "$cfg" && grep -q "No error has been found" "$WORK/$model-$cfg/out.log"; then
    echo "  PASS  $model ($cfg)"
    passed=$((passed + 1))
  else
    echo "  FAIL  $model ($cfg) — expected a clean run" >&2
    tail -20 "$WORK/$model-$cfg/out.log" >&2 || true
    failed=$((failed + 1))
  fi
}

expect_violation() { # model cfg invariant
  local model="$1" cfg="$2" invariant="$3"
  if run_tlc "$model" "$cfg"; then
    echo "  FAIL  $model ($cfg) — expected $invariant to be violated, TLC exited 0" >&2
    failed=$((failed + 1))
    return
  fi
  if grep -q "Invariant $invariant is violated" "$WORK/$model-$cfg/out.log"; then
    echo "  PASS  $model ($cfg) — $invariant violated as expected"
    passed=$((passed + 1))
  else
    echo "  FAIL  $model ($cfg) — expected $invariant to be violated" >&2
    tail -20 "$WORK/$model-$cfg/out.log" >&2 || true
    failed=$((failed + 1))
  fi
}

echo "verify-formal: positive models"
expect_pass JobLease JobLease
expect_pass Respawn Respawn
expect_pass DeferredHandoff DeferredHandoff
expect_pass P2pDelivery P2pDelivery
expect_pass DeferredQueue DeferredQueue
expect_pass DoorbellMailPush DoorbellMailPush
expect_pass DispatcherReap DispatcherReap
expect_pass JournalAppend JournalAppend
expect_pass SupervisorRetire SupervisorRetire
expect_pass PeerContention PeerContention

echo "verify-formal: negative fixtures (must fail)"
expect_violation JobLease JobLease.broken-steal NoSteal
expect_violation JobLease JobLease.broken-stale-settle NoStaleSettle
expect_violation JobLease JobLease.broken-revive-terminal TerminalStable
expect_violation Respawn Respawn.broken-claim-before-free OneLiveHolder
expect_violation Respawn Respawn.broken-drop-reservations ReservationsKept
expect_violation DeferredHandoff DeferredHandoff.broken-memory-only NoLostWake
expect_violation P2pDelivery P2pDelivery.broken-drop-on-busy EveryDirectiveReachesTargetOrIsJournaledPending
expect_violation DeferredQueue DeferredQueue.broken-ring-twice AtMostOnceRung
expect_violation DeferredQueue DeferredQueue.broken-strand EveryWakeAccounted
expect_violation DeferredQueue DeferredQueue.broken-preempt-any OnlyActNowPreempts
expect_violation DeferredQueue DeferredQueue.broken-preempt-unrecorded AtMostOnceRung
expect_violation DoorbellMailPush DoorbellMailPush.broken-thread-rung EveryWakeAccounted
expect_violation DispatcherReap DispatcherReap.broken-fallback-reap NeverReapOnFallback
expect_violation DispatcherReap DispatcherReap.broken-reap-live-session NeverReapLiveSession
expect_violation DispatcherReap DispatcherReap.broken-label-dropped LabelOnlyDoorKept
expect_violation JournalAppend JournalAppend.broken-interleave NoTornRow
expect_violation SupervisorRetire SupervisorRetire.broken-delete-on-failure PlistOnlyWhenGone
expect_violation PeerContention PeerContention.broken-no-tiebreak AtMostOneHolder

# Every model in the directory must be exercised: a new .tla that nobody added
# an expectation for would otherwise pass silently as "not run".
for model in "$MODELS"/*.tla; do
  name="$(basename "$model" .tla)"
  grep -q "expect_pass $name $name$" "$SELF" || die "model $name has no positive expectation"
done

echo "verify-formal: ${passed} passed, ${failed} failed"
[ "$failed" -eq 0 ] || die "protocol models did not match their expectations"

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
MODELS="${FLYWHEEL_FORMAL_MODELS_DIR:-.agents/skills/verify-protocol-tla/models}"
TRACES="${FLYWHEEL_FORMAL_TRACES_DIR:-$(dirname "$MODELS")/traces}"
case "$TRACES" in
  /*) ;;
  *) TRACES="$PWD/$TRACES" ;;
esac
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


conform_check() { # model trace -> prints the exit code; output in $WORK/conform-<name>/out.log
  local model="$1" trace="$2" dir="$WORK/conform-$(basename "$trace")" rc=0
  mkdir -p "$dir"
  cp "$MODELS/$model.tla" "$MODELS/$model.cfg" "$dir/"
  ( cd "$dir" && "$CHECKER" conform -trace "$trace" -config "$model.cfg" "$model.tla" ) >"$dir/out.log" 2>&1 || rc=$?
  echo "$rc"
}

expect_conform() { # model trace
  local model="$1" trace="$2" rc
  rc="$(conform_check "$model" "$trace")"
  if [ "$rc" = 0 ]; then
    echo "  PASS  conform $(basename "$trace")"
    passed=$((passed + 1))
  else
    echo "  FAIL  conform $(basename "$trace") — expected 0, got $rc" >&2
    tail -20 "$WORK/conform-$(basename "$trace")/out.log" >&2 || true
    failed=$((failed + 1))
  fi
}

expect_nonconform() { # model trace expected_rc step_named_in_output
  local model="$1" trace="$2" expected="$3" step="$4" rc
  rc="$(conform_check "$model" "$trace")"
  if [ "$rc" = "$expected" ] && grep -q "$step" "$WORK/conform-$(basename "$trace")/out.log"; then
    echo "  PASS  conform $(basename "$trace") — exit $rc names $step"
    passed=$((passed + 1))
  else
    echo "  FAIL  conform $(basename "$trace") — expected exit $expected with $step, got $rc" >&2
    tail -20 "$WORK/conform-$(basename "$trace")/out.log" >&2 || true
    failed=$((failed + 1))
  fi
}

echo "verify-formal: positive models"
# Discovery, not a hand list: <M>.tla + <M>.cfg (or <M>.<variant>.cfg) is a
# positive run. Adding a model is adding files, never editing this script.
for tla in "$MODELS"/*.tla; do
  [ -f "$tla" ] || continue
  model="$(basename "$tla" .tla)"
  for cfg in "$MODELS"/*.cfg; do
    [ -f "$cfg" ] || continue
    cfg_name="$(basename "$cfg" .cfg)"
    case "$cfg_name" in
      "$model" | "$model".*) ;;
      *) continue ;;
    esac
    case "$cfg_name" in
      *.broken-*) continue ;;
    esac
    expect_pass "$model" "$cfg_name"
  done
done

echo "verify-formal: negative fixtures (must fail)"
for cfg in "$MODELS"/*.broken-*.cfg; do
  [ -f "$cfg" ] || continue
  name="$(basename "$cfg" .cfg)"
  model="${name%%.*}"
  invariant="$(head -1 "$cfg" | sed -n 's/^\\\* expect: //p')"
  [ -n "$invariant" ] || die "$name has no first-line '\\* expect: <Invariant>'"
  expect_violation "$model" "$name" "$invariant"
done

echo "verify-formal: trace conformance (recorded doorbell runs)"
for trace in "$TRACES"/*.recorded.jsonl; do
  [ -f "$trace" ] || continue
  name="$(basename "$trace" .recorded.jsonl)"
  expect_conform "$name" "$trace"
done
for trace in "$TRACES"/*.jsonl; do
  [ -f "$trace" ] || continue
  case "$(basename "$trace")" in
    *.recorded.jsonl) continue ;;
  esac
  name="$(basename "$trace")"
  model="${name%%.*}"
  step="$(tail -1 "$trace" | jq -r '.action // empty' 2>/dev/null || true)"
  [ -n "$step" ] || die "$name has no final action to name"
  expect_nonconform "$model" "$trace" 20 "$step"
done

echo "verify-formal: ${passed} passed, ${failed} failed"
[ "$failed" -eq 0 ] || die "protocol models did not match their expectations"

#!/usr/bin/env bash
# One operator door for the flywheel/toron stack.
#
# Why: `why`, `/health.hosts`, and `briefing.required_skills` are only useful
# when the installed binaries are the ones the source says they are. A fresh
# `cargo install` of one crate used to leave the other two at an old
# generation, and no command compared the live host rows with the binary on
# disk.
#
# Usage:
#   scripts/stack.sh status [--check]
#   scripts/stack.sh install [--dry-run] [--no-codesign] [--no-restart]
#                            [--skip-toron]
#
# status is read-only. `--check` exits 1 when a binary is missing, the
# doorbell health endpoint is down, a live host generation differs from the
# installed flywheel, or toron daemon status fails.
#
# install also stops live hosts whose generation is stale and restarts their
# per-project supervisors, so the replacement executes the new bytes.
#
# install builds from FLYWHEEL_SOURCE (default: this repo) and TORON_SOURCE
# (default: its sibling ../toron), ad-hoc codesigns on macOS, restarts any
# installed launchd units, then runs `status --check`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FLYWHEEL_SOURCE="${FLYWHEEL_SOURCE:-$ROOT}"
if [ -n "${TORON_SOURCE:-}" ]; then
  TORON_SOURCE_RESOLVED="$TORON_SOURCE"
elif [ -d "$ROOT/../toron" ]; then
  TORON_SOURCE_RESOLVED="$(cd "$ROOT/../toron" && pwd)"
else
  TORON_SOURCE_RESOLVED=""
fi
DOORBELL_URL="${DOORBELL_URL:-http://127.0.0.1:18980}"
CHECK=0
DRY_RUN=0
NO_CODESIGN=0
NO_RESTART=0
SKIP_TORON=0

die() {
  echo "stack: $*" >&2
  exit 2
}

usage() {
  sed -n '2,25p' "$0"
  exit "${1:-0}"
}

run() {
  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'dry-run:'
    printf ' %q' "$@"
    printf '\n'
    return 0
  fi
  "$@"
}

cargo_command() {
  if [ -n "${CARGO_CMD:-}" ]; then
    # Deliberate word splitting: CARGO_CMD carries the mbx toolchain prefix.
    read -r -a CARGO_CMD_ARR <<< "$CARGO_CMD"
  elif command -v mbx >/dev/null 2>&1; then
    CARGO_CMD_ARR=(mbx +nightly-aarch64-apple-darwin)
  else
    CARGO_CMD_ARR=(cargo)
  fi
}

bin_dir() {
  printf '%s\n' "${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin"
}

installed_bins() {
  local dir
  dir="$(bin_dir)"
  printf '%s\n' "$dir/flywheel" "$dir/doorbell" "$dir/toron"
}

restart_label() {
  local label="$1"
  local domain
  domain="gui/$(id -u)/$label"
  if launchctl print "$domain" >/dev/null 2>&1; then
    run launchctl kickstart -k "$domain"
  fi
}

stale_host_rows() {
  local health="$1"
  local hash="$2"
  jq -r --arg h "$hash" '
    .hosts[]?
    | select(.state == "live")
    | select((.pid // 0) > 1)
    | select(.generation.binary_hash != null and .generation.binary_hash != "")
    | select(.generation.binary_hash != $h)
    | "\(.pid)\t\(.project // "")\t\(.workspace // "")"
  ' <<<"$health"
}

# A state file can outlive its process; do not kill a recycled pid.
host_pid_is_doorbell() {
  local pid="$1"
  ps -p "$pid" -o command= 2>/dev/null \
    | grep -Eq '(^|[ /])flywheel __doorbell([[:space:]]|$)'
}

# A binary replacement cannot make an already-running host execute the new
# bytes. Stop each live host whose watcher generation differs, then let the
# per-project supervisor respawn it from the new binary. A vanished stale pid
# is not enough: the same workspace must come back live on the new hash.
host_replacement_ready() {
  local health="$1"
  local hash="$2"
  local rows="$3"
  local project workspace
  while IFS=$'\t' read -r _ project workspace; do
    [ -n "$workspace" ] || continue
    jq -e --arg ws "$workspace" --arg h "$hash" \
      'any(.hosts[]?; .workspace == $ws and .state == "live" and .generation.binary_hash == $h)' \
      >/dev/null 2>&1 <<<"$health" || return 1
  done <<<"$rows"
  return 0
}

recover_stale_hosts() {
  local health hash rows original_rows pid project workspace label
  local waited=0 killed=0
  command -v jq >/dev/null 2>&1 || return 1
  [ -x "$(bin_dir)/flywheel" ] || return 1
  health="$(curl -fsS --max-time 2 "$DOORBELL_URL/health" 2>/dev/null)" || return 1
  hash="$(dd if="$(bin_dir)/flywheel" bs=8192 count=1 2>/dev/null | shasum -a 1 | awk '{print $1}')"
  [ -n "$hash" ] || return 1
  rows="$(stale_host_rows "$health" "$hash")"
  [ -n "$rows" ] || return 0
  original_rows="$rows"

  echo "stale live hosts: $(printf '%s\n' "$rows" | wc -l | tr -d ' '); stopping for replacement"
  while IFS=$'\t' read -r pid project workspace; do
    [ -n "$pid" ] || continue
    if ! host_pid_is_doorbell "$pid"; then
      echo "  refusing pid $pid for $workspace: command is not a flywheel doorbell host" >&2
      continue
    fi
    echo "  stopping stale host $pid ($project/$workspace)"
    kill "$pid" 2>/dev/null || true
    killed=$((killed + 1))
    if [ -n "$project" ]; then
      restart_label "dev.flywheel.doorbell.$project"
    else
      echo "  no project on $workspace row; supervisor not guessed" >&2
    fi
  done <<<"$rows"
  [ "$killed" -gt 0 ] || return 1

  while [ "$waited" -lt 90 ]; do
    sleep 2
    waited=$((waited + 2))
    health="$(curl -fsS --max-time 2 "$DOORBELL_URL/health" 2>/dev/null)" || continue
    if host_replacement_ready "$health" "$hash" "$original_rows"; then
      echo "stale hosts replaced after ${waited}s"
      return 0
    fi
  done
  echo "stale hosts not replaced after ${waited}s: $(printf '%s\n' "$original_rows" | tr '\n' ' ')" >&2
  return 1
}

show_status() {
  local bad=0
  local health=""
  local current_hash=""
  local stale=0
  local bin path version dir health_version installed_version toron_json
  dir="$(bin_dir)"

  if ! command -v jq >/dev/null 2>&1; then
    echo "stack: jq is required for status (brew install jq)" >&2
    return 1
  fi

  echo "== binaries =="
  for bin in toron flywheel doorbell; do
    if [ -x "$dir/$bin" ]; then
      path="$dir/$bin"
    elif path="$(command -v "$bin" 2>/dev/null)"; then
      :
    else
      printf '%-9s missing from PATH and %s\n' "$bin" "$dir"
      bad=1
      continue
    fi
    if version="$("$path" --version 2>&1)"; then
      printf '%-9s %s\n          %s\n' "$bin" "$version" "$path"
    else
      printf '%-9s version probe failed: %s\n          %s\n' "$bin" "$version" "$path"
      bad=1
    fi
  done

  echo
  echo "== doorbell =="
  if health="$(curl -fsS --max-time 2 "$DOORBELL_URL/health" 2>/dev/null)"; then
    if command -v jq >/dev/null 2>&1; then
      jq -r '"version \(.build.version // "?"), status \(.status // "?"), mode \(.mode // "?")"' <<<"$health"
      health_version="$(jq -r '.build.version // empty' <<<"$health")"
      installed_version="$("$dir/doorbell" --version 2>/dev/null | awk '{print $2}')"
      if [ -n "$health_version" ] && [ -n "$installed_version" ] && [ "$health_version" != "$installed_version" ]; then
        echo "version skew: /health $health_version, $dir/doorbell $installed_version"
        bad=1
      fi
      if jq -e '.hosts | type == "array" and length > 0' >/dev/null 2>&1 <<<"$health"; then
        echo "hosts:"
        jq -r '.hosts[] | "  \(.workspace)\t\(.state)\tpid=\(.pid // "-")\tgen=\(.generation.binary_hash[0:12] // "?")\tage=\(.last_seen_age_s // "?" )s"' <<<"$health"
        if command -v shasum >/dev/null 2>&1 && [ -x "$dir/flywheel" ]; then
          # Host generations hash the first 8 KiB with SHA-1 (WatcherGen::capture).
          current_hash="$(dd if="$dir/flywheel" bs=8192 count=1 2>/dev/null | shasum -a 1 | awk '{print $1}')"
          stale="$(jq --arg h "$current_hash" '[.hosts[] | select(.state == "live" and .generation.binary_hash != null and .generation.binary_hash != $h)] | length' <<<"$health")"
          if [ "$stale" -gt 0 ]; then
            echo "stale live hosts: $stale generation(s) differ from $dir/flywheel"
            bad=1
          fi
        fi
      else
        echo "hosts: none"
      fi
    else
      printf '%s\n' "$health"
    fi
  else
    echo "health unavailable at $DOORBELL_URL/health"
    bad=1
  fi

  echo
  echo "== toron =="
  if command -v toron >/dev/null 2>&1; then
    if ! toron_json="$(toron daemon status --json)"; then
      echo "toron daemon status failed" >&2
      bad=1
    else
      printf '%s\n' "$toron_json"
      if jq -e '.build.restart_required == true' >/dev/null 2>&1 <<<"$toron_json"; then
        echo "toron restart required: a replaced build is still running"
        bad=1
      fi
    fi
  else
    bad=1
  fi

  if [ "$CHECK" -eq 1 ] && [ "$bad" -ne 0 ]; then
    return 1
  fi
  return 0
}

do_status() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --check) CHECK=1; shift ;;
      -h|--help) usage 0 ;;
      *) die "unknown status arg: $1" ;;
    esac
  done
  show_status
}

do_install() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --dry-run) DRY_RUN=1; shift ;;
      --no-codesign) NO_CODESIGN=1; shift ;;
      --no-restart) NO_RESTART=1; shift ;;
      --skip-toron) SKIP_TORON=1; shift ;;
      -h|--help) usage 0 ;;
      *) die "unknown install arg: $1" ;;
    esac
  done

  [ -f "$FLYWHEEL_SOURCE/Cargo.toml" ] || die "no Cargo.toml at $FLYWHEEL_SOURCE (set FLYWHEEL_SOURCE)"
  [ -f "$FLYWHEEL_SOURCE/crates/doorbell-cli/Cargo.toml" ] || die "no doorbell crate at $FLYWHEEL_SOURCE/crates/doorbell-cli"
  if [ "$SKIP_TORON" -eq 0 ]; then
    [ -n "$TORON_SOURCE_RESOLVED" ] || die "no toron source (set TORON_SOURCE)"
    [ -f "$TORON_SOURCE_RESOLVED/crates/toron-cli/Cargo.toml" ] || die "no toron crate at $TORON_SOURCE_RESOLVED/crates/toron-cli"
  fi

  cargo_command
  echo "installing flywheel from $FLYWHEEL_SOURCE"
  run "${CARGO_CMD_ARR[@]}" install --path "$FLYWHEEL_SOURCE" --force
  echo "installing doorbell from $FLYWHEEL_SOURCE/crates/doorbell-cli"
  run "${CARGO_CMD_ARR[@]}" install --path "$FLYWHEEL_SOURCE/crates/doorbell-cli" --force
  if [ "$SKIP_TORON" -eq 0 ]; then
    echo "installing toron from $TORON_SOURCE_RESOLVED/crates/toron-cli"
    run "${CARGO_CMD_ARR[@]}" install --path "$TORON_SOURCE_RESOLVED/crates/toron-cli" --force
  fi

  if [ "$(uname -s)" = "Darwin" ] && [ "$NO_CODESIGN" -eq 0 ]; then
    while IFS= read -r bin; do
      if [ "$SKIP_TORON" -eq 1 ] && [ "${bin##*/}" = "toron" ]; then
        continue
      fi
      if [ -f "$bin" ]; then
        echo "codesigning $bin"
        run codesign --force --sign - "$bin"
      fi
    done < <(installed_bins)
  fi

  if [ "$NO_RESTART" -eq 0 ]; then
    if [ "$(uname -s)" = "Darwin" ] && command -v launchctl >/dev/null 2>&1; then
      for label in com.doorbell.serve dev.flywheel.daemon dev.flywheel.cron; do
        restart_label "$label"
      done
      # Per-project supervisors are named dev.flywheel.doorbell.<project>.
      while IFS= read -r label; do
        restart_label "$label"
      done < <(launchctl list 2>/dev/null | awk '$3 ~ /^dev\.flywheel\.doorbell\./ {print $3}')
      if [ "$SKIP_TORON" -eq 0 ]; then
        restart_label com.toron.serve
      fi
    else
      echo "no launchd restart on this host; restart the services when ready"
    fi
  fi

  if [ "$DRY_RUN" -eq 0 ] && [ "$NO_RESTART" -eq 0 ]; then
    # Keepalive needs a beat to bind and publish /health.
    sleep 2
    recover_stale_hosts
    sleep 1
    CHECK=1
    show_status
  fi
}

if [ $# -eq 0 ]; then
  usage 2
fi
cmd="$1"
shift
case "$cmd" in
  status) do_status "$@" ;;
  install) do_install "$@" ;;
  -h|--help|help) usage 0 ;;
  *) die "unknown command: $cmd" ;;
esac

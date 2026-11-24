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
#                            [--skip-toron] [--operator]
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

RESTART_FAILURES=""

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
OPERATOR=0

die() {
  echo "stack: $*" >&2
  exit 2
}

# bd-9zw9u: the toron logrotate LaunchAgent is written by toron's own install,
# which stack install does not call. A host installed by an older binary keeps
# the invalid raw-&& plist forever, so rewrite it here when it fails plutil.
LOGROTATE_PLIST="${HOME:-}/Library/LaunchAgents/com.toron.logrotate.plist"

logrotate_plist_valid() {
  [ -n "${HOME:-}" ] || return 0
  [ -f "$LOGROTATE_PLIST" ] || return 0
  command -v plutil >/dev/null 2>&1 || return 0
  plutil -lint "$LOGROTATE_PLIST" >/dev/null 2>&1
}

rewrite_logrotate_plist() {
  if logrotate_plist_valid; then
    return 0
  fi
  [ -n "${HOME:-}" ] || return 0
  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'dry-run: rewrite %s\n' "$LOGROTATE_PLIST"
    return 0
  fi
  mkdir -p "$(dirname "$LOGROTATE_PLIST")"
  cat >"$LOGROTATE_PLIST" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.toron.logrotate</string>
	<key>ProgramArguments</key>
	<array>
		<string>/bin/sh</string>
		<string>-c</string>
		<string>for f in "$HOME/.local/state/toron/serve.out.log" "$HOME/.local/state/toron/serve.err.log"; do [ -f "$f" ] || continue; sz=$(stat -f%z "$f" 2>/dev/null || stat -c%s "$f"); if [ "$sz" -gt 1048576 ]; then cp "$f" "$f.1" &amp;&amp; : > "$f"; fi; done</string>
	</array>
	<key>StartInterval</key>
	<integer>3600</integer>
</dict>
</plist>
PLIST
  echo "rewrote invalid toron logrotate plist: $LOGROTATE_PLIST"
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
  printf '%s\n' "$dir/flywheel" "$dir/toron"
}

# bd-i4edn: launchd refuses to spawn a replaced binary ("needs LWCR update",
# exit 78) until the job is re-registered; kickstart -k does not do that.
restart_label() {
  local label="$1"
  local uid domain plist out rc
  uid="$(id -u)"
  domain="gui/$uid/$label"
  plist="$HOME/Library/LaunchAgents/$label.plist"
  if [ -f "$plist" ]; then
    run launchctl bootout "$domain" 2>/dev/null || true
    if [ "${DRY_RUN:-0}" -eq 1 ]; then
      run launchctl bootstrap "gui/$uid" "$plist"
      return 0
    fi
    # bd-rgs02: launchd needs a beat to finish the bootout. Bootstrapping in
    # the same instant returns "Bootstrap failed: 5: Input/output error" even
    # though a later bootstrap of the same plist succeeds. Wait for the job
    # to leave the plane before bringing it back (bounded: never hang install).
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      launchctl print "$domain" >/dev/null 2>&1 || break
      sleep 0.2
    done
    # bd-rgs02: a bootstrap failure (e.g. launchd I/O error 5) must name the
    # job and show why, then let the OTHER jobs restart. The final doctor
    # fails the install, so one bad job cannot hide the rest of the report.
    rc=0
    out="$(launchctl bootstrap "gui/$uid" "$plist" 2>&1)" || rc=$?
    if [ "$rc" -ne 0 ]; then
      echo "restart $label: launchctl bootstrap failed (exit $rc): $(printf '%s' "$out" | tail -3)" >&2
      RESTART_FAILURES+="$label "
      return 0
    fi
  elif launchctl print "$domain" >/dev/null 2>&1; then
    rc=0
    out="$(launchctl kickstart -k "$domain" 2>&1)" || rc=$?
    if [ "$rc" -ne 0 ]; then
      echo "restart $label: launchctl kickstart failed (exit $rc): $(printf '%s' "$out" | tail -3)" >&2
      RESTART_FAILURES+="$label "
      return 0
    fi
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
  ' <<<"$health" | while IFS=$'\t' read -r pid project workspace; do
    [ -n "$pid" ] || continue
    if kill -0 "$pid" 2>/dev/null; then
      printf '%s\t%s\t%s\n' "$pid" "$project" "$workspace"
    fi
  done
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
  # ADR-0020: the legacy :18980 doorbell daemon is retired. If it is absent,
  # there are no stale legacy hosts to replace — that is success, not failure.
  health="$(curl -fsS --max-time 2 "$DOORBELL_URL/health" 2>/dev/null)" || {
    echo "no legacy doorbell daemon; nothing to replace"
    return 0
  }
  command -v jq >/dev/null 2>&1 || return 1
  [ -x "$(bin_dir)/flywheel" ] || return 1
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

  local timeout="${RECOVER_STALE_TIMEOUT:-90}"
  while [ "$waited" -lt "$timeout" ]; do
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
  for bin in toron flywheel; do
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
  if [ -x "$dir/doorbell" ] || command -v doorbell >/dev/null 2>&1; then
    path="${dir}/doorbell"
    [ -x "$path" ] || path="$(command -v doorbell 2>/dev/null)"
    version="$("$path" --version 2>&1 || echo unknown)"
    printf '%-9s %s (legacy, sunset by ADR-0020)\n          %s\n' "doorbell" "$version" "$path"
  fi

  echo
  echo "== doorbell =="
  if health="$(curl -fsS --max-time 2 "$DOORBELL_URL/health" 2>/dev/null)"; then
    if command -v jq >/dev/null 2>&1; then
      jq -r '"version \(.build.version // "?"), status \(.status // "?"), mode \(.mode // "?")"' <<<"$health"
      health_version="$(jq -r '.build.version // empty' <<<"$health")"
      installed_version="$("$dir/doorbell" --version 2>/dev/null | awk '{print $2}')"
      if [ -n "$health_version" ] && [ -n "$installed_version" ] && [ "$health_version" != "$installed_version" ]; then
        echo "version skew: /health $health_version, $dir/doorbell $installed_version"
      fi
      if jq -e '.hosts | type == "array" and length > 0' >/dev/null 2>&1 <<<"$health"; then
        echo "hosts:"
        jq -r '.hosts[] | "  \(.workspace)\t\(.state)\tpid=\(.pid // "-")\tgen=\(.generation.binary_hash[0:12] // "?")\tage=\(.last_seen_age_s // "?" )s"' <<<"$health"
        if command -v shasum >/dev/null 2>&1 && [ -x "$dir/flywheel" ]; then
          # Host generations hash the first 8 KiB with SHA-1 (WatcherGen::capture).
          current_hash="$(dd if="$dir/flywheel" bs=8192 count=1 2>/dev/null | shasum -a 1 | awk '{print $1}')"
          stale="$(stale_host_rows "$health" "$current_hash" | grep -c . || true)"
          if [ "$stale" -gt 0 ]; then
            echo "stale live hosts: $stale generation(s) differ from $dir/flywheel"
          fi
        fi
      else
        echo "hosts: none"
      fi
    else
      printf '%s\n' "$health"
    fi
  else
    echo "consolidated into toron daemon on :18080 (port :18980 retired per ADR-0020)"
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

  if [ -n "${HOME:-}" ] && [ -f "$LOGROTATE_PLIST" ]; then
    if command -v plutil >/dev/null 2>&1 && ! plutil -lint "$LOGROTATE_PLIST" >/dev/null 2>&1; then
      echo "invalid plist: $LOGROTATE_PLIST" >&2
      bad=1
    else
      echo "com.toron.logrotate.plist: ok"
    fi
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

# bd-0uzrq.24: replacing the installed binaries is a fleet mutation, so the
# rule lives in the tool and this door asks it. A crew pin, or a crew pane that
# unset AGENT_NAME, is refused; `--operator` is the deliberate escape. Fail
# CLOSED: no binary to answer, or a binary too old to know the verb, refuses.
require_fleet_authority() {
  [ "$OPERATOR" -eq 1 ] && return 0
  local fw
  fw="$(command -v flywheel 2>/dev/null || true)"
  if [ -z "$fw" ]; then
    die "install is captain-only and no flywheel binary is on PATH to prove the caller's identity; an operator runs it with --operator"
  fi
  "$fw" __guard-fleet --what "stack install" \
    || die "refused: replacing the installed binaries is captain-only (an operator passes --operator)"
}

do_install() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --dry-run) DRY_RUN=1; shift ;;
      --no-codesign) NO_CODESIGN=1; shift ;;
      --no-restart) NO_RESTART=1; shift ;;
      --skip-toron) SKIP_TORON=1; shift ;;
      --operator) OPERATOR=1; shift ;;
      -h|--help) usage 0 ;;
      *) die "unknown install arg: $1" ;;
    esac
  done

  require_fleet_authority

  [ -f "$FLYWHEEL_SOURCE/Cargo.toml" ] || die "no Cargo.toml at $FLYWHEEL_SOURCE (set FLYWHEEL_SOURCE)"
  if [ "$SKIP_TORON" -eq 0 ]; then
    [ -n "$TORON_SOURCE_RESOLVED" ] || die "no toron source (set TORON_SOURCE)"
    [ -f "$TORON_SOURCE_RESOLVED/crates/toron-cli/Cargo.toml" ] || die "no toron crate at $TORON_SOURCE_RESOLVED/crates/toron-cli"
  fi

  cargo_command
  echo "installing flywheel from $FLYWHEEL_SOURCE"
  run "${CARGO_CMD_ARR[@]}" install --path "$FLYWHEEL_SOURCE" --force
  if [ "${INSTALL_LEGACY_DOORBELL:-0}" -eq 1 ] && [ -f "$FLYWHEEL_SOURCE/crates/doorbell-cli/Cargo.toml" ]; then
    echo "installing legacy doorbell from $FLYWHEEL_SOURCE/crates/doorbell-cli"
    run "${CARGO_CMD_ARR[@]}" install --path "$FLYWHEEL_SOURCE/crates/doorbell-cli" --force
  fi
  if [ "$SKIP_TORON" -eq 0 ]; then
    echo "installing toron from $TORON_SOURCE_RESOLVED/crates/toron-cli"
    run "${CARGO_CMD_ARR[@]}" install --path "$TORON_SOURCE_RESOLVED/crates/toron-cli" --force
  fi

  # bd-9os20: seats load the plugin with --plugin-dir, so install from this export.
  plugin_src="$FLYWHEEL_SOURCE/plugins/claude-flywheel"
  plugin_dir="${XDG_DATA_HOME:-$HOME/.local/share}/flywheel/plugins/claude-flywheel"
  if [ -f "$plugin_src/hooks/hooks.json" ]; then
    echo "installing Claude wake plugin into $plugin_dir"
    run mkdir -p "${plugin_dir%/*}"
    run rm -rf "$plugin_dir"
    run cp -R "$plugin_src" "$plugin_dir"
  fi

  # bd-zc7b4: the pi events extension must survive proof-export cleanup, so
  # install it beside the Claude plugin under the durable data home.
  ext_src="$FLYWHEEL_SOURCE/extensions/pi-flywheel-events"
  ext_dir="${XDG_DATA_HOME:-$HOME/.local/share}/flywheel/extensions/pi-flywheel-events"
  if [ -f "$ext_src/src/index.ts" ]; then
    echo "installing pi events extension into $ext_dir"
    run mkdir -p "${ext_dir%/*}"
    run rm -rf "$ext_dir"
    run cp -R "$ext_src" "$ext_dir"
  fi

  rewrite_logrotate_plist

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
      domain="gui/$(id -u)/com.doorbell.serve"
      if launchctl print "$domain" >/dev/null 2>&1; then
        echo "retiring standalone doorbell daemon ($domain)"
        launchctl bootout "$domain" 2>/dev/null || true
      fi
      for label in dev.flywheel.daemon dev.flywheel.cron; do
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
    if [ -n "$RESTART_FAILURES" ]; then
      echo "restart failures (doctor will decide): $RESTART_FAILURES" >&2
    fi
    CHECK=1
    show_status
  fi
}

main() {
  if [ $# -eq 0 ]; then
    usage 2
  fi
  local cmd="$1"
  shift
  case "$cmd" in
    status) do_status "$@" ;;
    install) do_install "$@" ;;
    -h|--help|help) usage 0 ;;
    *) die "unknown command: $cmd" ;;
  esac
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  main "$@"
fi

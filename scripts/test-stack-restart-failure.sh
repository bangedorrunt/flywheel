#!/usr/bin/env bash
# bd-rgs02: a launchctl bootstrap failure must name the job, show its exit and
# stderr, and NOT abort the other jobs. The final doctor fails the install.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/stack-restart-failure.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

mkdir -p "$TMP/bin" "$TMP/home/Library/LaunchAgents"
export PATH="$TMP/bin:$PATH"
export HOME="$TMP/home"
export LAUNCHCTL_LOG="$TMP/launchctl.log"
DRY_RUN=0

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

cat >"$TMP/bin/launchctl" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$LAUNCHCTL_LOG"
case "$1" in
  bootstrap)
    case "$*" in
      *bad*) echo "Bootstrap failed: 5: Input/output error" >&2; exit 5 ;;
    esac ;;
esac
exit 0
SH
chmod +x "$TMP/bin/launchctl"

for label in good dev.flywheel.bad last; do
  printf '<plist version="1.0"><dict/></plist>\n' >"$HOME/Library/LaunchAgents/$label.plist"
done

out="$(
  # shellcheck disable=SC1091
  . "$ROOT/scripts/stack.sh"
  restart_label good
  restart_label dev.flywheel.bad
  restart_label last
)" 2>&1 || fail "one bad job must not abort the restart loop: $out"

uid="$(id -u)"
for label in good dev.flywheel.bad last; do
  grep -qxF "bootstrap gui/$uid $HOME/Library/LaunchAgents/$label.plist" "$LAUNCHCTL_LOG" \
    || fail "job $label was not attempted: $(cat "$LAUNCHCTL_LOG")"
done
grep -qF 'restart dev.flywheel.bad: launchctl bootstrap failed (exit 5)' <<<"$out" \
  || fail "the failing job and exit must be named: $out"
grep -qF 'Bootstrap failed: 5: Input/output error' <<<"$out" \
  || fail "the stderr tail must be shown: $out"
pass "a failed job is named and diagnosed, and the other jobs still restart"
echo ok

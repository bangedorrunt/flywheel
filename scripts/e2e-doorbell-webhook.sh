#!/usr/bin/env bash
# e2e-doorbell-webhook.sh — bd-8ulu acceptance gate. Fail-closed.
#
# Proves the whole chain live, with no swarm and no panes:
#   subscription row (identity x scope x sink x endpoint)
#     -> daemon registers the endpoint's adapter at host start
#       -> a watch-line wake for a PANE-LESS subscribed pin (digest-bound,
#          bd-bk9t) routes to that adapter
#         -> the operator's endpoint receives the POST on the URL's own
#            port and path.
#
# A `herdr` stand-in (`true`) reports no panes, which is exactly the
# condition the digest route exists for: a human wake with zero live swarm.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/debug/doorbell"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/doorbell-webhook.XXXXXX")"
PORT="${DOORBELL_WEBHOOK_TEST_PORT:-18994}"
HOOK_PORT="${DOORBELL_HOOK_TEST_PORT:-18995}"
FAIL=0

pass() { echo "PASS: $1"; }
fail() {
  echo "FAIL: $1"
  FAIL=1
}

cleanup() {
  [ -n "${DAEMON_PID:-}" ] && kill "$DAEMON_PID" >/dev/null 2>&1
  [ -n "${HOOK_PID:-}" ] && kill "$HOOK_PID" >/dev/null 2>&1
  sleep 0.3
  if [ "$FAIL" -eq 0 ]; then rm -rf "$TMP"; else echo "-- artifacts kept: $TMP"; fi
}
trap cleanup EXIT

echo "== bd-8ulu webhook sink: subscription -> route -> HTTP POST =="
[ -x "$BIN" ] || {
  fail "doorbell binary missing (build: mbx +nightly-aarch64-apple-darwin build -p doorbell-cli)"
  echo "RESULT: FAIL"
  exit 1
}

PROJ="$TMP/proj"
FW="$TMP/fw"       # flywheel state dir (journals, subscriptions, leases)
DS="$TMP/ds"       # doorbell state dir (pidfile, log)
mkdir -p "$FW/doorbell" "$FW/journals" "$DS"

# 1. A subscription row: one pin, webhook sink, its OWN endpoint.
#    Per-subscription routing is the point — the endpoint is the row's, not
#    a daemon-wide setting.
cat >"$FW/doorbell/subscriptions.json" <<JSON
{
  "cli-probe-WakeProbe": {
    "id": "cli-probe-WakeProbe",
    "identity": { "Pin": "WakeProbe" },
    "scope": { "One": "w1" },
    "filter": { "classes": [], "kinds": [] },
    "sink": "Webhook",
    "budget": { "window_minutes": 30, "max_prompts": 3 },
    "project": "$PROJ",
    "endpoint": "http://127.0.0.1:$HOOK_PORT/wake"
  }
}
JSON

# 2. The operator's endpoint. Records what actually arrived.
cat >"$TMP/hook.py" <<'PY'
import http.server, sys

LOG = sys.argv[1]
PORT = int(sys.argv[2])


class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(n).decode("utf-8", "replace")
        with open(LOG, "a") as f:
            f.write("PATH %s\nHOST %s\n%s\n" % (self.path, self.headers.get("Host"), body))
        self.send_response(200)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *a):
        pass


http.server.HTTPServer(("127.0.0.1", PORT), H).serve_forever()
PY
python3 "$TMP/hook.py" "$TMP/hook.log" "$HOOK_PORT" &
HOOK_PID=$!
sleep 0.5
kill -0 "$HOOK_PID" 2>/dev/null || {
  fail "hook receiver did not start on $HOOK_PORT"
  echo "RESULT: FAIL"
  exit 1
}

# 3. A toron stand-in: watch lines addressed to the subscribed pin, and an
#    empty inbox (the wake is the ring, not a mail body).
#
#    Digest is a BATCHING route (10 lines / 300s default), so the harness
#    emits one past the threshold instead of waiting out the interval.
cat >"$TMP/toron" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  watch)
    for i in $(seq 1 12); do
      echo "{\"kind\":9,\"id\":\"ab$i\",\"tags\":[[\"p\",\"WakeProbe\"],[\"h\",\"ch-1\"]],\"content\":\"@WakeProbe hold expired\"}"
    done
    sleep 600
    ;;
  *) echo '[]' ;;
esac
EOF
chmod +x "$TMP/toron"

# 4. Host the workspace. `herdr` = true -> no panes known -> the wake is
#    digest-bound (bd-bk9t), which is precisely the no-swarm human case.
export DOORBELL_STATE_DIR="$DS"
export DOORBELL_PORT="$PORT"
export DOORBELL_FLYWHEEL_STATE_DIR="$FW"
export DOORBELL_WEBHOOK_ALLOWLIST="127.0.0.1"
export DOORBELL_TORON_BIN="$TMP/toron"
export DOORBELL_HERDR_BIN=true

"$BIN" serve --state-dir "$DS" --port "$PORT" --flywheel-state-dir "$FW" \
  --host "$PROJ/w1" >"$TMP/daemon.log" 2>&1 &
DAEMON_PID=$!

for _ in $(seq 1 40); do
  grep -q "registered webhook:http://127.0.0.1:$HOOK_PORT/wake" "$TMP/daemon.log" && break
  sleep 0.5
done
if grep -q "registered webhook:http://127.0.0.1:$HOOK_PORT/wake" "$TMP/daemon.log"; then
  pass "daemon registered the row's endpoint (per-subscription route)"
else
  fail "endpoint adapter never registered; daemon.log:"
  tail -20 "$TMP/daemon.log"
fi

# 5. Wait for the digest route to ship the pane-less pin's wake.
for _ in $(seq 1 120); do
  [ -s "$TMP/hook.log" ] && break
  sleep 0.5
done

if [ -s "$TMP/hook.log" ]; then
  pass "endpoint received a POST"
else
  fail "no POST reached the endpoint (wake routed nowhere)"
  echo "--- daemon.log ---"
  tail -30 "$TMP/daemon.log"
  echo "--- journal ---"
  cat "$FW/journals/doorbell-journal-w1.jsonl" 2>/dev/null | tail -10
fi

if grep -q "^PATH /wake$" "$TMP/hook.log" 2>/dev/null; then
  pass "the URL's PATH was honoured (regression: send path hardcoded /)"
else
  fail "POST path was not /wake:"
  head -3 "$TMP/hook.log" 2>/dev/null
fi

if grep -q "^HOST 127.0.0.1:$HOOK_PORT$" "$TMP/hook.log" 2>/dev/null; then
  pass "the URL's PORT was honoured (regression: send path hardcoded :80)"
else
  fail "POST went to the wrong port:"
  head -3 "$TMP/hook.log" 2>/dev/null
fi

if grep -q "WakeProbe" "$TMP/hook.log" 2>/dev/null; then
  pass "the wake's pin is named in the delivered payload"
else
  fail "payload does not name the pin:"
  cat "$TMP/hook.log" 2>/dev/null | head -5
fi

# 6. Fail-closed: an off-allowlist endpoint must register NOTHING.
cat >"$TMP/toron2" <<'EOF'
#!/usr/bin/env bash
echo '[]'
EOF
chmod +x "$TMP/toron2"
sed -i '' "s#http://127.0.0.1:$HOOK_PORT/wake#http://evil.example/wake#" \
  "$FW/doorbell/subscriptions.json" 2>/dev/null ||
  sed -i "s#http://127.0.0.1:$HOOK_PORT/wake#http://evil.example/wake#" \
    "$FW/doorbell/subscriptions.json"

kill "$DAEMON_PID" >/dev/null 2>&1
wait "$DAEMON_PID" 2>/dev/null
rm -f "$TMP/hook.log"
: >"$TMP/daemon2.log"
"$BIN" serve --state-dir "$DS" --port "$PORT" --flywheel-state-dir "$FW" \
  --host "$PROJ/w1" >"$TMP/daemon2.log" 2>&1 &
DAEMON_PID=$!
for _ in $(seq 1 40); do
  grep -q "refused (D6 egress)" "$TMP/daemon2.log" && break
  sleep 0.5
done
if grep -q "refused (D6 egress)" "$TMP/daemon2.log"; then
  pass "off-allowlist endpoint refused loudly (fail-closed)"
else
  fail "off-allowlist endpoint was not refused; daemon2.log:"
  tail -20 "$TMP/daemon2.log"
fi
sleep 3
if [ ! -s "$TMP/hook.log" ]; then
  pass "nothing was sent to the refused endpoint"
else
  fail "a refused endpoint still received traffic:"
  cat "$TMP/hook.log"
fi

echo
if [ "$FAIL" -eq 0 ]; then
  echo "RESULT: PASS"
else
  echo "RESULT: FAIL"
fi
exit "$FAIL"

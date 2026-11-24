#!/usr/bin/env bash
set -euo pipefail

HERE="$(builtin cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/measure-crew-tokens.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

DB="$TMP/opencode.db"
sqlite3 "$DB" <<'SQL'
CREATE TABLE session_message (id text PRIMARY KEY, session_id text, type text, seq integer, time_created integer, time_updated integer, data text);
CREATE TABLE session_part (message_id text, session_id text, ordinal integer, type text, call_id text, time_created integer, time_updated integer, data text);
INSERT INTO session_message VALUES ('m0','ses_lean','user',0,1,1,'{"text":"brief"}');
INSERT INTO session_message VALUES ('m1','ses_lean','assistant',1,2,2,'{"tokens":{"input":12000,"output":10,"cache":{"read":500,"write":0}}}');
INSERT INTO session_part VALUES ('m1','ses_lean',0,'tool','c1',2,2,'{"type":"tool","name":"shell","state":{"input":{"command":"toron next --project flywheel --as X"}}}');
INSERT INTO session_message VALUES ('m2','ses_lean','assistant',2,3,3,'{"tokens":{"input":1000,"output":10,"cache":{"read":13000,"write":0}}}');
INSERT INTO session_part VALUES ('m2','ses_lean',0,'tool','c2',3,3,'{"type":"tool","name":"read","state":{"input":{"filePath":"/repo/src/decide.rs"}}}');
INSERT INTO session_message VALUES ('n1','ses_noread','assistant',1,2,2,'{"tokens":{"input":9000,"output":10,"cache":{"read":0,"write":0}}}');
INSERT INTO session_part VALUES ('n1','ses_noread',0,'tool','c3',2,2,'{"type":"tool","name":"shell","state":{"input":{"command":"toron next"}}}');
SQL

out="$(OPENCODE_DB="$DB" bash "$SCRIPT" --json opencode:ses_lean)" || fail "lean opencode session should pass: $out"
python3 - "$out" <<'PY' || fail "lean opencode numbers: $out"
import json, sys
r = json.loads(sys.argv[1])[0]
assert r["harness"] == "opencode", r
assert r["pre_source_read_input_tokens"] == 12500, r
assert r["first_source_read_turn"] == 2, r
assert r["target_lt"] == 15000, r
assert r["pass"] is True, r
PY

if out="$(OPENCODE_DB="$DB" bash "$SCRIPT" --json opencode:ses_noread)"; then
  fail "a session with no source read is not a measurement: $out"
fi
python3 - "$out" <<'PY' || fail "no-read result: $out"
import json, sys
r = json.loads(sys.argv[1])[0]
assert r["first_source_read_turn"] is None, r
assert r["pre_source_read_input_tokens"] is None, r
assert r["pass"] is False, r
PY

JSONL="$TMP/claude.jsonl"
cat > "$JSONL" <<'JSON'
{"type":"assistant","message":{"role":"assistant","usage":{"input_tokens":3,"cache_read_input_tokens":20000,"cache_creation_input_tokens":1000},"content":[{"type":"tool_use","name":"REPL","input":{"code":"await $`echo $AGENT_NAME`"}}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"streamed"}]}}
{"type":"assistant","message":{"role":"assistant","usage":{"input_tokens":5,"cache_read_input_tokens":21500,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","name":"REPL","input":{"code":"await file('src/pane.rs').text()"}}]}}
JSON
out="$(bash "$SCRIPT" --json --harness claude "$JSONL")" || fail "claude REPL session should pass: $out"
python3 - "$out" <<'PY' || fail "claude REPL numbers: $out"
import json, sys
r = json.loads(sys.argv[1])[0]
assert r["first_source_read_turn"] == 2, r
assert r["pre_source_read_input_tokens"] == 21003, r
PY

echo "ok: measure-crew-tokens"

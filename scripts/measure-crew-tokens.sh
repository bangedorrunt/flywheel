#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: measure-crew-tokens.sh [--harness auto|claude|pi|opencode] [--json] <source>...

D1/R8 gate: input tokens on the turn just before a crew's first source-file
read. Targets: claude <25,000; pi and opencode <15,000.

A source is a Claude or pi session JSONL path, or opencode:<session_id>
(read from $OPENCODE_DB, default ~/.local/share/opencode/opencode.db).
A session with no source read has no measurement and fails.
EOF
}

HARNESS="auto"
JSON_OUT=0
SOURCES=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --harness)
      HARNESS="${2:?--harness requires a value}"
      shift 2
      ;;
    --json)
      JSON_OUT=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      SOURCES+=("$1")
      shift
      ;;
  esac
done

if [[ ${#SOURCES[@]} -eq 0 ]]; then
  usage >&2
  exit 2
fi

OPENCODE_DB="${OPENCODE_DB:-$HOME/.local/share/opencode/opencode.db}" \
python3 - "$HARNESS" "$JSON_OUT" "${SOURCES[@]}" <<'PY'
import json
import os
import pathlib
import sqlite3
import sys

harness_opt = sys.argv[1]
json_out = sys.argv[2] == "1"
sources = sys.argv[3:]

TARGETS = {"claude": 25000, "pi": 15000, "opencode": 15000}
FILE_TOOLS = {"read", "Read", "grep", "Grep", "Glob", "glob", "find_files", "multi_grep"}
COMMAND_TOOLS = {"bash", "Bash", "shell", "REPL"}
SOURCE_HINTS = ("rg ", "grep ", "cat ", "head ", "sed ", "src/", "crates/", "file(")
SKIP_SUBSTRINGS = ("SKILL.md", "crew-card.md", ".agents/skills/", "toron next")


def is_source_read(name: str, arg_json: str) -> bool:
    if any(skip in arg_json for skip in SKIP_SUBSTRINGS):
        return False
    if name in FILE_TOOLS:
        return True
    if name in COMMAND_TOOLS:
        return any(tok in arg_json for tok in SOURCE_HINTS)
    return False


def jsonl_input_tokens(usage: dict) -> int:
    if not isinstance(usage, dict):
        return 0
    return (
        int(usage.get("input_tokens", 0) or usage.get("input", 0) or 0)
        + int(usage.get("cache_read_input_tokens", 0) or usage.get("cacheRead", 0) or 0)
        + int(usage.get("cache_creation_input_tokens", 0) or usage.get("cacheWrite", 0) or 0)
    )


def jsonl_tool_calls(msg: dict) -> list[tuple[str, str]]:
    content = msg.get("content")
    if not isinstance(content, list):
        return []
    calls = []
    for block in content:
        if isinstance(block, dict) and block.get("type") in ("tool_use", "toolCall"):
            inp = block.get("input") or block.get("arguments") or {}
            calls.append((str(block.get("name", "")), json.dumps(inp, ensure_ascii=False)))
    return calls


def jsonl_turns(path: pathlib.Path) -> tuple[str, list[tuple[int, list]]]:
    entries = []
    with path.open("r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if line:
                try:
                    entries.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    harness = harness_opt
    if harness == "auto":
        pi = "/.pi/" in str(path) or any(e.get("type") == "session" for e in entries[:5])
        harness = "pi" if pi else "claude"
    turns = []
    for entry in entries:
        msg = entry.get("message") if isinstance(entry.get("message"), dict) else entry
        if msg.get("role") != "assistant":
            continue
        usage = msg.get("usage") or entry.get("usage") or {}
        turns.append((jsonl_input_tokens(usage), jsonl_tool_calls(msg)))
    return harness, turns


def opencode_turns(session_id: str) -> list[tuple[int, list]]:
    db = os.environ["OPENCODE_DB"]
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    try:
        messages = con.execute(
            "SELECT id, data FROM session_message WHERE session_id = ? AND type = 'assistant' ORDER BY seq",
            (session_id,),
        ).fetchall()
        turns = []
        for message_id, data in messages:
            tokens = json.loads(data).get("tokens") or {}
            cache = tokens.get("cache") or {}
            total = int(tokens.get("input", 0) or 0) + int(cache.get("read", 0) or 0) + int(cache.get("write", 0) or 0)
            calls = []
            for (part,) in con.execute(
                "SELECT data FROM session_part WHERE message_id = ? AND type = 'tool' ORDER BY ordinal",
                (message_id,),
            ):
                p = json.loads(part)
                inp = (p.get("state") or {}).get("input") or {}
                calls.append((str(p.get("name", "")), json.dumps(inp, ensure_ascii=False)))
            turns.append((total, calls))
        return turns
    finally:
        con.close()


def analyze(source: str) -> dict:
    if source.startswith("opencode:"):
        harness, turns = "opencode", opencode_turns(source.removeprefix("opencode:"))
    else:
        harness, turns = jsonl_turns(pathlib.Path(source))
    metered = [(tok, calls) for tok, calls in turns if tok > 0 or calls]
    target = TARGETS[harness]
    first_read = None
    pre_read = None
    for idx, (_, calls) in enumerate(metered):
        if any(is_source_read(name, args) for name, args in calls):
            first_read = idx + 1
            earlier = [tok for tok, _ in metered[:idx] if tok > 0]
            pre_read = earlier[-1] if earlier else metered[idx][0]
            break
    first_metered = next((tok for tok, _ in metered if tok > 0), None)
    return {
        "source": source,
        "harness": harness,
        "target_lt": target,
        "turn1_input_tokens": first_metered,
        "pre_source_read_input_tokens": pre_read,
        "first_source_read_turn": first_read,
        "assistant_turns": len(metered),
        "pass": pre_read is not None and 0 < pre_read < target,
    }


results = [analyze(s) for s in sources]
if json_out:
    print(json.dumps(results, indent=2))
else:
    for r in results:
        status = "PASS" if r["pass"] else "FAIL"
        pre = r["pre_source_read_input_tokens"]
        pre_text = "no source read" if pre is None else str(pre)
        print(
            f"{status} [{r['harness']}] {r['source']}: pre_source_read={pre_text} "
            f"(turn1={r['turn1_input_tokens']}, first_read_turn={r['first_source_read_turn']}, target<{r['target_lt']})"
        )

sys.exit(0 if all(r["pass"] for r in results) else 1)
PY

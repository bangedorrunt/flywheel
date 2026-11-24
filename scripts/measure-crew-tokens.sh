#!/usr/bin/env bash
set -euo pipefail

# D1/R8 proof: measure crew context bloat from Claude Code or Pi session JSONL.
# Reports turn-1 input tokens and the input tokens on the turn immediately before
# the crew's first source-file read tool call, then checks against R8 targets
# (<25,000 for Claude, <15,000 for Pi).

usage() {
  cat <<'EOF'
Usage: measure-crew-tokens.sh [--harness auto|claude|pi] [--json] <session.jsonl>...
EOF
}

HARNESS="auto"
JSON_OUT=0
FILES=()

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
      FILES+=("$1")
      shift
      ;;
  esac
done

if [[ ${#FILES[@]} -eq 0 ]]; then
  usage >&2
  exit 2
fi

python3 - "$HARNESS" "$JSON_OUT" "${FILES[@]}" <<'PY'
import json
import pathlib
import sys

harness_opt = sys.argv[1]
json_out = sys.argv[2] == "1"
paths = [pathlib.Path(p) for p in sys.argv[3:]]

SOURCE_TOOLS = {"read", "Read", "grep", "Grep", "rg", "Glob", "glob", "find_files", "multi_grep", "bash", "Bash"}
SKIP_SUBSTRINGS = ("SKILL.md", "crew-card.md", ".agents/skills/", "toron next")


def detect_harness(path: pathlib.Path, entries: list[dict]) -> str:
    if harness_opt != "auto":
        return harness_opt
    s = str(path)
    if "/.pi/" in s or any(e.get("type") == "session" for e in entries[:5]):
        return "pi"
    return "claude"


def total_input_tokens(usage: dict) -> int:
    if not isinstance(usage, dict):
        return 0
    return int(usage.get("input_tokens", 0) or usage.get("input", 0) or 0) + int(
        usage.get("cache_read_input_tokens", 0) or usage.get("cacheRead", 0) or 0
    ) + int(
        usage.get("cache_creation_input_tokens", 0) or usage.get("cacheWrite", 0) or 0
    )


def extract_tool_calls(msg: dict) -> list[tuple[str, str]]:
    content = msg.get("content")
    if not isinstance(content, list):
        return []
    calls = []
    for block in content:
        if not isinstance(block, dict):
            continue
        btype = block.get("type", "")
        if btype in ("tool_use", "toolCall"):
            name = str(block.get("name", ""))
            inp = block.get("input") or block.get("arguments") or {}
            calls.append((name, json.dumps(inp, ensure_ascii=False)))
    return calls


def is_source_read(name: str, arg_json: str) -> bool:
    if name not in SOURCE_TOOLS:
        return False
    if any(skip in arg_json for skip in SKIP_SUBSTRINGS):
        return False
    if name in ("bash", "Bash"):
        # Only count shell commands inspecting source files, not `toron next`.
        return any(tok in arg_json for tok in ("rg ", "grep ", "cat ", "head ", "sed ", "src/", "crates/"))
    return True


def analyze(path: pathlib.Path) -> dict:
    entries = []
    with path.open("r", encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                entries.append(json.loads(line))
            except json.JSONDecodeError:
                continue

    harness = detect_harness(path, entries)
    target = 15000 if harness == "pi" else 25000

    assistant_turns = []
    for entry in entries:
        msg = entry.get("message") if isinstance(entry.get("message"), dict) else entry
        role = msg.get("role")
        if role != "assistant":
            continue
        usage = msg.get("usage") or entry.get("usage") or {}
        tok = total_input_tokens(usage)
        calls = extract_tool_calls(msg)
        assistant_turns.append((tok, calls))

    turn1_tokens = assistant_turns[0][0] if assistant_turns else 0
    pre_read_tokens = turn1_tokens
    first_source_turn = None
    for idx, (tok, calls) in enumerate(assistant_turns):
        if any(is_source_read(name, args) for name, args in calls):
            first_source_turn = idx + 1
            pre_read_tokens = assistant_turns[idx - 1][0] if idx > 0 else tok
            break

    passed = pre_read_tokens > 0 and pre_read_tokens < target
    return {
        "file": str(path),
        "harness": harness,
        "target_lt": target,
        "turn1_input_tokens": turn1_tokens,
        "pre_source_read_input_tokens": pre_read_tokens,
        "first_source_read_turn": first_source_turn,
        "assistant_turns": len(assistant_turns),
        "pass": passed,
    }


results = [analyze(p) for p in paths]
if json_out:
    print(json.dumps(results, indent=2))
else:
    for r in results:
        status = "PASS" if r["pass"] else "FAIL"
        print(
            f"{status} [{r['harness']}] {r['file']}: "
            f"pre_source_read={r['pre_source_read_input_tokens']} "
            f"(turn1={r['turn1_input_tokens']}, target<{r['target_lt']})"
        )

sys.exit(0 if all(r["pass"] for r in results) else 1)
PY

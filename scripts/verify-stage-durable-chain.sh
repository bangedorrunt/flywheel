#!/usr/bin/env bash
set -euo pipefail

server="${RESONATE_SERVER:-http://127.0.0.1:8001}"
workflow="${STAGE_WORKFLOW:-flyw0925083820d3}"
run_id="${STAGE_RUN_ID:-fw-composed-2026-09-25T08:38:20Z}"
root_promise="${STAGE_ROOT_PROMISE:-$run_id}"

stage_json="$({ flywheel workflow compose "stage witness" --parts stage:ce-execute --dry-run --json --project flywheel; })"
root_json="$(resonate promises get "$root_promise" --server "$server")"
child_json="$(resonate promises get "$root_promise.0" --server "$server")"
timer_json="$(resonate promises get "$root_promise.0.0" --server "$server")"
history_json="$(flywheel workflow history "$workflow")"

STAGE_JSON="$stage_json" ROOT_JSON="$root_json" CHILD_JSON="$child_json" TIMER_JSON="$timer_json" HISTORY_JSON="$history_json" RUN_ID="$run_id" python3 - <<'PY'
import json
import os

stage = json.loads(os.environ["STAGE_JSON"])
assert isinstance(stage.get("dry_run"), dict), stage
steps = stage["dry_run"]["steps"]
assert steps, stage
assert steps[0]["action"] == "dispatch_job", steps[0]
assert steps[0]["name"] == "coder-ce-execute-1", steps[0]

for label in ("ROOT_JSON", "CHILD_JSON", "TIMER_JSON"):
    promise = json.loads(os.environ[label])["promise"]
    assert promise["state"] == "resolved", (label, promise)

history = json.loads(os.environ["HISTORY_JSON"])
matching = [row for row in history["runs"] if row.get("run_id") == os.environ["RUN_ID"]]
assert matching, history
assert matching[0]["status"] == "succeeded", matching[0]

print(json.dumps({
    "stage_step": steps[0],
    "workflow": history["workflow"],
    "run_id": os.environ["RUN_ID"],
    "history_status": matching[0]["status"],
    "resonate": {
        "root": "resolved",
        "child": "resolved",
        "timer": "resolved",
    },
}, indent=2))
PY

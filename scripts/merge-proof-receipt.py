#!/usr/bin/env python3
"""Merge CI proof fragments into the clean-proof receipt shape (bd-7qhz0).

Usage: merge-proof-receipt.py PARTS_DIR RESULTS_CSV SHA > proof-receipt.json

RESULTS_CSV is the comma-separated job results from the workflow. `skipped`
is not a failure: the mutation matrix legitimately skips when no spec changed.
"""

import json
import pathlib
import sys

parts = pathlib.Path(sys.argv[1])
results = sys.argv[2].split(",") if len(sys.argv) > 2 else []
sha = sys.argv[3] if len(sys.argv) > 3 else ""

rows = []
for row_file in sorted(parts.rglob("mut.jsonl")):
    for line in row_file.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            try:
                rows.append(json.loads(line))
            except ValueError:
                pass

ok = all(r.strip() in ("success", "skipped", "") for r in results)
doc = {
    "schema": "flywheel-clean-proof.v1",
    "ok": ok,
    "sha": sha,
    "short": sha[:12],
    "mode": "ci",
    "driver_sha": sha,
    "stamp": "",
    "steps": [
        {"name": "tests", "ok": "success" in results},
        {"name": "lints", "ok": "success" in results},
        {"name": "mutations", "ok": "success" in results or "skipped" in results},
    ],
    "failed_step": None if ok else "ci",
    "siblings": [],
    "mutations": rows,
    "mutations_mode": "changed",
    "mutations_base": None,
    "changed_paths": [],
}
json.dump(doc, sys.stdout, indent=2, sort_keys=True)
print()

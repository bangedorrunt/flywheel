#!/usr/bin/env python3
"""Merge CI proof fragments into the clean-proof receipt shape (bd-7qhz0).

Usage: merge-proof-receipt.py PARTS_DIR RESULTS_CSV SHA [MUT_EXPECTED] > proof-receipt.json

RESULTS_CSV is the comma-separated job results in workflow `needs` order:
prepare,build,tests,lints,mutations. A job result other than `success` is
NEVER recorded as ok: a skipped test job after a failed build must not look
like a pass to ship land or proof status. The mutation matrix is the one
exception: when MUT_EXPECTED is false (no spec changed) its `skipped` is
expected, and the receipt says so explicitly.
"""

import json
import pathlib
import sys

parts = pathlib.Path(sys.argv[1])
names = ["prepare", "build", "tests", "lints", "mutations"]
raw = sys.argv[2].split(",") if len(sys.argv) > 2 else []
results = dict(zip(names, [r.strip() for r in raw]))
sha = sys.argv[3] if len(sys.argv) > 3 else ""
mut_expected = (sys.argv[4] if len(sys.argv) > 4 else "true").lower() == "true"

rows = []
for row_file in sorted(parts.rglob("mut.jsonl")):
    for line in row_file.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line:
            try:
                rows.append(json.loads(line))
            except ValueError:
                pass


def job_ok(name):
    conclusion = results.get(name, "missing")
    if conclusion == "success":
        return True
    if (
        name == "mutations"
        and conclusion == "skipped"
        and not mut_expected
        and results.get("build") == "success"
    ):
        return True
    return False


steps = [{"name": name, "ok": job_ok(name), "conclusion": results.get(name, "missing")}
         for name in names if name != "prepare"]
failed = next((name for name in names if not job_ok(name)), None)
doc = {
    "schema": "flywheel-clean-proof.v1",
    "ok": failed is None,
    "sha": sha,
    "short": sha[:12],
    "mode": "ci",
    "driver_sha": sha,
    "stamp": "",
    "steps": steps,
    "failed_step": failed,
    "siblings": [],
    "mutations": rows,
    "mutations_mode": "changed",
    "mutations_base": None,
    "changed_paths": [],
}
json.dump(doc, sys.stdout, indent=2, sort_keys=True)
print()

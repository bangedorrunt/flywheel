#!/usr/bin/env python3
"""Derive the CI mutation base from the newest GREEN Proof receipt (bd-n6kyt).

`discover` asks gh for completed successful Proof runs, checks each head is an
ancestor of HEAD, downloads its proof-receipt artifact and requires ok:true.
`select` applies the same pure rule to a prepared runs document, so the rule is
testable without gh, git or the network. Usage:

  ci-proof-base.py discover --current-run ID --fallback SHA --repo DIR
  ci-proof-base.py select --runs-json FILE --fallback SHA
"""

import argparse
import json
import os
import pathlib
import subprocess
import sys
import tempfile


def pick_base(runs, fallback):
    """Newest eligible run's sha, else the fallback pin.

    A run is eligible when it is not the current run, completed successfully,
    its head is an ancestor of HEAD and its receipt artifact says ok:true.
    """
    eligible = [
        r for r in runs
        if r.get("eligible") is True
        and r.get("headSha")
    ]
    if not eligible:
        return fallback
    newest = max(eligible, key=lambda r: (r.get("createdAt") or "", str(r.get("databaseId") or "")))
    return newest["headSha"]


def git_ok(repo, *args):
    return subprocess.run(
        ["git", "-C", repo, *args], capture_output=True
    ).returncode == 0


def discover(current, fallback, repo):
    env = dict(os.environ)
    out = subprocess.run(
        [
            "gh", "run", "list", "--workflow", "Proof", "--status", "success",
            "--limit", "20", "--json", "databaseId,headSha,status,conclusion,createdAt",
        ],
        cwd=repo, capture_output=True, text=True, env=env,
    )
    if out.returncode != 0:
        print(
            "ci-proof-base: gh run list failed; using fallback: "
            + out.stderr.strip(),
            file=sys.stderr,
        )
        print(fallback)
        return
    runs = json.loads(out.stdout)
    prepared = []
    with tempfile.TemporaryDirectory(prefix="ci-proof-base-") as tmp:
        for run in runs:
            if str(run.get("databaseId")) == str(current):
                continue
            sha = run.get("headSha") or ""
            if not sha or not git_ok(repo, "merge-base", "--is-ancestor", sha, "HEAD"):
                continue
            dest = pathlib.Path(tmp, str(run.get("databaseId")))
            dest.mkdir(exist_ok=True)
            dl = subprocess.run(
                ["gh", "run", "download", str(run.get("databaseId")), "-n",
                 "proof-receipt", "-D", str(dest)],
                cwd=repo, capture_output=True, text=True, env=env,
            )
            receipt = dest / "proof-receipt.json"
            if dl.returncode != 0 or not receipt.is_file():
                continue
            try:
                doc = json.loads(receipt.read_text(encoding="utf-8"))
            except ValueError:
                continue
            if doc.get("ok") is not True or doc.get("sha") != sha:
                continue
            run = dict(run)
            run["eligible"] = True
            prepared.append(run)
    print(pick_base(prepared, fallback))


def main():
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    d = sub.add_parser("discover")
    d.add_argument("--current-run", required=True)
    d.add_argument("--fallback", required=True)
    d.add_argument("--repo", default=".")
    s = sub.add_parser("select")
    s.add_argument("--runs-json", required=True)
    s.add_argument("--fallback", required=True)
    args = p.parse_args()
    if args.cmd == "discover":
        discover(args.current_run, args.fallback, args.repo)
    else:
        runs = json.loads(pathlib.Path(args.runs_json).read_text(encoding="utf-8"))
        print(pick_base(runs, args.fallback))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

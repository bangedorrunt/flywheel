#!/usr/bin/env python3
"""Content-addressed keys and the verdict cache for mutation specs (bd-jp9z1.2).

A clean proof re-runs every mutation spec even when nothing a spec reads has
changed: proof25 spent 41 minutes on 26 specs whose inputs were identical to a
prior red run. This remembers the RED instead, keyed by content.

The key covers everything a verdict depends on, read either from a checkout
(clean-proof's export) or from a git revision (ship land recomputes at the
proven sha):

  spec bytes · every target file named by its mutations · every source file
  that declares an expect= test · scripts/verify-mutations.sh (the harness) ·
  Cargo.lock · the driver revision · the sibling pins from proof.toml

A cache hit requires: key match, every declared mutation present with its
expected verdict, and a witness receipt that still exists and says ok:true.
Anything else is a miss and the spec runs.

Usage:
  mutation-cache.py key    --root DIR --spec SPEC --driver SHA
  mutation-cache.py key    --proven REV --repo GITDIR --spec SPEC --driver SHA
  mutation-cache.py lookup --dir CACHE --key KEY --expect FILE
  mutation-cache.py store  --dir CACHE --key KEY --spec SPEC --rows ROWS
                           --witness RECEIPT --witness-sha SHA
"""

import argparse
import hashlib
import json
import os
import pathlib
import subprocess
import sys
import tempfile

HEADER = b"flywheel-mutation-key-v1\n"
RED = "RED"
DEAD = "DEAD-CONFIRMED"


def die(msg: str) -> "None":
    print(f"mutation-cache: {msg}", file=sys.stderr)
    raise SystemExit(2)


def add(h: "hashlib._Hash", label: str, content: "bytes | None") -> None:
    h.update(label.encode())
    h.update(b"\0")
    if content is None:
        h.update(b"MISSING\0")
        return
    h.update(str(len(content)).encode())
    h.update(b"\0")
    h.update(content)
    h.update(b"\0")


def siblings_from_proof_toml(read) -> "list[tuple[str, str]]":
    raw = read("proof.toml")
    if raw is None:
        return []
    out = []
    cur = {}
    for line in raw.decode("utf-8", "replace").splitlines():
        s = line.strip()
        if not s or s.startswith("#"):
            continue
        if s.startswith("["):
            if cur.get("path"):
                out.append((cur["path"], cur.get("rev", "")))
            cur = {}
            continue
        if "=" in s:
            key, _, value = s.partition("=")
            cur[key.strip()] = value.strip().strip('"')
    if cur.get("path"):
        out.append((cur["path"], cur.get("rev", "")))
    return out


def spec_listing(root: str, listing_args: "list[str]") -> "tuple[list[str], list[tuple[str, str, bool]]]":
    """Targets and expects, through the harness's own loader (bash expansion)."""
    harness = os.path.join(root, "scripts/verify-mutations.sh")
    if not os.path.isfile(harness):
        die(f"no harness at {harness}")
    listed = subprocess.run(
        ["bash", harness, *listing_args, "--list"],
        cwd=root,
        capture_output=True,
        text=True,
    )
    expects = subprocess.run(
        ["bash", harness, *listing_args, "--list-expect"],
        cwd=root,
        capture_output=True,
        text=True,
    )
    if listed.returncode != 0:
        die(f"the harness cannot list the spec: {listed.stderr.strip()}")
    targets = sorted(
        {line.split("\t")[1] for line in listed.stdout.splitlines() if line.strip()}
    )
    tests = []
    if expects.returncode == 0:
        for line in expects.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) >= 3:
                tests.append((parts[0], parts[1], parts[2] == "1"))
    return targets, tests


def key_for(read, root: str, spec: str, driver: str, declaring_of) -> str:
    listing_args = [spec]
    targets, tests = spec_listing(root, listing_args)
    h = hashlib.sha256()
    h.update(HEADER)
    add(h, "spec:" + spec, read(spec))
    for path in targets:
        add(h, "target:" + path, read(path))
    names = sorted({name for (_id, name, _green) in tests if name and name != "?"})
    declaring = set()
    missing = []
    for name in names:
        found = declaring_of(name)
        if not found:
            missing.append(name)
        declaring.update(found)
    for path in sorted(declaring):
        add(h, "testfile:" + path, read(path))
    add(h, "harness", read("scripts/verify-mutations.sh"))
    add(h, "lock", read("Cargo.lock"))
    add(h, "driver", driver.encode())
    for name, rev in siblings_from_proof_toml(read):
        add(h, "sibling:" + name, rev.encode())
    for name in missing:
        add(h, "test-missing", name.encode())
    return h.hexdigest()


def source_files(root: str) -> "list[str]":
    out = []
    for base, dirs, files in os.walk(root):
        dirs[:] = [d for d in dirs if d not in {".git", "target", ".beads", "node_modules"}]
        for name in files:
            if name.endswith(".rs") or name.endswith(".sh") or name.endswith(".py"):
                out.append(os.path.relpath(os.path.join(base, name), root))
    return sorted(out)


def read_root(root: str):
    def read(rel: str) -> "bytes | None":
        try:
            return pathlib.Path(root, rel).read_bytes()
        except OSError:
            return None

    return read


def computed_key(args) -> str:
    if args.proven:
        repo = args.repo or "."

        def git_show(rel: str) -> "bytes | None":
            out = subprocess.run(
                ["git", "-C", repo, "show", f"{args.proven}:{rel}"],
                capture_output=True,
            )
            return out.stdout if out.returncode == 0 else None

        def declaring(name: str) -> "list[str]":
            out = subprocess.run(
                ["git", "-C", repo, "grep", "-l", "-F", f"fn {name}(", args.proven, "--", "*.rs"],
                capture_output=True,
                text=True,
            )
            found = []
            for line in out.stdout.splitlines():
                parts = line.split(":", 1)
                if len(parts) == 2:
                    found.append(parts[1])
            return found

        with tempfile.TemporaryDirectory() as tmp:
            spec_bytes = git_show(args.spec)
            if spec_bytes is None:
                die(f"{args.spec} does not exist at {args.proven}")
            dest = pathlib.Path(tmp, args.spec)
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes(spec_bytes)
            harness = git_show("scripts/verify-mutations.sh")
            if harness is None:
                die(f"scripts/verify-mutations.sh does not exist at {args.proven}")
            os.makedirs(os.path.join(tmp, "scripts"), exist_ok=True)
            pathlib.Path(tmp, "scripts/verify-mutations.sh").write_bytes(harness)
            return key_for(git_show, tmp, args.spec, args.driver, declaring)
    if not args.root:
        die("one of --root or --proven is required")
    cached_files = source_files(args.root)

    def declaring(name: str) -> "list[str]":
        needle = f"fn {name}(".encode()
        out = []
        for rel in cached_files:
            if not rel.endswith(".rs"):
                continue
            try:
                body = pathlib.Path(args.root, rel).read_bytes()
            except OSError:
                continue
            if needle in body:
                out.append(rel)
        return out

    return key_for(read_root(args.root), args.root, args.spec, args.driver, declaring)


def expect_rows(path: str) -> "list[tuple[str, str, bool]]":
    rows = []
    for line in pathlib.Path(path).read_text().splitlines():
        parts = line.split("\t")
        if len(parts) >= 3:
            rows.append((parts[0], parts[1], parts[2] == "1"))
    return rows


def lookup(args) -> int:
    entry = pathlib.Path(args.dir, args.key + ".json")
    if not entry.is_file():
        print(f"mutation-cache: no entry for {args.key}", file=sys.stderr)
        return 3
    try:
        doc = json.loads(entry.read_text())
    except ValueError:
        print("mutation-cache: unreadable entry", file=sys.stderr)
        return 3
    witness = doc.get("witness") or {}
    receipt = witness.get("receipt") or ""
    # One gate: an absent, unreadable or malformed witness is a miss. A separate
    # existence check would be a second spelling of the same answer.
    try:
        proof = json.loads(pathlib.Path(receipt).read_text())
    except (OSError, ValueError):
        print(f"mutation-cache: witness receipt {receipt or '<none>'} is unusable", file=sys.stderr)
        return 3
    if proof.get("ok") is not True:
        print("mutation-cache: witness receipt is not green", file=sys.stderr)
        return 3
    cached = {row.get("id"): row for row in doc.get("rows", [])}
    for mid, expect_name, green in expect_rows(args.expect):
        row = cached.get(mid)
        if row is None or row.get("expect_test") != expect_name:
            print(f"mutation-cache: {mid} is not in the entry", file=sys.stderr)
            return 3
        if row.get("verdict") != (DEAD if green else RED):
            print(f"mutation-cache: {mid} verdict is {row.get('verdict')}", file=sys.stderr)
            return 3
    for row in doc.get("rows", []):
        row = dict(row)
        row["cached"] = receipt
        row["key"] = args.key
        print(json.dumps(row, sort_keys=True))
    return 0


def store(args) -> int:
    pathlib.Path(args.dir).mkdir(parents=True, exist_ok=True)
    rows = []
    for line in pathlib.Path(args.rows).read_text().splitlines():
        line = line.strip()
        if line:
            rows.append(json.loads(line))
    doc = {
        "schema": "flywheel-mutation-cache.v1",
        "key": args.key,
        "spec": args.spec,
        "witness": {"receipt": args.witness, "sha": args.witness_sha},
        "rows": rows,
    }
    target = pathlib.Path(args.dir, args.key + ".json")
    tmp = target.with_suffix(".tmp")
    tmp.write_text(json.dumps(doc, indent=2, sort_keys=True) + "\n")
    tmp.replace(target)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)
    k = sub.add_parser("key")
    k.add_argument("--root")
    k.add_argument("--proven")
    k.add_argument("--repo")
    k.add_argument("--spec", required=True)
    k.add_argument("--driver", default="")
    k.set_defaults(func=lambda a: print(computed_key(a)) or 0)
    lk = sub.add_parser("lookup")
    lk.add_argument("--dir", required=True)
    lk.add_argument("--key", required=True)
    lk.add_argument("--expect", required=True)
    lk.set_defaults(func=lookup)
    st = sub.add_parser("store")
    st.add_argument("--dir", required=True)
    st.add_argument("--key", required=True)
    st.add_argument("--spec", required=True)
    st.add_argument("--rows", required=True)
    st.add_argument("--witness", required=True)
    st.add_argument("--witness-sha", required=True)
    st.set_defaults(func=store)
    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())

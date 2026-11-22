#!/usr/bin/env bash
# Verify-coverage gate (bd-bd-ksbtp, epic bd-jp9z1).
#
# Reads docs/verification/behaviors.toml and proves the ledger cannot rot:
#   1. every referenced test name exists in the source tree;
#   2. every mutation names a file that exists;
#   3. every behavior has >= 1 test and >= 1 mutation;
#   4. a concurrent (race-prone) behavior names a formal model that exists;
#   5. every status is known (proven | pending);
#   6. module coverage: every src module is named by some behavior's owner.
#
# Exit 0 when the ledger is consistent. The module check is advisory unless
# --strict is passed, so the ledger can grow while the gate is already wired:
# an unledgered module is REPORTED, never silently ignored, and --strict turns
# the report into a failure once the seed is complete.
#
# Usage: scripts/verify-coverage.sh [--strict] [--json] [--repo <dir>]
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
STRICT=0
JSON=0
while [ "$#" -gt 0 ]; do
  case "$1" in
    --strict) STRICT=1 ;;
    --json) JSON=1 ;;
    --repo)
      [ "$#" -ge 2 ] || { echo "verify-coverage: --repo needs a directory" >&2; exit 2; }
      REPO="$2"; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "verify-coverage: unknown argument $1" >&2; exit 2 ;;
  esac
  shift
done
cd "$REPO"

LEDGER="docs/verification/behaviors.toml"
[ -f "$LEDGER" ] || { echo "verify-coverage: missing $LEDGER" >&2; exit 1; }

exec python3 - "$LEDGER" "$STRICT" "$JSON" <<'PY'
import json
import pathlib
import sys
import tomllib

ledger = pathlib.Path(sys.argv[1])
strict = sys.argv[2] == "1"
as_json = sys.argv[3] == "1"
root = pathlib.Path.cwd()

data = tomllib.loads(ledger.read_text())
behaviors = data.get("behavior", [])
errors: list[str] = []
warnings: list[str] = []
owners: set[str] = set()

sources: dict[pathlib.Path, str] = {}
for path in root.rglob("*.rs"):
    if "target" in path.parts:
        continue
    sources[path] = path.read_text(errors="ignore")

def test_exists(spec: str) -> bool:
    """The named fn must exist AND carry a test attribute (a helper is not a test)."""
    file, _, name = spec.partition("::")
    path = root / file
    if path not in sources:
        return False
    lines = sources[path].splitlines()
    for i, line in enumerate(lines):
        if not (f"fn {name}(" in line or f"fn {name}<" in line):
            continue
        for back in range(1, 5):
            if i - back < 0:
                break
            prev = lines[i - back].strip()
            if prev.startswith("#[test]") or prev.startswith("#[tokio::test"):
                return True
            if prev.startswith("fn ") or prev.startswith("pub fn "):
                break
        return False
    return False


def mutation_exists(spec: str) -> bool:
    """'scripts/mutations/<area>.spec.sh#<name>' must name a declared mutation."""
    file, _, name = spec.partition("#")
    if not name:
        return False
    path = root / file
    if not path.is_file():
        return False
    needle = f'mutation "{name} '
    for line in path.read_text(errors="ignore").splitlines():
        if line.startswith(needle):
            return True
    return False

def formal_exists(spec: str) -> bool:
    """A formal entry is a TLA model file plus its .cfg, or a workflow-verify chart."""
    parts = spec.split("::")
    if len(parts) == 2:
        model = root / parts[0]
        cfg = root / parts[1]
        return model.is_file() and cfg.is_file()
    file = root / parts[0]
    return file.is_file()

for b in behaviors:
    bid = b.get("id")
    owner = b.get("owner")
    status = b.get("status")
    tests = b.get("tests", [])
    mutations = b.get("mutations", [])
    if not bid:
        errors.append("a behavior has no id")
        continue
    if status not in {"proven", "pending"}:
        errors.append(f"{bid}: unknown status {status!r} (proven|pending)")
    if not owner or not (root / owner).exists():
        errors.append(f"{bid}: owner module {owner!r} does not exist")
    else:
        owners.add(owner)
    if not tests:
        errors.append(f"{bid}: no test")
    for spec in tests:
        if not test_exists(spec):
            errors.append(f"{bid}: test not found: {spec}")
    if not mutations:
        if status == "pending" and b.get("missing"):
            warnings.append(f"{bid}: pending — {b.get('missing')}")
        else:
            errors.append(
                f"{bid}: no mutation (a check never seen failing proves nothing); "
                "a pending entry must name what is 'missing'"
            )
    for m in mutations:
        if not m.strip():
            errors.append(f"{bid}: empty mutation")
        elif not mutation_exists(m):
            errors.append(
                f"{bid}: mutation not declared: {m} "
                "(expected scripts/mutations/<area>.spec.sh#<name>)"
            )
    if b.get("concurrent"):
        formal = b.get("formal")
        if not formal:
            errors.append(f"{bid}: concurrent behavior without a formal model")
        elif not formal_exists(formal):
            errors.append(f"{bid}: formal model missing: {formal}")

# Module coverage: every top-level src module and every src/<dir>/mod.rs.
modules = sorted(
    {p.name for p in (root / "src").glob("*.rs") if p.name != "main.rs"}
    | {"src/" + p.parent.name + "/" for p in (root / "src").glob("*/mod.rs")}
)
ledger_owners = {o.split("/")[-1] for o in owners}
unledgered = [m for m in modules if m not in ledger_owners and m.removesuffix(".rs") not in ledger_owners]

proven = sum(1 for b in behaviors if b.get("status") == "proven")
pending = sum(1 for b in behaviors if b.get("status") == "pending")

body = {
    "schema": "flywheel-verify-coverage.v1",
    "behaviors": len(behaviors),
    "proven": proven,
    "pending": pending,
    "unledgered_modules": unledgered,
    "strict": strict,
    "errors": errors,
    "ok": not errors and (not strict or not unledgered),
}

if as_json:
    print(json.dumps(body, indent=2))
else:
    print(
        f"verify-coverage: {len(behaviors)} behavior(s) — {proven} proven, {pending} pending; "
        f"{len(unledgered)} module(s) without a ledger entry"
    )
    for w in warnings:
        print(f"  warn: {w}")
    for e in errors:
        print(f"  ERROR: {e}", file=sys.stderr)
    if unledgered:
        stream = sys.stderr if strict else sys.stdout
        print(
            f"  {'ERROR' if strict else 'note'}: unledgered module(s): "
            + ", ".join(unledgered),
            file=stream,
        )

sys.exit(0 if body["ok"] else 1)
PY

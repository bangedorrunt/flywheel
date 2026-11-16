#!/usr/bin/env bash
# Lint mutation needles before anything expensive runs (bd-jp9z1.3).
#
# Every `find=` in every spec must occur EXACTLY ONCE in its target file at
# this tree. Zero occurrences is the NEEDLE-MISS that used to surface 40
# minutes into a proof; two or more means perl substitutes the first and the
# spec silently stops covering the other site.
#
# Pure text work, no build: well under a second for the whole directory, so
# clean-proof runs it first and a crew can run it before committing a spec.
#
# Usage: scripts/verify-needles.sh [--repo <dir>] [spec ...]
#        (no spec runs every scripts/mutations/*.spec.sh)
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SPECS=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --repo)
      [ "$#" -ge 2 ] || { echo "verify-needles: --repo needs a directory" >&2; exit 2; }
      REPO="$2"
      shift
      ;;
    -h | --help)
      sed -n '2,14p' "$0"
      exit 0
      ;;
    -*)
      echo "verify-needles: unknown flag $1" >&2
      exit 2
      ;;
    *) SPECS+=("$1") ;;
  esac
  shift
done

cd "$REPO"
if [ "${#SPECS[@]}" -eq 0 ]; then
  SPECS=(scripts/mutations/*.spec.sh)
fi

RECORDS="$(mktemp "${TMPDIR:-/tmp}/verify-needles.XXXXXX")"
trap 'rm -f "$RECORDS"' EXIT

spec_count=0
for spec in "${SPECS[@]}"; do
  [ -f "$spec" ] || { echo "verify-needles: no such spec: $spec" >&2; exit 2; }
  spec_count=$((spec_count + 1))
  (
    # The spec's own `mutation` calls become records: spec, id, target,
    # base64(find). Sourced in a subshell so one spec's variables cannot leak
    # into the next.
    mutation() {
      local label="$1"
      shift
      local file="" find=""
      while [ "$#" -gt 0 ]; do
        case "$1" in
          file=*) file="${1#file=}" ;;
          find=*) find="${1#find=}" ;;
        esac
        shift
      done
      printf '%s\t%s\t%s\t%s\n' \
        "$spec" "${label%%  *}" "$file" \
        "$(printf '%s' "$find" | base64 | tr -d '\n')"
    }
    . "$spec"
    # bd-6setu.18: TEST_FILTER is a substring filter, and a filter that names
    # nothing runs zero tests — the spec then reads VACUOUS instead of RED, so
    # its mutation is silently lost. Record every filter name for the check.
    if [ -n "${TEST_FILTER+set}" ]; then
      for name in "${TEST_FILTER[@]}"; do
        [ -n "$name" ] || continue
        printf 'FILTER\t%s\t%s\n' "$spec" "$name"
      done
    fi
  ) >>"$RECORDS"
done

exec python3 - "$RECORDS" "$spec_count" <<'PY'
import base64
import pathlib
import sys

records = pathlib.Path(sys.argv[1])
spec_count = sys.argv[2]
bad = 0
rows = 0
dead_filters = []
raw = records.read_text(errors="ignore").splitlines()


def test_name_index() -> list[str]:
    """Test fn names and module names across the bin and the workspace crates.

    A filter is a SUBSTRING match over the names libtest prints, and specs name
    them in several forms (`fn`, `module::tests::fn`, `cmd::swarm::tests::fn`,
    or a bare prefix like `bd20_`). Resolving the exact nested path would need a
    Rust parser; resolving "does this name still exist anywhere" does not, and
    that is the failure this gate exists to catch: a test DELETED while a spec
    kept naming it. So a filter is live when every `::` segment matches some
    test fn or module name.

    Limit, stated on purpose: a filter naming a real fn under the WRONG module
    passes here and fails loudly at the test runner instead.
    """
    import re

    names: list[str] = []
    # A spec may target the bin's src/tests OR a workspace crate: the doorbell,
    # awareness and wake specs all point at crates/*.
    roots = [pathlib.Path("src"), pathlib.Path("tests")]
    for crate in sorted(pathlib.Path("crates").glob("*/")):
        roots.extend([crate / "src", crate / "tests"])
    for root in roots:
        if not root.is_dir():
            continue
        for path in root.rglob("*.rs"):
            try:
                text = path.read_text(errors="ignore")
            except OSError:
                continue
            stem = path.with_suffix("").name
            if stem != "mod":
                names.append(stem)
            names.extend(re.findall(r"\bmod\s+([A-Za-z0-9_]+)", text))
            lines = text.splitlines()
            for i, line in enumerate(lines):
                if "#[test]" not in line and "#[tokio::test" not in line:
                    continue
                for follow in lines[i + 1 : i + 5]:
                    m = re.search(r"\bfn\s+([A-Za-z0-9_]+)\s*\(", follow)
                    if m:
                        names.append(m.group(1))
                        break
    return names


test_names = None
for line in raw:
    if not line.strip():
        continue
    if line.startswith("FILTER\t"):
        _, spec, name = line.split("\t", 2)
        if test_names is None:
            test_names = test_name_index()
        segments = [seg for seg in name.split("::") if seg]
        live = any(seg in candidate for seg in segments for candidate in test_names)
        if not live:
            dead_filters.append((spec, name))
        continue
    spec, mid, file, encoded = line.split("\t", 3)
    rows += 1
    find = base64.b64decode(encoded).decode("utf-8", "replace")
    if not find:
        print(f"{spec}#{mid}: empty find=", file=sys.stderr)
        bad += 1
        continue
    target = pathlib.Path(file)
    if not target.is_file():
        print(f"{spec}#{mid}: target does not exist: {file}", file=sys.stderr)
        bad += 1
        continue
    occurrences = target.read_text(errors="ignore").count(find)
    if occurrences != 1:
        print(
            f"{spec}#{mid}: find= occurs {occurrences} time(s) in {file} "
            "(want exactly 1)",
            file=sys.stderr,
        )
        bad += 1

for spec, name in dead_filters:
    print(
        f"{spec}: TEST_FILTER names {name!r}, which matches no #[test] fn — "
        "its mutants prove nothing (bd-6setu.18)",
        file=sys.stderr,
    )
bad += len(dead_filters)
if bad:
    print(
        f"verify-needles: {bad} bad needle(s) across {rows} mutation(s) — "
        "fix the find= text before proving anything",
        file=sys.stderr,
    )
    sys.exit(1)
print(f"verify-needles: {spec_count} spec(s), {rows} mutation(s), 0 bad")
PY

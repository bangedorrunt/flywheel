#!/usr/bin/env bash
# Helpers shared by scripts/clean-proof.sh and scripts/proof-selftest.sh
# (bd-v6j35). Sourced, never executed. Every function is side-effect free
# unless its name says otherwise.

# proof_siblings <repo-root>
# Print one `path<TAB>rev` line per [[sibling]] in <repo-root>/proof.toml.
# No file, or no [[sibling]] block: print nothing (single-repo project).
proof_siblings() {
  local root="$1" cfg="$1/proof.toml"
  [ -f "$cfg" ] || return 0
  awk '
    /^\[\[sibling\]\]/ { if (path != "") print path "\t" rev; path=""; rev=""; in_block=1; next }
    /^\[/ { if (path != "") print path "\t" rev; path=""; rev=""; in_block=0; next }
    in_block && /^[[:space:]]*path[[:space:]]*=/ { v=$0; sub(/^[^=]*=[[:space:]]*"/,"",v); sub(/".*/,"",v); path=v }
    in_block && /^[[:space:]]*rev[[:space:]]*=/ { v=$0; sub(/^[^=]*=[[:space:]]*"/,"",v); sub(/".*/,"",v); rev=v }
    END { if (path != "") print path "\t" rev }
  ' "$cfg"
}

# proof_mutation_base <repo-root>
# Print the `[mutations] base` revision from proof.toml, or nothing. It is the
# last revision a FULL mutation run covered, so a landing only re-proves the
# specs whose target files changed since then. Unset means "no base": every
# spec runs (the safe default, and what a fresh clone does).
proof_mutation_base() {
  local root="$1" cfg="$1/proof.toml"
  [ -f "$cfg" ] || return 0
  awk '
    /^\[/ { in_block = ($0 ~ /^\[mutations\]/) ; next }
    in_block && /^[[:space:]]*base[[:space:]]*=/ {
      v=$0; sub(/^[^=]*=[[:space:]]*"/,"",v); sub(/".*/,"",v); print v; exit
    }
  ' "$cfg"
}

# proof_archive <source-repo> <rev> <dest-dir>
# Extract one revision from git objects into <dest-dir> (created). Fails loud
# if the rev is not in that repo.
proof_archive() {
  local repo="$1" rev="$2" dest="$3"
  git -C "$repo" cat-file -e "${rev}^{commit}" 2>/dev/null \
    || { echo "proof: rev $rev not found in $repo" >&2; return 1; }
  mkdir -p "$dest"
  git -C "$repo" archive "$rev" | tar -x -C "$dest"
}

# proof_apply_mutation <file> <find> <replace>
# Replace the FIRST literal <find> in <file> with <replace>, keeping a backup
# beside it. Prints the backup path. Fails when <find> is absent — a mutation
# that never applied is not a mutation.
proof_apply_mutation() {
  local file="$1" find="$2" replace="$3" backup status
  backup="$(mktemp "${TMPDIR:-/tmp}/proof-mutation.XXXXXX")" || return 1
  cp "$file" "$backup"
  MUT_FILE="$file" MUT_FIND="$find" MUT_REPLACE="$replace" python3 - <<'PY'
import os, sys
path, find, repl = os.environ["MUT_FILE"], os.environ["MUT_FIND"], os.environ["MUT_REPLACE"]
with open(path, encoding="utf-8") as fh:
    text = fh.read()
if find not in text:
    sys.exit("mutation find text is absent from " + path)
with open(path, "w", encoding="utf-8") as fh:
    fh.write(text.replace(find, repl, 1))
PY
  status=$?
  if [ "$status" -ne 0 ]; then
    rm -f "$backup"
    return 1
  fi
  printf '%s\n' "$backup"
}

# proof_restore_mutation <file> <backup>
proof_restore_mutation() {
  cp "$2" "$1"
  rm -f "$2"
}

# proof_mutation_verdict <actual> <expect>
# 0 when the observed RED/GREEN matches the expectation, 1 otherwise.
proof_mutation_verdict() {
  [ "$1" = "$2" ]
}

# proof_write_receipt <receipt> <ok:0|1> <sha> <short> <stamp> <mode> <tmpdir>
#                     <failed-step> <steps> <statuses> <siblings> <mutations-file>
# Write the machine-readable proof receipt (flywheel-clean-proof.v1). An empty
# mutations-file states `"mutations": "none"` rather than failing.
proof_write_receipt() {
  PROOF_RECEIPT="$1" PROOF_OK="$2" PROOF_SHA="$3" PROOF_SHORT="$4" \
  PROOF_STAMP="$5" PROOF_MODE="$6" PROOF_TMP="$7" PROOF_FAILED="$8" \
  PROOF_STEPS="$9" PROOF_STATUSES="${10}" PROOF_SIBLINGS="${11}" \
  PROOF_MUTATIONS_FILE="${12}" python3 - <<'PY'
import json, os

def lines(key):
    return [x for x in os.environ.get(key, "").splitlines() if x]

steps = [{"name": n, "status": int(s)}
         for n, s in zip(lines("PROOF_STEPS"), lines("PROOF_STATUSES"))]
siblings = []
for row in lines("PROOF_SIBLINGS"):
    path, _, rev = row.partition("=")
    siblings.append({"path": path, "rev": rev})
mid = os.environ.get("PROOF_MUTATIONS_FILE", "")
mutations = []
if mid and os.path.exists(mid):
    with open(mid, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                mutations.append(json.loads(line))
            except ValueError:
                mutations.append(line)

# How the mutation set was chosen, so a landing can tell "nothing to re-prove"
# (changed, base known, no spec target touched) from "nobody asked for a proof"
# (none) and from "a full run covered everything". `changed_paths` is the diff
# the selection was made from; the landing re-derives the target match itself
# rather than trusting this list.
mutations_mode = os.environ.get("PROOF_MUTATIONS_MODE") or None
mutations_base = os.environ.get("PROOF_MUTATIONS_BASE") or None
changed_paths = lines("PROOF_CHANGED_PATHS")
receipt = {
    "schema": "flywheel-clean-proof.v1",
    "ok": os.environ["PROOF_OK"] == "0",
    "sha": os.environ["PROOF_SHA"],
    "short": os.environ["PROOF_SHORT"],
    "mode": os.environ["PROOF_MODE"],
    "stamp": os.environ["PROOF_STAMP"],
    "steps": steps,
    "failed_step": os.environ.get("PROOF_FAILED") or None,
    "siblings": siblings,
    "mutations": mutations,
    "mutations_mode": mutations_mode,
    "mutations_base": mutations_base,
    "changed_paths": changed_paths,
    "logs": os.environ.get("PROOF_TMP"),
}
path = os.environ["PROOF_RECEIPT"]
with open(path, "w", encoding="utf-8") as fh:
    json.dump(receipt, fh, indent=2, sort_keys=True)
    fh.write("\n")
print("clean-proof: receipt " + path)
PY
}

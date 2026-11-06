#!/bin/bash
# check-docs-links.sh: every in-repo relative markdown link in README.md + docs/ must resolve.
# `../` escapes (../../toron, ../../beads) are the cross-repo convention and are
# SKIPPED here — they resolve on the docs site, never on a lone disk checkout.
cd "$(dirname "$0")/.." || exit 1
miss=0
for f in README.md $(find docs -name '*.md'); do
  while IFS= read -r t; do
    [[ "$t" == http* || "$t" == mailto:* || "$t" == "#"* || "$t" == ../* || -z "$t" ]] && continue
    t="${t%%#*}"
    [ -f "$(dirname "$f")/$t" ] || { echo "MISS: $f -> $t"; miss=1; }
  done < <(rg -o --no-filename '\]\(.+?\)' "$f" | sed 's/^](//;s/)$//')
done
exit $miss

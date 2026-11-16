#!/usr/bin/env bash
# Install the repo's hooks path (bd-4z7mb). One command, no per-clone setup:
#   bash scripts/hooks/install.sh
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
git -C "$root" config core.hooksPath scripts/hooks
echo "hooks path: $(git -C "$root" config core.hooksPath)"

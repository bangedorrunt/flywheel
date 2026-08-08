#!/usr/bin/env bash
# Clippy zero-warnings gate (bd-review-hardening-build-gates-zbz.1).
# Runs the exact discipline command; exits non-zero on any lint.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo clippy --all-targets --all-features --locked -- -D warnings

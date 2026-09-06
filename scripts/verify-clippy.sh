#!/usr/bin/env bash
# Rust quality gates (bd-review-hardening-build-gates-zbz.1, bd-14ea).
# Mirrors beads scripts/ci-local.sh + .github/workflows/ci.yml steps.
# Runs the exact discipline commands; exits non-zero on any failure.
#
# Deviation from beads: no `--no-default-features` clippy gate — flywheel
# does not build without default features (E0425/E0428/E0433), so that gate
# would fail loud on main. Fix the feature-gating before adding it.
set -euo pipefail
cd "$(dirname "$0")/.."
echo "-> fmt"
cargo fmt --all -- --check
echo "-> clippy (all features)"
# --locked: the committed lock is refreshed with the pinned toolchain (toron
# path bumps + rmcp re-resolve ride this commit); a lock refresh is otherwise
# a separate bead. No --offline: cold caches must be able to fetch.
cargo clippy --all-targets --all-features --locked -- -D warnings
echo "-> check (all targets)"
cargo check --all-targets --all-features --locked

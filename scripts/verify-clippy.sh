#!/usr/bin/env bash
# Rust quality gates (bd-review-hardening-build-gates-zbz.1, bd-14ea).
# Mirrors beads scripts/ci-local.sh + .github/workflows/ci.yml steps.
# Runs the exact discipline commands; exits non-zero on any failure.
#
# bd-iaza: the `--no-default-features` check gate is live — the serve-gated
# DaemonCmd/cmd_serve/cmd_daemon sites are cfg-gated, so the minimal build
# compiles. Keep it green when adding feature-gated code.
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
echo "-> check (no default features)"
cargo check --locked --no-default-features
echo "-> check (lean backend-only)"
cargo check --locked --no-default-features --features backend-herdr,mail-toron

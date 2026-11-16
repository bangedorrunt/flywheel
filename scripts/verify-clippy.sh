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
# bd-hj36: scope every gate to THIS workspace's packages. Bare --all also
# reports diffs in the sibling toron tree (live path-dep), red/greening this
# repo on another session's dirt. When adding a workspace member, add its -p
# here too.
PKGS="-p flywheel -p doorbell-core -p doorbell-rt -p doorbell-cli"
# bd-0uzrq.4: EVERY leg denies warnings, and `cargo check` does not take
# `-- -D warnings` (that is clippy-driver syntax), so the discipline rides
# RUSTFLAGS for the whole script. A bare `check` compiled a dead binding
# (crew.rs:919) and reported success while the -D warnings build caught it.
export RUSTFLAGS="${RUSTFLAGS:-} -D warnings"
echo "-> fmt"
cargo fmt $PKGS -- --check
echo "-> clippy (all features)"
# --locked: the committed lock is refreshed with the pinned toolchain (toron
# path bumps + rmcp re-resolve ride this commit); a lock refresh is otherwise
# a separate bead. No --offline: cold caches must be able to fetch.
cargo clippy --all-targets --all-features --locked $PKGS -- -D warnings
# bd-0uzrq.4: EVERY leg denies warnings. A plain `check` compiled the crate and
# reported success while a dead binding (crew.rs:919) sat in it — the
# -D warnings build caught what these legs let through.
echo "-> check (all targets)"
cargo check --all-targets --all-features --locked $PKGS
echo "-> check (no default features)"
cargo check --locked --no-default-features -p flywheel
echo "-> check (lean backend-only)"
cargo check --locked --no-default-features --features backend-hc,mail-toron -p flywheel

#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-8n6j8: the built-in lint gate must not read the operator's config.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(every_builtin_passes_full_lint_gate)

mutation "H1  the gate test reads config_dir() again" \
    file=src/cmd/workflow.rs \
    expect=every_builtin_passes_full_lint_gate \
    find='            lint_flywheel_tier_in(name, wf, None, Some(&config_root))' \
    replace='            lint_flywheel_tier(name, wf, None)'

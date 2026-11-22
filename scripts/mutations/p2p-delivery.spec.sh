# Sourced by scripts/verify-mutations.sh.
# A busy peer target's directive is DEFERRED through the one handoff, never
# dropped (bd-ncgyf).

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(cmd::peer::tests::a_busy_target_defers_through_the_handoff)

mutation "P1  a busy target's p2p wake is dropped instead of deferred" \
    file=src/cmd/peer.rs \
    expect=a_busy_target_defers_through_the_handoff \
    find=$'    crate::doorbell::append_deferred_wake_with_wm(\n        &crate::config::state_dir(),\n        &directive.project,\n        pin,\n        line,\n        Some(crate::doorbell::now_micros()),\n        Some(directive.verb),\n    )\n    .map_err(|e| anyhow::anyhow!("record the deferred p2p wake: {e}"))?;\n' \
    replace=$'    let _ = (pin, line);\n'

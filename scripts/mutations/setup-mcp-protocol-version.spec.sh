#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-958by: pi-mcp-adapter defaults to `legacy` unless setup-mcp pins the
# protocol version. Dropping that line must turn the witness RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(setup_mcp_pi_entry_pins_protocol_version)

mutation "P1  setup-mcp stops pinning the pi protocol version" \
    file=src/main.rs \
    expect=setup_mcp_pi_entry_pins_protocol_version \
    find='            merged.insert("protocolVersion".into(), serde_json::json!("2026-07-28"));' \
    replace='            merged.insert("protocolVersion".into(), serde_json::json!("legacy"));'

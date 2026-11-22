#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-958by: pi 1.6's built-in MCP client ignores pi-mcp-adapter keys. Writing
# protocolVersion (or directTools) into mcp.json must turn the witness RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(setup_mcp_pi_entry_is_adapter_free)

mutation "P1  setup-mcp reintroduces an adapter-only protocolVersion" \
    file=src/main.rs \
    expect=setup_mcp_pi_entry_is_adapter_free \
    find='    let want = serde_json::json!({"url":url});' \
    replace='    let want = serde_json::json!({"url":url,"protocolVersion":"2026-07-28"});'

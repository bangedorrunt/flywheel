#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# The strict fake server enforces toron's Streamable HTTP rules; dropping
# either required wire field must turn the end-to-end test RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(real_events_server_accepts_the_client_wire_and_serves_a_page)

mutation "S1  the client drops the mcp-method header (-32020)" \
    file=src/events_client.rs \
    expect=real_events_server_accepts_the_client_wire_and_serves_a_page \
    find='            .set("mcp-method", method)' \
    replace='            .set("mcp-not-method", method)'

mutation "S2  the client drops _meta protocolVersion (-32602)" \
    file=src/events_client.rs \
    expect=real_events_server_accepts_the_client_wire_and_serves_a_page \
    find='        .entry("io.modelcontextprotocol/protocolVersion")' \
    replace='        .entry("io.modelcontextprotocol/notTheProtocolVersion")'

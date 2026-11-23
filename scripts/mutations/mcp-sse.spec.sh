#!/usr/bin/env bash
# Sourced by scripts/verify-mutations.sh.
# bd-1o41y: dropping SSE frame parsing must turn the remote-client test RED.

CARGO_ARGS=(-p flywheel --bin flywheel)
TEST_FILTER=(remote_client_survives_an_sse_initialize real_initialize_sse_negotiation_is_used_on_the_next_call)

mutation "S1  the remote client parses only bare JSON" \
    file=src/mcp_http.rs \
    expect=remote_client_survives_an_sse_initialize \
    find='    for frame in body.split("\n\n") {' \
    replace='    for frame in std::iter::empty::<&str>() {'

mutation "S2  the negotiated protocol version is ignored" \
    file=src/mcp_http.rs \
    expect=real_initialize_sse_negotiation_is_used_on_the_next_call \
    find='            .set("MCP-Protocol-Version", self.protocol_version());' \
    replace='            .set("MCP-Protocol-Version", PROTOCOL_VERSION);'

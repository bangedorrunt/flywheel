# Sourced by scripts/verify-mutations.sh. bd-rrlo2: the events client must
# send the protocol version in _meta and the method in the mcp-method header.

CARGO_ARGS=(--bin flywheel)
TEST_FILTER=(events_requests_declare_the_protocol_version events_requests_carry_the_mcp_method_header)

mutation "M1  events requests omit the protocol version" \
    file=src/events_client.rs \
    expect=events_requests_declare_the_protocol_version \
    find='        .entry("io.modelcontextprotocol/protocolVersion")' \
    replace='        .entry("io.modelcontextprotocol/notTheProtocolVersion")'

mutation "M2  events requests omit the method header" \
    file=src/events_client.rs \
    expect=events_requests_carry_the_mcp_method_header \
    find='            .set("mcp-method", method)' \
    replace='            .set("mcp-not-method", method)'

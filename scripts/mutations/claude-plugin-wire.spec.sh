# Sourced by scripts/verify-mutations.sh. bd-gu288: the plugin must use the
# real events surface and carry no duplicate Rust decision.

CARGO_ARGS=(--test claude_plugin_wire)
TEST_FILTER=(claude_plugin_speaks_real_events_surface)

mutation "W1  the plugin stops using the local mcp-events proxy" \
    file=plugins/claude-flywheel/hooks/register.ts \
    expect=claude_plugin_speaks_real_events_surface \
    find=$'      "mcp-events",\n      "--subscription",' \
    replace=$'      "events/poll",\n      "--subscription",'

mutation "W2  the decoder reverts to the occurrence method" \
    file=plugins/claude-flywheel/src/decode.ts \
    expect=claude_plugin_speaks_real_events_surface \
    find='PROXY_METHOD = "notifications/claude/channel"' \
    replace='PROXY_METHOD = "notifications/events/event"'

mutation "W3  the hint drops the neutral wake prefix" \
    file=plugins/claude-flywheel/src/wake_prefix.ts \
    expect=claude_plugin_speaks_real_events_surface \
    find='[wake]' \
    replace='[notice]'

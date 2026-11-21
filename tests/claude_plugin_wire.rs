//! bd-gu288: the Claude wake plugin must speak the real toron events surface
//! and own no duplicate Rust decision. Source scan so the proof's mutation
//! stage can make every assertion fail.

use std::path::Path;

fn read(rel: &str) -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

#[test]
fn claude_plugin_speaks_real_events_surface() {
    let register = read("plugins/claude-flywheel/hooks/register.ts");
    let decode = read("plugins/claude-flywheel/src/decode.ts");
    let hint = read("plugins/claude-flywheel/src/hint.ts");
    let prefix = read("plugins/claude-flywheel/src/wake_prefix.ts");

    assert!(
        !register.contains("$.mcp.call"),
        "events/* are JSON-RPC methods, never MCP tools"
    );
    assert!(
        register.contains("\"mcp-events\","),
        "the local proxy owns the token; the plugin never reads it"
    );
    // bd-krzqu: the ONE wire contract is the proxy's notifications/claude/channel
    // frame with bus ids in params.meta and the rendered hint in params.content.
    assert!(
        decode.contains("PROXY_METHOD = \"notifications/claude/channel\""),
        "decode the frame the proxy really emits"
    );
    assert!(
        decode.contains("meta.event_id") && decode.contains("meta.event_name"),
        "the bus ids come from params.meta"
    );
    assert!(
        decode.contains("params.content"),
        "the rendered hint comes from params.content"
    );
    assert!(
        !decode.contains("content.includes"),
        "urgency comes from a typed field, never body text"
    );
    let fixture = read("plugins/claude-flywheel/test/fixtures/proxy-notice.json");
    assert!(
        fixture.contains("\"method\": \"notifications/claude/channel\""),
        "the captured frame uses the real method"
    );
    assert!(
        hint.contains("WAKE_PREFIX"),
        "the hint must use the shared prefix"
    );
    assert!(prefix.contains("[wake]"), "agent_text's neutral prefix");

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert!(
        !root.join("src/hook_decide.rs").exists(),
        "one decision only: the plugin's TypeScript decide()"
    );
}

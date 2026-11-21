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
        register.contains("mcp-events"),
        "the local proxy owns the token; the plugin never reads it"
    );
    assert!(
        decode.contains("eventId"),
        "toron occurrences carry eventId, not id/content"
    );
    assert!(
        decode.contains("subscriptionId"),
        "frames are routed by subscriptionId"
    );
    assert!(
        !decode.contains("content.includes"),
        "urgency comes from a typed field, never body text"
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

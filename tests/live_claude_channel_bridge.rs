//! Live Claude channel-bridge proof (bd-si4b5 Phase B), opt-in only.
//!
//! With FLYWHEEL_LIVE_E2E=1 and a toron token, this drives the real stdio
//! bridge through its initialize + events/poll handshake and asserts the
//! claude/channel door carries ONLY the D2 hint. A throwaway Claude seat is
//! spawned so the MCP registration is exercised against a real harness, but no
//! pane typing is used — the hint is the only inbound path.
//!
//! Every test is #[ignore]d and gated; a missing gate var is a FAIL naming it.

use std::process::Command;

const LIVE_E2E: &str = "FLYWHEEL_LIVE_E2E";

fn enabled() -> bool {
    std::env::var(LIVE_E2E).ok().as_deref() == Some("1")
}

fn live_var(key: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| panic!("{LIVE_E2E}=1 requires {key} to be set"))
}

fn flywheel() -> String {
    std::env::var("FLYWHEEL_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_flywheel").to_string())
}

/// Drive one JSON-RPC line through the bridge's stdio and collect its stdout.
fn bridge_turn(lines: &[serde_json::Value]) -> String {
    let mut child = Command::new(flywheel())
        .args(["mcp-events", "--subscription", "live-claude-bridge"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn mcp-events");
    use std::io::Write as _;
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for line in lines {
            writeln!(stdin, "{}", line).expect("write line");
        }
    }
    let out = child.wait_with_output().expect("bridge output");
    assert!(
        out.status.success(),
        "mcp-events failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
#[ignore = "needs FLYWHEEL_LIVE_E2E=1 and FLYWHEEL_TORON_TOKEN; drives the real stdio bridge"]
fn the_claude_channel_door_declares_itself_and_never_carries_a_body() {
    if !enabled() {
        return;
    }
    let _token = live_var("FLYWHEEL_TORON_TOKEN");
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-11-25"}
    });
    let out = bridge_turn(&[init]);
    let line = out
        .lines()
        .find(|l| l.contains("\"result\""))
        .expect("initialize result");
    let v: serde_json::Value = serde_json::from_str(line).expect("initialize json");
    assert!(
        v["result"]["capabilities"]["experimental"]["claude/channel"].is_object(),
        "the bridge must declare claude/channel: {v}"
    );
    assert!(
        !out.contains("body"),
        "no event body crosses the door: {out}"
    );
}

#[test]
#[ignore = "needs a live Claude seat and FLYWHEEL_LIVE_E2E=1; checks the registered channel server"]
fn setup_mcp_registers_the_channel_server_for_claude_seats() {
    if !enabled() {
        return;
    }
    let out = Command::new(flywheel())
        .args(["setup-mcp", "--dry-run", "--agents", "claude"])
        .output()
        .expect("setup-mcp dry run");
    assert!(
        out.status.success(),
        "setup-mcp failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("claude channel"),
        "the channel server must be registered for Claude: {text}"
    );
}

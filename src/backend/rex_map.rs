//! Rex session/block projectors onto the herdr shapes flywheel already
//! consumes (bd-i328).
//!
//! Pure by contract: no process spawn, no live Rex, no I/O. Callers hand in
//! ids and the terminal's status records; these functions only map them.
//!
//! governed-by: ADR-0002

// The mappers are the landing surface for the Rex adapter; nothing wires them
// yet, and this is a bin crate where `pub` does not exempt dead code.
#![allow(dead_code)]

use std::collections::BTreeMap;

use serde_json::Value;
use toron_core::backend::AgentStatus;
use toron_core::backend::herdr::{PaneInfo, WorkspaceInfo};
use toron_core::osc7501::{self, ProgramState};

/// A Rex session is a herdr workspace; the session id is the workspace id.
/// Its agent status is `Unknown`: a session with no records proves nothing.
pub fn session_to_workspace(session_id: &str, label: &str) -> WorkspaceInfo {
    WorkspaceInfo {
        workspace_id: session_id.to_string(),
        number: 0,
        label: label.to_string(),
        focused: false,
        pane_count: 0,
        tab_count: 0,
        active_tab_id: String::new(),
        agent_status: AgentStatus::Unknown,
        cwd: None,
    }
}

/// A Rex block is a herdr pane: block id is the pane id, session id is the
/// workspace id, and a Rex window is the tab. `terminal_id` stays empty —
/// Rex has no second terminal handle beyond the block.
pub fn block_to_pane(session_id: &str, block_id: &str, label: &str, window_id: &str) -> PaneInfo {
    PaneInfo {
        pane_id: block_id.to_string(),
        terminal_id: String::new(),
        workspace_id: session_id.to_string(),
        tab_id: window_id.to_string(),
        focused: false,
        agent_status: AgentStatus::Unknown,
        revision: 0,
        title: None,
        cwd: None,
        agent: None,
        agent_session: None,
        display_agent: None,
        foreground_cwd: None,
        label: Some(label.to_string()),
        scroll: None,
        state_labels: serde_json::Map::new(),
        terminal_title: None,
        terminal_title_stripped: None,
        tokens: serde_json::Map::new(),
    }
}

/// Fold one terminal's records into a single status.
///
/// `records_json` is a JSON array (or a bare record) whose items are either
/// raw OSC 7501 strings or objects carrying `state` (and optionally `id`).
/// Both go through `osc7501`, so string and object records obey one grammar:
/// a discarded report changes nothing, and `clear` removes records instead of
/// being stored.
///
/// Rex `idle` is at-a-prompt and is reported as `Idle` here; `error` is
/// `Unknown`, never `Done` and never `Idle`.
pub fn status_from_records(records_json: &str) -> AgentStatus {
    let Ok(value) = serde_json::from_str::<Value>(records_json) else {
        return AgentStatus::Unknown;
    };
    let items = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    let mut records: BTreeMap<String, ProgramState> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for item in items {
        let Some((id, state)) = record_state(&item) else {
            continue;
        };
        // Mirrors `osc7501::Board::apply`: clear drops the record and its children.
        if state == ProgramState::Clear {
            if id.is_empty() {
                records.clear();
            } else {
                let prefix = format!("{id}/");
                records.retain(|key, _| key != &id && !key.starts_with(&prefix));
            }
            continue;
        }
        records.insert(id.clone(), state);
        order.push(id);
    }
    // A root record speaks for the whole terminal, so a blocked child never
    // hides a working root. With no root, one blocked child blocks; otherwise
    // the newest surviving record speaks, and a cleared one is gone.
    if let Some(state) = records.get("") {
        return status_of(*state);
    }
    if records
        .values()
        .any(|state| *state == ProgramState::Blocked)
    {
        return AgentStatus::Blocked;
    }
    order
        .iter()
        .rev()
        .find_map(|id| records.get(id))
        .map_or(AgentStatus::Unknown, |state| status_of(*state))
}

/// Parse one record into `(id, state)`. The empty id is the root.
fn record_state(item: &Value) -> Option<(String, ProgramState)> {
    // ponytail: an object record is rendered as an OSC report so `osc7501`
    // stays the only parser; an id outside the OSC id grammar drops the
    // record, same as a malformed raw string. The empty id is omitted: `id=`
    // with no value is rejected by the parser and would lose the record.
    let raw = match item {
        Value::String(raw) => raw.clone(),
        Value::Object(map) => {
            let state = map.get("state").and_then(Value::as_str)?;
            let id = map.get("id").and_then(Value::as_str).unwrap_or_default();
            if id.is_empty() {
                format!("\u{1b}]7501;state={state}\u{1b}\\")
            } else {
                format!("\u{1b}]7501;state={state}:id={id}\u{1b}\\")
            }
        }
        _ => return None,
    };
    let report = osc7501::parse_report(&raw)?;
    Some((report.id.unwrap_or_default(), report.state))
}

fn status_of(state: ProgramState) -> AgentStatus {
    match state {
        ProgramState::Idle => AgentStatus::Idle,
        ProgramState::Working => AgentStatus::Working,
        ProgramState::Blocked => AgentStatus::Blocked,
        ProgramState::Done => AgentStatus::Done,
        // A failed program is not a finished one, and not an idle one.
        ProgramState::Error => AgentStatus::Unknown,
        // Never stored: `clear` removes records in the fold above.
        ProgramState::Clear => AgentStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(body: &str) -> String {
        format!("\u{1b}]7501;{body}\u{1b}\\")
    }

    #[test]
    fn osc_error_is_not_done() {
        // The whole point of bd-i328: a failed program is Unknown.
        let failed = serde_json::json!([osc("state=error:app=freebuff")]).to_string();
        let status = status_from_records(&failed);
        assert_eq!(status, AgentStatus::Unknown);
        assert_ne!(status, AgentStatus::Done);
        assert_ne!(status, AgentStatus::Idle);

        // An object record reads the same way, and error still is not done.
        assert_eq!(
            status_from_records(r#"[{"state":"error","app":"freebuff"}]"#),
            AgentStatus::Unknown
        );

        let idle = serde_json::json!([osc("state=idle:app=claude-code")]).to_string();
        assert_eq!(status_from_records(&idle), AgentStatus::Idle);
        assert_eq!(
            status_from_records(r#"[{"state":"idle"}]"#),
            AgentStatus::Idle
        );

        let blocked = serde_json::json!([osc("state=blocked:kind=permission")]).to_string();
        assert_eq!(status_from_records(&blocked), AgentStatus::Blocked);
        assert_eq!(
            status_from_records(r#"[{"state":"blocked"}]"#),
            AgentStatus::Blocked
        );

        assert_eq!(status_from_records("[]"), AgentStatus::Unknown);
        assert_eq!(status_from_records("not json"), AgentStatus::Unknown);
    }

    #[test]
    fn root_record_outranks_a_blocked_child() {
        let records = serde_json::json!([
            osc("state=working:app=deploy"),
            osc("state=blocked:id=eu-west:kind=permission"),
        ])
        .to_string();
        assert_eq!(status_from_records(&records), AgentStatus::Working);

        // No root: a blocked child blocks the terminal.
        let child_only = serde_json::json!([
            osc("state=working:id=us-east"),
            osc("state=blocked:id=eu-west"),
        ])
        .to_string();
        assert_eq!(status_from_records(&child_only), AgentStatus::Blocked);

        // Clear is not a stored status: it drops the record it names.
        let cleared = serde_json::json!([
            osc("state=blocked:id=eu-west"),
            osc("state=clear:id=eu-west"),
        ])
        .to_string();
        assert_eq!(status_from_records(&cleared), AgentStatus::Unknown);
    }

    #[test]
    fn session_and_block_project_onto_herdr_shapes() {
        let workspace = session_to_workspace("sess-1", "rex-build");
        assert_eq!(workspace.workspace_id, "sess-1");
        assert_eq!(workspace.label, "rex-build");
        assert_eq!(workspace.agent_status, AgentStatus::Unknown);

        let pane = block_to_pane("sess-1", "blk-7", "cargo test", "win-2");
        assert_eq!(pane.pane_id, "blk-7");
        assert_eq!(pane.workspace_id, "sess-1");
        assert_eq!(pane.label.as_deref(), Some("cargo test"));
        assert_eq!(pane.tab_id, "win-2");
    }
}

// governed-by: ADR-0039
//! bd-lk0kt: the pre-edit hook takes the lease. A seat never has to remember
//! to reserve before writing; the first edit acquires, a conflict queues one
//! waiter and names the holder, and a repeat by the holder is a no-op.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, bail};

/// The tool being invoked. PreToolUse fires for EVERY tool; a read carrying a
/// `file_path` must never take a lease (bd-lk0kt rework).
pub fn tool_name_from_payload(payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    for key in ["tool_name", "tool", "name"] {
        if let Some(name) = value[key].as_str()
            && !name.is_empty()
        {
            return Some(name.to_string());
        }
    }
    None
}

/// Write tools across the harnesses flywheel installs. Reads, greps and
/// globs are deliberately absent.
pub fn is_write_tool(name: &str) -> bool {
    matches!(
        name,
        "Edit"
            | "Write"
            | "MultiEdit"
            | "NotebookEdit"
            | "apply_patch"
            | "str_replace_editor"
            | "create_file"
            | "write_file"
            | "replace_in_file"
            | "patch"
    )
}

/// Pull the edited path out of a harness hook payload. Claude sends
/// `tool_input.file_path`; other harnesses use `file_path` or `path`.
pub fn path_from_payload(payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    for candidate in [
        value["tool_input"]["file_path"].as_str(),
        value["tool_input"]["path"].as_str(),
        value["file_path"].as_str(),
        value["path"].as_str(),
    ] {
        if let Some(path) = candidate
            && !path.trim().is_empty()
        {
            return Some(path.to_string());
        }
    }
    None
}

/// A path outside the primary repo is spelled `../<repo>/<path>` so the lease
/// board keys it exactly as an agent would reserve it by hand.
pub fn lease_path(path: &str, workspace: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        let root = std::path::Path::new(workspace);
        if let Ok(rel) = p.strip_prefix(root) {
            return rel.to_string_lossy().to_string();
        }
        return path.to_string();
    }
    path.to_string()
}

/// The seat's live brief bead, or a stable fallback that `crew done` and
/// `ship land release-by-reason` also know how to free (bd-lk0kt rework).
pub fn lease_reason(project: &str, pin: &str) -> String {
    crate::cmd::crew::current_bead_for_pin(project, pin)
        .filter(|bead| !bead.is_empty())
        .unwrap_or_else(|| format!("pre-edit:{pin}"))
}

/// Claude's PreToolUse contract: exit 2 denies the write; 0 lets it proceed.
#[must_use]
pub fn block_exit_code(holder: &str) -> i32 {
    if holder.is_empty() { 0 } else { 2 }
}

pub fn blocked_message(path: &str, holder: &str) -> String {
    format!(
        "blocked: {path} is leased by {holder}; your edit was queued and you will be woken when the file is handed over"
    )
}

/// Invoke toron's queued acquire. The CLI owns the waitlist and the single
/// request-release per holder; this module only translates the outcome.
pub fn acquire(project: &str, pin: &str, path: &str, reason: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("toron");
    cmd.args([
        "reserve",
        "acquire",
        "--project",
        project,
        "--as",
        pin,
        "--paths",
        path,
        "--exclusive",
        "--queue",
    ]);
    if let Some(reason) = reason.filter(|r| !r.is_empty()) {
        cmd.args(["--reason", reason]);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "pre-edit lease failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().find(|line| line.trim_start().starts_with('{'));
    let value: serde_json::Value = serde_json::from_str(first.unwrap_or("{}"))?;
    let holder = value["conflicts"]
        .as_array()
        .and_then(|rows| rows.first())
        .and_then(|row| row["holders"].as_array())
        .and_then(|names| names.first())
        .and_then(|name| name.as_str())
        .unwrap_or_default();
    Ok(holder.to_string())
}

pub fn workspace_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_payload_yields_the_edited_path() {
        assert_eq!(
            path_from_payload(r#"{"tool_input":{"file_path":"src/main.rs"}}"#),
            Some("src/main.rs".into())
        );
        assert_eq!(
            path_from_payload(r#"{"file_path":"../toron/src/lib.rs"}"#),
            Some("../toron/src/lib.rs".into())
        );
        assert_eq!(path_from_payload("not json"), None);
    }

    #[test]
    fn only_write_tools_are_leased() {
        let read = r#"{"tool_name":"Read","tool_input":{"file_path":"src/main.rs"}}"#;
        assert_eq!(tool_name_from_payload(read).as_deref(), Some("Read"));
        assert!(!is_write_tool("Read"));
        assert!(!is_write_tool("Grep"));
        assert!(!is_write_tool("Glob"));
        let write = r#"{"tool_name":"Edit","tool_input":{"file_path":"src/main.rs"}}"#;
        assert_eq!(tool_name_from_payload(write).as_deref(), Some("Edit"));
        assert!(is_write_tool("Edit"));
        assert!(is_write_tool("Write"));
    }

    #[test]
    fn lease_reason_falls_back_to_a_releasable_name() {
        let reason = lease_reason("no-such-project-for-pre-edit", "Alpha");
        assert_eq!(reason, "pre-edit:Alpha");
    }

    #[test]
    fn the_hook_filters_write_tools_at_the_door() {
        let main = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"),
        )
        .unwrap();
        assert!(
            main.contains("if !crate::pre_edit::is_write_tool(&tool)"),
            "PreToolUse must ignore reads before taking any lease"
        );
    }

    #[test]
    fn a_held_path_denies_the_edit() {
        assert_eq!(block_exit_code("TealCove"), 2);
        assert_eq!(block_exit_code(""), 0);
    }

    #[test]
    fn a_blocked_edit_names_the_holder() {
        let msg = blocked_message("src/main.rs", "TealCove");
        assert!(msg.contains("src/main.rs") && msg.contains("TealCove"));
        assert!(msg.contains("queued"));
    }
}

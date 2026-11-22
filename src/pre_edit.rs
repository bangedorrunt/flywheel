// governed-by: ADR-0039
//! bd-lk0kt: the pre-edit hook takes the lease. A seat never has to remember
//! to reserve before writing; the first edit acquires, a conflict queues one
//! waiter and names the holder, and a repeat by the holder is a no-op.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Result, bail};

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
    if path.starts_with("../") || !path.contains('/') {
        return path.to_string();
    }
    // A relative path with a directory is already project-relative.
    path.to_string()
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
    fn a_blocked_edit_names_the_holder() {
        let msg = blocked_message("src/main.rs", "TealCove");
        assert!(msg.contains("src/main.rs") && msg.contains("TealCove"));
        assert!(msg.contains("queued"));
    }
}

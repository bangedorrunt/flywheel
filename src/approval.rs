//! Read-only commit-approval check (bd-bd5j). One reader every plane can
//! call: `sha_approved` answers whether an approvals file holds a line for
//! this repo at exactly this sha. No subcommand, no daemon, no writes — the
//! approvals file is committed by a human, never by the commit it approves.

use std::path::{Path, PathBuf};

/// Where the approvals file lives when the caller does not pass a path.
/// Tests never touch this path; they use a temp file.
pub fn default_approvals_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/state/stack-comms/approvals.jsonl")
}

/// A sha counts only as the full 40 hex digits. Anything shorter (or not
/// hex at all) is not an approval, even if a line happens to name it.
fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// True only when some line of `file` names `repo` with exactly `sha`.
/// A missing file, a short sha, a different repo, and any malformed line
/// are all false. A bad line is skipped, never a crash.
pub fn sha_approved(file: &Path, repo: &str, sha: &str) -> bool {
    if !is_full_sha(sha) {
        return false;
    }
    let Ok(body) = std::fs::read_to_string(file) else {
        return false;
    };
    body.lines().any(|line| {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return false;
        };
        v.get("repo").and_then(|r| r.as_str()) == Some(repo)
            && v.get("sha").and_then(|s| s.as_str()) == Some(sha)
            && v.get("sha")
                .and_then(|s| s.as_str())
                .is_some_and(is_full_sha)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn line(repo: &str, sha: &str) -> String {
        format!(
            "{{\"repo\":\"{repo}\",\"sha\":\"{sha}\",\"approver\":\"IvoryMarlin\",\"at\":\"2026-10-23T00:00:00Z\",\"subject\":\"approve toron comms slice\"}}\n"
        )
    }

    #[test]
    fn an_unapproved_sha_refuses() {
        // tempfile keeps the fixture off the operator's state dir.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("approvals.jsonl");
        assert!(
            !sha_approved(&file, "toron", SHA),
            "a missing file is not an approval"
        );
        std::fs::write(&file, "").unwrap();
        assert!(
            !sha_approved(&file, "toron", SHA),
            "an empty file approves nothing"
        );
        std::fs::write(&file, line("toron", SHA)).unwrap();
        assert!(
            sha_approved(&file, "toron", SHA),
            "a matching repo and sha approves"
        );
        assert!(
            !sha_approved(&file, "toron", "abc1234"),
            "a 7-hex prefix is not an approval"
        );
        assert!(
            !sha_approved(&file, "flywheel", SHA),
            "the other repo's line is not this repo's approval"
        );
        // A bad line is skipped, not a crash; the good line still counts.
        std::fs::write(&file, format!("not json\n{}", line("toron", SHA))).unwrap();
        assert!(sha_approved(&file, "toron", SHA));
    }
}

//! Bead-optional work units (bd-ihiqo.3).
//!
//! One [`WorkRef`] names the work: a bead id, or an ad-hoc objective plus
//! the verify command that proves it. Closing a bead runs its `br` verify
//! and the gate+close ceremony; closing ad-hoc work commits the named
//! paths and writes a journal receipt next to the bead receipts, so both
//! closes leave the same evidence shape. `--bead auto` mints a bead only
//! when the work outlives the attempt that ran it.

use anyhow::{Context, Result};

/// What a unit of work points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkRef {
    /// A tracked bead: verify comes from `br show`, close is the gate +
    /// `br close` ceremony (the captain's `ship land` ships it later).
    Bead { id: String },
    /// Untracked work: the caller names the verify command outright.
    AdHoc { objective: String, verify: String },
}

/// The verify command for a unit: `br show` for beads, as-given for ad-hoc.
pub fn verify_command(dir: &std::path::Path, target: &WorkRef) -> Result<String> {
    match target {
        WorkRef::AdHoc { verify, .. } => {
            if verify.trim().is_empty() {
                anyhow::bail!("ad-hoc work needs --verify <command>");
            }
            Ok(verify.clone())
        }
        WorkRef::Bead { id } => {
            let raw = crate::br::run_br(Some(dir), &["show", id, "--json"])
                .with_context(|| format!("br show {id}"))?;
            verify_from_br_json(id, &raw)
        }
    }
}

/// Pick the verify command out of `br show --json` output (a single bead
/// object or a one-element array, depending on the br version).
pub fn verify_from_br_json(id: &str, raw: &str) -> Result<String> {
    let v: serde_json::Value =
        serde_json::from_str(raw).with_context(|| format!("br show {id} is not JSON"))?;
    let bead: Option<&serde_json::Value> = match v.as_array() {
        Some(a) => a.first(),
        None => Some(&v),
    };
    let verify = bead
        .and_then(|b| b.get("verify"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if verify.is_empty() {
        anyhow::bail!("bead {id} names no verify command; dress it before closing");
    }
    Ok(verify)
}

/// Whether an ad-hoc close mints a bead: only when the work outlives this
/// attempt (verify failed, so the remainder needs tracking). A green close
/// needs no bead — the work is done, not handed on.
pub fn needs_bead_on_failure(verify_passed: bool, auto_bead: bool) -> bool {
    !verify_passed && auto_bead
}

/// The journal receipt path for an ad-hoc close, beside the bead receipts:
/// `.flywheel/receipts/adhoc-<slug>-<shortsha>.txt`.
pub fn adhoc_receipt_path(
    repo: &std::path::Path,
    objective: &str,
    sha: &str,
) -> std::path::PathBuf {
    let mut slug: String = objective
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if c == ' ' || c == '-' || c == '_' {
                Some('-')
            } else {
                None
            }
        })
        .collect();
    slug.truncate(32);
    let slug = slug.trim_matches('-');
    let short = sha.chars().take(12).collect::<String>();
    repo.join(".flywheel")
        .join("receipts")
        .join(format!("adhoc-{slug}-{short}.txt"))
}

/// The REPORT body every close leaves behind: status, commit, verify
/// command plus its tail. Same fields as the bead ceremony's report.
pub fn close_report(status: &str, sha: &str, verify: &str, tail: &str) -> String {
    format!("status: {status}\ncommit: {sha}\nverify: {verify}\n{tail}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ad-hoc units carry their verify command; beads without one refuse.
    /// Bead JSON parses the same way `br show --json` emits it.
    #[test]
    fn adhoc_verify_comes_from_the_flag() {
        let dir = std::path::Path::new(".");
        let adhoc = WorkRef::AdHoc {
            objective: "o".into(),
            verify: "cargo test foo".into(),
        };
        assert_eq!(verify_command(dir, &adhoc).unwrap(), "cargo test foo");
        let empty = WorkRef::AdHoc {
            objective: "o".into(),
            verify: "  ".into(),
        };
        assert!(verify_command(dir, &empty).is_err());
        let raw = r#"[{"id":"bd-1","verify":"make check"}]"#;
        assert_eq!(verify_from_br_json("bd-1", raw).unwrap(), "make check");
        assert!(verify_from_br_json("bd-1", "[]").is_err());
        assert!(verify_from_br_json("bd-1", "not json").is_err());
    }

    /// A bead is minted on failure with --auto-bead, never on a green close.
    #[test]
    fn bead_auto_fires_only_when_work_outlives_the_attempt() {
        assert!(needs_bead_on_failure(false, true));
        assert!(!needs_bead_on_failure(true, true));
        assert!(!needs_bead_on_failure(false, false));
    }

    /// Receipts land beside the bead receipts with a stable name.
    #[test]
    fn adhoc_receipt_path_is_stable() {
        let p = adhoc_receipt_path(
            std::path::Path::new("/r"),
            "Fix the Thing!",
            "abcdef0123456789",
        );
        assert_eq!(
            p,
            std::path::Path::new("/r/.flywheel/receipts/adhoc-fix-the-thing-abcdef012345.txt")
        );
    }

    /// The journal receipt carries status, commit, verify command and tail.
    #[test]
    fn close_report_carries_verify_and_tail() {
        let r = close_report("closed", "abc123", "cargo test", "ok 4 passed");
        assert!(r.contains("cargo test"), "verify command present: {r}");
        assert!(r.contains("ok 4 passed"), "verify tail present: {r}");
        assert!(r.contains("abc123"), "commit present: {r}");
    }
}

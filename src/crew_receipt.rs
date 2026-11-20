//! Typed crew receipts (bd-bi0mv).
//!
//! A receipt the captain acts on is a validated schema, not prose. Parsing and
//! structural validation are pure; existence and ancestry checks are injected
//! so the core is testable without a live repo or ledger.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "flywheel-crew-receipt.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FenceVerdict {
    Green,
    Red,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadRef {
    pub id: String,
    pub repo: String,
    pub sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fence {
    pub sha: String,
    pub verdict: FenceVerdict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tests {
    pub cmd: String,
    pub tail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewReceipt {
    pub schema: String,
    pub brief_id: String,
    pub beads: Vec<BeadRef>,
    pub fence: Fence,
    pub tests: Tests,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutations: Option<Vec<String>>,
    /// RED mutation witnesses counted at landing (bd-r7852).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub witnesses: Option<Vec<crate::mutation_witnesses::Witness>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub field: String,
    pub reason: String,
}

impl std::fmt::Display for FieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.reason)
    }
}
impl std::error::Error for FieldError {}

fn err(field: &str, reason: &str) -> FieldError {
    FieldError {
        field: field.to_string(),
        reason: reason.to_string(),
    }
}

fn is_sha40(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn parse(raw: &str) -> Result<CrewReceipt, FieldError> {
    serde_json::from_str(raw).map_err(|e| err("json", &e.to_string()))
}

/// The ONE repo resolver, used by validation and by land. `.` is the project
/// repo, `../name` one workspace sibling, an absolute path only when already
/// under the workspace. More than one `..` is refused rather than escaping.
pub fn resolve_repo(
    workspace_root: &Path,
    project: &str,
    repo: &str,
) -> Result<PathBuf, FieldError> {
    if repo.trim().is_empty() {
        return Err(err("repo", "must not be empty"));
    }
    let p = Path::new(repo);
    let ups = p
        .components()
        .filter(|c| matches!(c, Component::ParentDir))
        .count();
    if ups > 1 {
        return Err(err("repo", "at most one '..' is allowed"));
    }
    if p.is_absolute() {
        return if p.starts_with(workspace_root) {
            Ok(p.to_path_buf())
        } else {
            Err(err("repo", "absolute path is outside the workspace"))
        };
    }
    if repo == "." {
        return Ok(workspace_root.join(project));
    }
    if let Ok(rest) = p.strip_prefix("..") {
        return Ok(workspace_root.join(rest));
    }
    Ok(workspace_root.join(p))
}

/// Injected facts: no git, br or cwd access lives in the validation core.
pub trait ReceiptChecks {
    fn bead_exists(&self, id: &str) -> bool;
    fn commit_exists(&self, repo: &Path, sha: &str) -> bool;
    fn is_ancestor(&self, repo: &Path, ancestor: &str, descendant: &str) -> bool;
}

pub fn validate_structure(r: &CrewReceipt) -> Result<(), FieldError> {
    if r.schema != SCHEMA {
        return Err(err(
            "schema",
            &format!("expected {SCHEMA}, got {}", r.schema),
        ));
    }
    if r.brief_id.trim().is_empty() {
        return Err(err("brief_id", "must not be empty"));
    }
    if r.beads.is_empty() {
        return Err(err("beads", "must name at least one bead"));
    }
    for (i, b) in r.beads.iter().enumerate() {
        if b.id.trim().is_empty() {
            return Err(err(&format!("beads[{i}].id"), "must not be empty"));
        }
        if b.repo.trim().is_empty() {
            return Err(err(&format!("beads[{i}].repo"), "must not be empty"));
        }
        if !is_sha40(&b.sha) {
            return Err(err(
                &format!("beads[{i}].sha"),
                "must be a bare 40-hex commit sha",
            ));
        }
    }
    if !is_sha40(&r.fence.sha) {
        return Err(err("fence.sha", "must be a bare 40-hex commit sha"));
    }
    if r.fence.verdict != FenceVerdict::Green {
        return Err(err("fence.verdict", "expected green"));
    }
    if r.tests.cmd.trim().is_empty() {
        return Err(err("tests.cmd", "must not be empty"));
    }
    Ok(())
}

pub fn validate_existence<C>(
    r: &CrewReceipt,
    workspace_root: &Path,
    project: &str,
    checks: &C,
) -> Result<(), FieldError>
where
    C: ReceiptChecks,
{
    for (i, b) in r.beads.iter().enumerate() {
        if !checks.bead_exists(&b.id) {
            return Err(err(
                &format!("beads[{i}].id"),
                &format!("no such bead {}", b.id),
            ));
        }
        let repo = resolve_repo(workspace_root, project, &b.repo)
            .map_err(|e| err(&format!("beads[{i}].repo"), &e.reason))?;
        if !checks.commit_exists(&repo, &b.sha) {
            return Err(err(
                &format!("beads[{i}].sha"),
                &format!("{} not found in {}", b.sha, repo.display()),
            ));
        }
        let proved = b.sha == r.fence.sha || checks.is_ancestor(&repo, &b.sha, &r.fence.sha);
        if !proved {
            return Err(err(
                "fence.sha",
                &format!(
                    "{} does not prove bead {} sha {} in {}",
                    r.fence.sha,
                    b.id,
                    b.sha,
                    repo.display()
                ),
            ));
        }
    }
    Ok(())
}

pub fn validate<C>(
    raw: &str,
    workspace_root: &Path,
    project: &str,
    checks: &C,
) -> Result<CrewReceipt, FieldError>
where
    C: ReceiptChecks,
{
    let r = parse(raw)?;
    validate_structure(&r)?;
    validate_existence(&r, workspace_root, project, checks)?;
    Ok(r)
}

pub fn canonical(r: &CrewReceipt) -> String {
    serde_json::to_string(r).expect("a parsed receipt always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_raw() -> String {
        format!(
            r#"{{
                "schema": "{SCHEMA}",
                "brief_id": "ScarletCove-1",
                "beads": [
                    {{"id":"bd-a","repo":".","sha":"{}"}},
                    {{"id":"bd-b","repo":"../toron","sha":"{}"}}
                ],
                "fence": {{"sha": "{}", "verdict": "green"}},
                "tests": {{"cmd": "cargo test", "tail": "ok"}}
            }}"#,
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(40),
        )
    }

    struct FakeChecks;
    impl ReceiptChecks for FakeChecks {
        fn bead_exists(&self, _id: &str) -> bool {
            true
        }
        fn commit_exists(&self, _repo: &Path, _sha: &str) -> bool {
            true
        }
        fn is_ancestor(&self, _repo: &Path, _ancestor: &str, _descendant: &str) -> bool {
            true
        }
    }

    fn root() -> PathBuf {
        PathBuf::from("/ws")
    }

    #[test]
    fn a_valid_receipt_parses_and_validates() {
        let r = validate(&valid_raw(), &root(), "flywheel", &FakeChecks).expect("valid");
        assert_eq!(r.beads[0].id, "bd-a");
        assert_eq!(r.fence.verdict, FenceVerdict::Green);
    }

    #[test]
    fn a_bad_field_names_itself_in_the_error() {
        let cases: Vec<(&str, String, &str)> = vec![
            ("schema", valid_raw().replace(SCHEMA, "wrong.v1"), "schema"),
            (
                "bead id",
                valid_raw().replace("\"bd-a\"", "\"\""),
                "beads[0].id",
            ),
            (
                "commit sha",
                valid_raw().replace(&"a".repeat(40), "short"),
                "beads[0].sha",
            ),
            (
                "fence verdict",
                valid_raw().replace("green", "red"),
                "fence.verdict",
            ),
            (
                "tests cmd",
                valid_raw().replace("cargo test", ""),
                "tests.cmd",
            ),
        ];
        for (label, raw, field) in cases {
            let e = validate(&raw, &root(), "flywheel", &FakeChecks).expect_err(label);
            assert_eq!(e.field, field, "{label}: {e}");
        }
    }

    #[test]
    fn a_verdict_typo_fails_at_parse() {
        let raw = valid_raw().replace("green", "grean");
        let e = validate(&raw, &root(), "flywheel", &FakeChecks).expect_err("typo");
        assert_eq!(e.field, "json");
    }

    #[test]
    fn malformed_json_names_the_json_field() {
        let e = validate("not json", &root(), "flywheel", &FakeChecks).expect_err("bad json");
        assert_eq!(e.field, "json");
    }

    #[test]
    fn repo_resolution_is_shared_and_bounded() {
        assert_eq!(
            resolve_repo(&root(), "flywheel", ".").unwrap(),
            PathBuf::from("/ws/flywheel")
        );
        assert_eq!(
            resolve_repo(&root(), "flywheel", "../toron").unwrap(),
            PathBuf::from("/ws/toron")
        );
        assert_eq!(
            resolve_repo(&root(), "flywheel", "/ws/toron").unwrap(),
            PathBuf::from("/ws/toron")
        );
        assert!(
            resolve_repo(&root(), "flywheel", "/etc/passwd").is_err(),
            "absolute paths outside the workspace are refused"
        );
        assert!(
            resolve_repo(&root(), "flywheel", "../../etc").is_err(),
            "more than one .. is refused"
        );
    }

    struct NoAncestor;
    impl ReceiptChecks for NoAncestor {
        fn bead_exists(&self, _id: &str) -> bool {
            true
        }
        fn commit_exists(&self, _repo: &Path, _sha: &str) -> bool {
            true
        }
        fn is_ancestor(&self, _repo: &Path, _ancestor: &str, _descendant: &str) -> bool {
            false
        }
    }

    #[test]
    fn a_fence_that_does_not_prove_a_bead_is_refused() {
        let e = validate(&valid_raw(), &root(), "flywheel", &NoAncestor).expect_err("stale fence");
        assert_eq!(e.field, "fence.sha");
    }
}

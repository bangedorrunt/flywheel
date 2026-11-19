//! Typed crew receipts (bd-bi0mv).
//!
//! A receipt the captain acts on is a validated schema, not prose. Crews close
//! a brief with either free text (kept, marked untyped) or a
//! `flywheel-crew-receipt.v1` JSON document that names its beads, commits, the
//! green fence and the proof tail. Parsing and structural validation are pure;
//! existence checks (git, br) are injected so the core is testable without a
//! live repo.

use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "flywheel-crew-receipt.v1";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRef {
    pub repo: String,
    pub sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fence {
    pub sha: String,
    pub verdict: String,
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
    pub beads: Vec<String>,
    pub commits: Vec<CommitRef>,
    pub fence: Fence,
    pub tests: Tests,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutations: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// A typed error naming the exact field that failed validation.
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

/// Parse a typed receipt from JSON. Malformed JSON names the `json` field.
pub fn parse(raw: &str) -> Result<CrewReceipt, FieldError> {
    serde_json::from_str(raw).map_err(|e| err("json", &e.to_string()))
}

/// Pure structural validation. No repo or ledger access.
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
        if b.trim().is_empty() {
            return Err(err(&format!("beads[{i}]"), "must not be empty"));
        }
    }
    if r.commits.is_empty() {
        return Err(err("commits", "must name at least one commit"));
    }
    if r.commits.len() != r.beads.len() {
        return Err(err(
            "commits",
            "one commit per bead is required for crew land",
        ));
    }
    for (i, c) in r.commits.iter().enumerate() {
        if c.repo.trim().is_empty() {
            return Err(err(&format!("commits[{i}].repo"), "must not be empty"));
        }
        if !is_sha40(&c.sha) {
            return Err(err(
                &format!("commits[{i}].sha"),
                "must be a bare 40-hex commit sha",
            ));
        }
    }
    if !is_sha40(&r.fence.sha) {
        return Err(err("fence.sha", "must be a bare 40-hex commit sha"));
    }
    if r.fence.verdict != "green" {
        return Err(err(
            "fence.verdict",
            &format!("expected green, got {}", r.fence.verdict),
        ));
    }
    if r.tests.cmd.trim().is_empty() {
        return Err(err("tests.cmd", "must not be empty"));
    }
    Ok(())
}

/// Existence validation: beads exist in the tracker and commits exist in
/// their named repos. Both checks are injected so tests need no live repo.
pub fn validate_existence<F, G>(
    r: &CrewReceipt,
    bead_exists: F,
    sha_exists: G,
) -> Result<(), FieldError>
where
    F: Fn(&str) -> bool,
    G: Fn(&str, &str) -> bool,
{
    for (i, b) in r.beads.iter().enumerate() {
        if !bead_exists(b) {
            return Err(err(&format!("beads[{i}]"), &format!("no such bead {b}")));
        }
    }
    for (i, c) in r.commits.iter().enumerate() {
        if !sha_exists(&c.repo, &c.sha) {
            return Err(err(
                &format!("commits[{i}].sha"),
                &format!("{} not found in repo {}", c.sha, c.repo),
            ));
        }
    }
    Ok(())
}

/// Parse + validate structure + existence in one pass.
pub fn validate<F, G>(raw: &str, bead_exists: F, sha_exists: G) -> Result<CrewReceipt, FieldError>
where
    F: Fn(&str) -> bool,
    G: Fn(&str, &str) -> bool,
{
    let r = parse(raw)?;
    validate_structure(&r)?;
    validate_existence(&r, bead_exists, sha_exists)?;
    Ok(r)
}

/// Canonical JSON for storage and the captain wake.
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
                "beads": ["bd-a", "bd-b"],
                "commits": [
                    {{"repo": ".", "sha": "{}"}},
                    {{"repo": "../toron", "sha": "{}"}}
                ],
                "fence": {{"sha": "{}", "verdict": "green"}},
                "tests": {{"cmd": "cargo test", "tail": "ok"}}
            }}"#,
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(40),
        )
    }

    fn all_exists(_repo: &str, _sha: &str) -> bool {
        true
    }
    fn no_bead(_id: &str) -> bool {
        true
    }

    #[test]
    fn a_valid_receipt_parses_and_validates() {
        let r = validate(&valid_raw(), no_bead, all_exists).expect("valid");
        assert_eq!(r.beads, vec!["bd-a", "bd-b"]);
        assert_eq!(r.fence.verdict, "green");
    }

    #[test]
    fn a_bad_field_names_itself_in_the_error() {
        let cases: Vec<(&str, String, &str)> = vec![
            ("schema", valid_raw().replace(SCHEMA, "wrong.v1"), "schema"),
            ("bead", valid_raw().replace("\"bd-a\"", "\"\""), "beads[0]"),
            (
                "commit sha",
                valid_raw().replace(&"a".repeat(40), "short"),
                "commits[0].sha",
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
            let e = validate(&raw, no_bead, all_exists).expect_err(label);
            assert_eq!(e.field, field, "{label}: {e}");
        }
    }

    #[test]
    fn malformed_json_names_the_json_field() {
        let e = validate("not json", no_bead, all_exists).expect_err("bad json");
        assert_eq!(e.field, "json");
    }

    #[test]
    fn missing_bead_and_missing_commit_name_their_fields() {
        let e = validate(&valid_raw(), |id| id != "bd-b", all_exists).expect_err("missing bead");
        assert_eq!(e.field, "beads[1]");
        let missing = "b".repeat(40);
        let e = validate(&valid_raw(), no_bead, |_repo, sha| sha != missing.as_str())
            .expect_err("missing commit");
        assert_eq!(e.field, "commits[1].sha");
    }
}

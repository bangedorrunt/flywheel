//! ADR-0022 §4.5 phase completion contracts (bd-cvv9): pure, machine-
//! checkable evidence checks for the five lifecycle phases. "Evidence over
//! claims" — no phase transition keys on an agent SAYING done; it keys on
//! the report carrying the contract fields, and (for plan) on the
//! runner-verified `dep_cycles` injected from `br dep cycles --json`.
//!
//! Input shape: the dispatch step's output value. A headless leaf returns
//! `{"response": "<agent stdout>"}`; the report is that stdout parsed as
//! JSON (an agent replying with a bare JSON object). A pane receipt or
//! prose reply is NOT a report — the contract fails loudly, never passes on
//! a missing witness.
//!
//! governed-by: ADR-0022 §4, §4.5, §3.3

/// One contract evaluation: phase, pass/fail, and the named evidence
/// (what was checked and what was missing — one line, citable in gate
/// events and the runs board).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseContractReport {
    pub phase: &'static str,
    pub ok: bool,
    pub evidence: String,
}

impl PhaseContractReport {
    fn ok(phase: &'static str, evidence: String) -> Self {
        Self {
            phase,
            ok: true,
            evidence,
        }
    }

    fn fail(phase: &'static str, evidence: String) -> Self {
        Self {
            phase,
            ok: false,
            evidence,
        }
    }
}

/// The review verdict vocabulary (§4 review row): a closed set.
pub const REVIEW_VERDICTS: [&str; 3] = ["AGREE", "AGREE-WITH-CHANGES", "REJECT"];

/// Extract the machine-readable report from a dispatch step output.
/// `{"response": "<json>"}` (headless leaf) parses the inner JSON; an
/// already-object value passes through; anything else is not a report.
pub fn agent_report(step_out: &serde_json::Value) -> Option<serde_json::Value> {
    // The headless leaf wraps agent stdout in `response` — that inner JSON
    // is the report (the wrapper itself never is). Bare objects/strings
    // pass through for direct-report callers (tests, pane reports).
    if let Some(r) = step_out.get("response").and_then(serde_json::Value::as_str) {
        return serde_json::from_str(r.trim()).ok();
    }
    match step_out {
        serde_json::Value::Object(_) => Some(step_out.clone()),
        serde_json::Value::String(s) => serde_json::from_str(s.trim()).ok(),
        _ => None,
    }
}

fn non_empty_str(v: &serde_json::Value) -> Option<&str> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty())
}

/// A non-empty field: a non-blank string, or a non-empty array of
/// non-blank strings (non_goals / open_questions read naturally as lists).
fn non_empty_field(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::String(s) => !s.trim().is_empty(),
        serde_json::Value::Array(items) => !items.is_empty(),
        _ => false,
    }
}

/// §4 resolve: goal statement carries target, non_goals, blast_radius,
/// open_questions — all four present (open_questions may be an empty
/// array: "no open questions" is a legitimate answer; the FIELD must exist).
pub fn resolve_contract(report: &serde_json::Value) -> PhaseContractReport {
    let mut missing: Vec<&str> = Vec::new();
    for field in ["target", "non_goals", "blast_radius"] {
        if !report.get(field).is_some_and(non_empty_field) {
            missing.push(field);
        }
    }
    if !report.get("open_questions").is_some_and(|v| {
        v.is_array() || non_empty_str(v).is_some() || matches!(v, serde_json::Value::Null)
    }) || report.get("open_questions").is_none()
    {
        missing.push("open_questions");
    }
    if missing.is_empty() {
        PhaseContractReport::ok(
            "resolve",
            "goal statement carries target, non_goals, blast_radius, open_questions".into(),
        )
    } else {
        PhaseContractReport::fail(
            "resolve",
            format!("goal statement missing/empty: {}", missing.join(", ")),
        )
    }
}

/// §4 plan: ≥1 bead with a non-empty acceptance, and the runner-verified
/// `dep_cycles` is 0 (`br dep cycles --json` count, injected by the chain
/// runner — the agent does not self-certify acyclicity).
pub fn plan_contract(report: &serde_json::Value) -> PhaseContractReport {
    let Some(beads) = report.get("beads").and_then(serde_json::Value::as_array) else {
        return PhaseContractReport::fail("plan", "no beads array in the report".into());
    };
    if beads.is_empty() {
        return PhaseContractReport::fail(
            "plan",
            "beads array is empty (no work decomposed)".into(),
        );
    }
    let mut bad: Vec<String> = Vec::new();
    for (i, b) in beads.iter().enumerate() {
        let id_ok = b.get("id").is_some_and(|v| non_empty_str(v).is_some());
        let acc_ok = b.get("acceptance").is_some_and(non_empty_field);
        if !id_ok || !acc_ok {
            bad.push(format!(
                "bead[{i}]{}",
                if id_ok {
                    ": acceptance empty"
                } else {
                    ": id missing"
                }
            ));
        }
    }
    let cycles = report.get("dep_cycles").and_then(serde_json::Value::as_u64);
    match cycles {
        Some(0) | None if bad.is_empty() && cycles.is_some() => {}
        _ => {}
    }
    if !bad.is_empty() {
        return PhaseContractReport::fail(
            "plan",
            format!("bead contract violations: {}", bad.join("; ")),
        );
    }
    match cycles {
        Some(0) => PhaseContractReport::ok(
            "plan",
            format!(
                "{} bead(s) with acceptance; dep_cycles=0 (br-verified)",
                beads.len()
            ),
        ),
        Some(n) => PhaseContractReport::fail(
            "plan",
            format!("dep_cycles={n}: the created set has cycles"),
        ),
        None => PhaseContractReport::fail(
            "plan",
            "dep_cycles absent — the chain runner must inject `br dep cycles --json`".into(),
        ),
    }
}

/// A bead id as it appears in commit messages (`bd-cvv9`, `bd-lease-liveness-bo0g`).
pub fn cites_bead_id(text: &str) -> bool {
    // bd- followed by 3+ [a-z0-9] — loose on purpose: message-cited ids are
    // evidence of linkage, not of format policing.
    let mut rest = text;
    while let Some(i) = rest.find("bd-") {
        let tail = &rest[i + 3..];
        let n = tail
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            .count();
        if n >= 3 {
            return true;
        }
        rest = &rest[i + 3..];
    }
    false
}

/// §4 implement: a commit whose message cites the bead id, and the bead's
/// named gate command exited 0 with output recorded.
pub fn implement_contract(report: &serde_json::Value) -> PhaseContractReport {
    let Some(commit) = report.get("commit_sha").and_then(|v| non_empty_str(v)) else {
        return PhaseContractReport::fail("implement", "no commit_sha in the report".into());
    };
    let message = report
        .get("commit_message")
        .and_then(|v| non_empty_str(v))
        .unwrap_or("");
    if !cites_bead_id(message) {
        return PhaseContractReport::fail(
            "implement",
            format!("commit_message does not cite a bead id (bd-…): {message:?}"),
        );
    }
    let gate_ok = report.get("gate").is_some_and(|g| {
        g.get("exit").and_then(serde_json::Value::as_i64) == Some(0)
            && g.get("command").is_some_and(|c| non_empty_str(c).is_some())
            && g.get("output").is_some()
    });
    if !gate_ok {
        return PhaseContractReport::fail(
            "implement",
            "gate incomplete: needs {command, exit: 0, output} on the bead's acceptance command"
                .into(),
        );
    }
    PhaseContractReport::ok(
        "implement",
        format!("commit {commit} cites a bead id; gate exit 0"),
    )
}

/// §4 review: findings posted (non-empty) and a verdict from the closed
/// vocabulary recorded.
pub fn review_contract(report: &serde_json::Value) -> PhaseContractReport {
    let findings_ok = report
        .get("findings")
        .is_some_and(|f| f.as_array().is_some_and(|a| !a.is_empty()));
    if !findings_ok {
        return PhaseContractReport::fail("review", "no findings array (or empty)".into());
    }
    let verdict = report.get("verdict").and_then(|v| non_empty_str(v));
    match verdict {
        Some(v) if REVIEW_VERDICTS.contains(&v) => {
            PhaseContractReport::ok("review", format!("findings posted; verdict {v}"))
        }
        Some(other) => PhaseContractReport::fail(
            "review",
            format!("verdict {other:?} not in {:?}", REVIEW_VERDICTS),
        ),
        None => PhaseContractReport::fail("review", "no verdict recorded".into()),
    }
}

/// §4 verify: the named gate command exited 0 and its output is recorded.
pub fn verify_contract(report: &serde_json::Value) -> PhaseContractReport {
    let Some(command) = report
        .get("gate")
        .and_then(|g| g.get("command"))
        .and_then(|c| non_empty_str(c))
    else {
        return PhaseContractReport::fail("verify", "no gate.command in the report".into());
    };
    let exit = report
        .get("gate")
        .and_then(|g| g.get("exit"))
        .and_then(serde_json::Value::as_i64);
    let output_recorded = report
        .get("gate")
        .and_then(|g| g.get("output"))
        .is_some_and(|o| !o.is_null());
    match exit {
        Some(0) if output_recorded => PhaseContractReport::ok(
            "verify",
            format!("gate `{command}` exit 0; output recorded"),
        ),
        Some(code) => PhaseContractReport::fail(
            "verify",
            format!("gate `{command}` exit {code} — contract not proven"),
        ),
        None => PhaseContractReport::fail("verify", "gate.exit absent".into()),
    }
}

/// Evaluate the §4.5 contract for a phase over a dispatch step output.
/// Unknown phases have no contract (the caller never gates them); a
/// missing/unspeakable report fails every contract loudly.
pub fn check_phase(phase: &str, step_out: &serde_json::Value) -> PhaseContractReport {
    let Some(report) = agent_report(step_out) else {
        return PhaseContractReport::fail(
            "phase",
            format!("no machine-readable report for phase `{phase}` (response is not JSON)"),
        );
    };
    match phase {
        "resolve" => resolve_contract(&report),
        "plan" => plan_contract(&report),
        "implement" => implement_contract(&report),
        "review" => review_contract(&report),
        "verify" => verify_contract(&report),
        other => {
            PhaseContractReport::fail("phase", format!("no §4.5 contract for phase `{other}`"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- agent_report -------------------------------------------------

    #[test]
    fn agent_report_parses_headless_response_string() {
        let out = json!({ "response": r#"{"target":"x"}"#, "ok": true });
        let report = agent_report(&out).expect("parses");
        assert_eq!(report["target"], "x");
    }

    #[test]
    fn agent_report_rejects_prose() {
        assert!(agent_report(&json!({ "response": "I am done, trust me" })).is_none());
    }

    // ---- resolve: passing + failing witnesses --------------------------

    #[test]
    fn resolve_contract_passing_witness() {
        let r = resolve_contract(&json!({
            "target": "fix the flake",
            "non_goals": ["no rewrite"],
            "blast_radius": "flywheel loop",
            "open_questions": []
        }));
        assert!(r.ok, "{}", r.evidence);
    }

    #[test]
    fn resolve_contract_failing_witness_names_every_missing_field() {
        let r = resolve_contract(&json!({ "target": "  " }));
        assert!(!r.ok);
        assert!(r.evidence.contains("target"), "{}", r.evidence);
        assert!(r.evidence.contains("non_goals"), "{}", r.evidence);
        assert!(r.evidence.contains("blast_radius"), "{}", r.evidence);
        assert!(r.evidence.contains("open_questions"), "{}", r.evidence);
    }

    // ---- plan ----------------------------------------------------------

    #[test]
    fn plan_contract_passing_witness() {
        let r = plan_contract(&json!({
            "beads": [ { "id": "bd-abc1", "acceptance": "cargo test -p flywall" } ],
            "dep_cycles": 0
        }));
        assert!(r.ok, "{}", r.evidence);
        assert!(r.evidence.contains("br-verified"), "{}", r.evidence);
    }

    #[test]
    fn plan_contract_failing_witnesses() {
        // no beads
        let r = plan_contract(&json!({ "beads": [], "dep_cycles": 0 }));
        assert!(!r.ok && r.evidence.contains("empty"));
        // bead without acceptance
        let r = plan_contract(&json!({
            "beads": [ { "id": "bd-abc1", "acceptance": " " } ], "dep_cycles": 0
        }));
        assert!(!r.ok && r.evidence.contains("acceptance"), "{}", r.evidence);
        // cycles present
        let r = plan_contract(&json!({
            "beads": [ { "id": "bd-abc1", "acceptance": "x" } ], "dep_cycles": 2
        }));
        assert!(
            !r.ok && r.evidence.contains("dep_cycles=2"),
            "{}",
            r.evidence
        );
        // dep_cycles not injected (agent self-certification is not evidence)
        let r = plan_contract(&json!({
            "beads": [ { "id": "bd-abc1", "acceptance": "x" } ]
        }));
        assert!(!r.ok && r.evidence.contains("inject"), "{}", r.evidence);
    }

    // ---- implement -----------------------------------------------------

    #[test]
    fn implement_contract_passing_witness() {
        let r = implement_contract(&json!({
            "commit_sha": "abc123",
            "commit_message": "fix(loop): no second GOAL (bd-dispatch-dedupe-2zlz)",
            "gate": { "command": "cargo test -p flywheel loop", "exit": 0, "output": "ok. 3 passed" }
        }));
        assert!(r.ok, "{}", r.evidence);
    }

    #[test]
    fn implement_contract_failing_witnesses() {
        // commit does not cite a bead id
        let r = implement_contract(&json!({
            "commit_sha": "abc123", "commit_message": "fix the thing",
            "gate": { "command": "true", "exit": 0, "output": "" }
        }));
        assert!(
            !r.ok && r.evidence.contains("does not cite"),
            "{}",
            r.evidence
        );
        // gate non-zero / missing output
        let r = implement_contract(&json!({
            "commit_sha": "abc123", "commit_message": "fix (bd-abc1)",
            "gate": { "command": "cargo test", "exit": 101 }
        }));
        assert!(
            !r.ok && r.evidence.contains("gate incomplete"),
            "{}",
            r.evidence
        );
        // no commit at all
        let r = implement_contract(&json!({ "gate": { "command": "true", "exit": 0 } }));
        assert!(!r.ok && r.evidence.contains("commit_sha"));
    }

    #[test]
    fn bead_id_citation_table() {
        assert!(cites_bead_id("fix: bd-cvv9 done"));
        assert!(cites_bead_id("bd-lease-liveness-bo0g"));
        assert!(!cites_bead_id("no id here"));
        assert!(!cites_bead_id("bd- is not an id"));
        assert!(!cites_bead_id("CBD-123 not lower"));
    }

    // ---- review --------------------------------------------------------

    #[test]
    fn review_contract_passing_and_failing_witnesses() {
        let ok = review_contract(&json!({
            "findings": [ "race in doorbell watcher" ], "verdict": "AGREE-WITH-CHANGES"
        }));
        assert!(ok.ok, "{}", ok.evidence);

        let bad_verdict = review_contract(&json!({
            "findings": [ "x" ], "verdict": "looks good"
        }));
        assert!(
            !bad_verdict.ok && bad_verdict.evidence.contains("not in"),
            "{}",
            bad_verdict.evidence
        );

        let no_findings = review_contract(&json!({ "verdict": "AGREE" }));
        assert!(!no_findings.ok && no_findings.evidence.contains("findings"));
    }

    // ---- verify --------------------------------------------------------

    #[test]
    fn verify_contract_passing_and_failing_witnesses() {
        let ok = verify_contract(&json!({
            "gate": { "command": "bash scripts/verify-gates.sh", "exit": 0, "output": "all green" }
        }));
        assert!(ok.ok, "{}", ok.evidence);

        let failed = verify_contract(&json!({
            "gate": { "command": "cargo test", "exit": 1, "output": "1 failed" }
        }));
        assert!(
            !failed.ok && failed.evidence.contains("exit 1"),
            "{}",
            failed.evidence
        );

        let no_output = verify_contract(&json!({
            "gate": { "command": "cargo test", "exit": 0 }
        }));
        assert!(!no_output.ok, "output must be recorded");
    }

    // ---- dispatch table + report extraction ----------------------------

    #[test]
    fn check_phase_end_to_end_over_headless_leaf_shape() {
        let step_out = json!({ "ok": true,
            "response": r#"{"target":"t","non_goals":["n"],"blast_radius":"b","open_questions":[]}"# });
        let r = check_phase("resolve", &step_out);
        assert!(r.ok, "{}", r.evidence);
        assert_eq!(r.phase, "resolve");
    }

    #[test]
    fn check_phase_fails_loudly_on_prose_and_unknown_phase() {
        let prose = check_phase("implement", &json!({ "response": "done, trust me" }));
        assert!(!prose.ok && prose.evidence.contains("no machine-readable report"));
        let unknown = check_phase("brainstorm", &json!({ "x": 1 }));
        assert!(!unknown.ok && unknown.evidence.contains("no §4.5 contract"));
    }
}

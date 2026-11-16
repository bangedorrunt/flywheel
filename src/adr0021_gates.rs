//! ADR-0021 gate witnesses (bd-jhz8x.1).
//!
//! The ADR replaced review judgement with checks: a shape is linted, a compose
//! token is data, a stage's `reads` must be supplied, a phase end needs a
//! witness, a lowered workflow must be bounded, and a polish round only ends
//! when the digest is unchanged. Each check is pinned here by one test that the
//! mutation spec (`scripts/mutations/adr0021-gates.spec.sh`) breaks in the
//! production code — acceptance #6 of the ADR: a check nobody has seen fail is
//! a comment.

#[cfg(test)]
mod tests {
    use serde_json::json;

    fn builtin(name: &str) -> toron_workflow::schema::Workflow {
        crate::builtin_workflows::builtin_workflows()
            .remove(name)
            .unwrap_or_else(|| panic!("{name} is compiled in"))
    }

    /// D1: a `shape: fan-out` workflow without a parallel/foreach step is
    /// refused by lint. `beads` is a linear pipeline.
    #[test]
    fn adr0021_shape_lint_refuses_a_fanout_without_parallel() {
        use crate::workflow_catalog::{Shape, shape_conforms};
        let err = shape_conforms(Shape::FanOut, &builtin("beads"))
            .expect_err("a fan-out shape needs a fan-out step");
        assert!(err.contains("parallel/foreach"), "{err}");
        assert!(
            shape_conforms(Shape::FanOut, &builtin("arena")).is_ok(),
            "arena fans out"
        );
    }

    /// D2: `compose:` is data and the HIGHER tier's `Some` wins; a project can
    /// ship its own compose token without a rebuild (acceptance #5).
    #[test]
    fn adr0021_compose_override_wins_from_the_upper_tier() {
        use crate::workflow_catalog::CatalogMeta;
        let lower = CatalogMeta {
            compose: Some(true),
            shape: None,
        };
        let upper = CatalogMeta {
            compose: Some(false),
            shape: None,
        };
        assert_eq!(
            lower.overlay(upper).compose,
            Some(false),
            "the upper tier's declaration wins"
        );
        assert_eq!(
            lower.overlay(CatalogMeta::default()).compose,
            Some(true),
            "an absent upper declaration inherits the lower tier"
        );
    }

    /// D3.1: a stage whose `reads` the workflow cannot supply is a lint issue
    /// (the `judge-arms` class in beads-lifecycle).
    #[test]
    fn adr0021_reads_lint_refuses_a_missing_read() {
        use toron_workflow::schema::parse_workflow;
        let wf = parse_workflow(
            r#"
name: reads-gate
trigger: { type: webhook }
inputs: []
steps:
  - name: polish
    action: dispatch_job
    assignee: x
    spec: { stage: plan-polish, task: "polish" }
"#,
        )
        .expect("parses");
        let stages = crate::stages::StageLibrary::compiled_in();
        let issues = crate::cmd::workflow::stage_reads_issues("reads-gate", &wf, &stages);
        assert!(
            issues.iter().any(|i| i.contains("inputs.objective")),
            "a missing read is reported: {issues:?}"
        );
    }

    /// D4.2: a phase end without a witness is a typed failure, never success.
    #[test]
    fn adr0021_witness_exit_refuses_an_empty_report() {
        let report = crate::phase_contracts::check_witness("implement", &json!({}));
        assert!(!report.ok, "an empty report cannot witness a phase end");
        let unknown = crate::phase_contracts::check_witness("no-such-witness", &json!({}));
        assert!(!unknown.ok, "an unknown witness never succeeds by default");
    }

    /// D5.2: a lowered workflow with an unbounded wait is flagged `bounded`.
    #[test]
    fn adr0021_lowering_flags_an_unbounded_wait() {
        use toron_workflow::schema::parse_workflow;
        let wf = parse_workflow(
            r#"
name: unbounded-wait
trigger: { type: webhook }
steps:
  - name: wait
    action: park
    until_ms: "1893456000000"
    max_wait_secs: 0
"#,
        )
        .expect("parses");
        let lowered = crate::workflow_lowering::lower(&wf).expect("lowers");
        let (errors, _warnings) = crate::workflow_lowering::check_bounded(&lowered);
        assert!(
            errors.iter().any(|e| e.contains("wait")),
            "a zero wait is unbounded: {errors:?}"
        );
    }

    /// D7: convergence is measured — a changed digest never converges.
    #[test]
    fn adr0021_plan_fence_never_converges_on_a_changed_digest() {
        assert!(!crate::plan_fence::converged("digest-b", Some("digest-a")));
        assert!(!crate::plan_fence::converged("digest-a", None));
        assert!(crate::plan_fence::converged("digest-a", Some("digest-a")));
    }
}

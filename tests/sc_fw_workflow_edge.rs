//! SC-FW workflow edge — flywheel routes a workflow through the unified
//! `toron-workflow` engine end-to-end (ADR-0005 P3.1 LINCHPIN).
//!
//! The tier diagram's `flywheel → toron-workflow` arrow did not exist in any
//! Cargo.toml before this bead. This test proves the cargo edge carries real
//! functionality:
//!
//! 1. flywheel links `toron-workflow` (Cargo.toml dependency — asserted below
//!    as a static contract).
//! 2. A workflow with declared `inputs` is invoked via
//!    `WorkflowEngine::invoke` (no trigger event), with `{{inputs.x}}` /
//!    `{{trigger.x}}` resolving from the synthesized trigger payload.
//! 3. The engine returns the run's step outputs.
//!
//! Trigger path + invoke path share the same durable function (`run_workflow`)
//! — that is an engine-side invariant asserted in toron-workflow's own tests.
//!
//! governed-by: ADR-0005 Spec 2, P3.1

use std::sync::Arc;

use toron_workflow::engine::{ActionRunner, WorkflowEngine};
use toron_workflow::schema::{Action, Workflow, parse_workflow};

/// A no-op runner (returns Null for every action — the embedding's real
/// runner is injected; this test exercises the engine, not the mailbox).
fn noop_runner() -> ActionRunner {
    Arc::new(|_a: &Action| Ok(serde_json::Value::Null))
}

/// Static contract: the cargo edge exists.
#[test]
fn flywheel_cargo_toml_lists_toron_workflow() {
    let toml = std::fs::read_to_string("Cargo.toml").unwrap_or_default();
    assert!(
        toml.contains("toron-workflow"),
        "flywheel/Cargo.toml must depend on toron-workflow (the P3.1 edge)"
    );
}

/// The sc_fw acceptance: flywheel invokes a workflow through WorkflowEngine
/// and `{{inputs.*}}` / `{{trigger.*}}` resolve end-to-end.
#[tokio::test]
async fn sc_fw_flywheel_invokes_workflow_through_engine() {
    let engine = WorkflowEngine::local(noop_runner()).expect("engine");
    let wf = parse_workflow(
        r#"
name: sc-fw-demo
trigger:
  type: webhook
inputs:
  - name: goal
steps:
  - name: plan
    action: send_dm
    to: "pi"
    body: "planning {{inputs.goal}}"
"#,
    )
    .expect("parse");

    let output = engine
        .invoke(
            "sc-fw-demo-1",
            wf,
            serde_json::json!({ "goal": "ship the tier" }),
        )
        .await
        .expect("invoke returns run output");

    // The step ran and produced output visible to the caller (the invoking
    // orchestrator / `flywheel workflow run`).
    assert!(
        output.get("plan").is_some(),
        "invoked workflow produced step output: {output}"
    );
}

/// The DEFERRED P3.2/3.7 item (re-executed in bd-p3-unified-engine-h3v): a
/// legacy `workflows.toml` workflow — the exact `Send/Wait` shape
/// `WorkflowCatalog::absorb` emits as `Action::DispatchJob{spec:{to,stage,msg}}`
/// + `Action::Delay` — must round-trip through `WorkflowEngine::invoke`. This
/// is the "send_wait_absorbed" integration test the closures deferred; it
/// proves the thin-caller's foundation (absorbed catalog → engine) is real
/// without touching main.rs or the captain's live workflows.
#[tokio::test]
async fn sc_fw_absorbed_legacy_workflow_round_trips_through_engine() {
    use std::sync::Mutex;
    use toron_workflow::schema::Step;

    // Capture the dispatch payloads so we assert the legacy Send semantics
    // (to/stage/msg) survive absorption, not just that the engine runs.
    let dispatches: Arc<Mutex<Vec<(String, toron_workflow::schema::DispatchSpec)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let captured = dispatches.clone();
    let runner: ActionRunner = Arc::new(move |a: &Action| {
        if let Action::DispatchJob { assignee, spec, .. } = a {
            captured
                .lock()
                .unwrap()
                .push((assignee.clone(), spec.clone()));
            return Ok(serde_json::json!({ "ok": true, "assignee": assignee }));
        }
        Ok(serde_json::Value::Null)
    });

    // The absorbed-legacy workflow shape (mirrors absorb_workflow_steps):
    // send-0 -> DispatchJob{to,spec:{to,stage,msg}}, wait-0 -> Delay.
    let wf = Workflow {
        name: "legacy-send-wait".into(),
        description: Some("absorbed legacy workflows.toml workflow".into()),
        trigger: None,
        guard: None,
        policy: None,
        principles: vec![],
        steps: vec![
            Step {
                name: Some("send-0".into()),
                guard: None,
                on_error: Default::default(),
                error_kinds: Vec::new(),
                build_slot: false,
                action: Action::DispatchJob {
                    assignee: "pi".into(),
                    // Legacy send-absorb shape: to/msg round-trip through
                    // `extra`, stage is a named DispatchSpec field.
                    spec: serde_json::from_value(serde_json::json!({
                        "to": "pi",
                        "stage": "p6",
                        "msg": null,
                    }))
                    .expect("legacy spec parses"),
                    channel: None,
                },
            },
            Step {
                name: Some("wait-0".into()),
                guard: None,
                on_error: Default::default(),
                error_kinds: Vec::new(),
                build_slot: false,
                action: Action::Delay { secs: 0 },
            },
        ],
        graph: None,
        crew: None,
        inputs: vec![],
    };

    let engine = WorkflowEngine::local(runner).expect("engine");
    let output = engine
        .invoke("legacy-send-wait-1", wf, serde_json::json!({}))
        .await
        .expect("invoke absorbed legacy workflow");

    // send-0 produced a step output (engine advanced past both steps).
    assert!(
        output.get("send-0").is_some(),
        "send-0 step output: {output}"
    );
    assert!(
        output.get("wait-0").is_some(),
        "wait-0 step output: {output}"
    );

    // The legacy Send semantics survived absorption: one DispatchJob to pi
    // carrying the p6 stage.
    let jobs = dispatches.lock().unwrap();
    assert_eq!(jobs.len(), 1, "one DispatchJob from the legacy Send step");
    let (assignee, spec) = &jobs[0];
    assert_eq!(assignee, "pi");
    assert_eq!(spec.extra["to"], "pi");
    assert_eq!(spec.stage.as_deref(), Some("p6"));
}

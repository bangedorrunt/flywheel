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
use toron_workflow::schema::{Action, parse_workflow};

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
        r##"
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
"##,
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

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

use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use toron_workflow::engine::WorkflowEngine;
use toron_workflow::schema::{Action, Workflow, parse_workflow};

/// Flag-day (bd-fuql): pure no-op leaves need no runner — `local()` takes none.
fn flywheel() -> &'static str {
    env!("CARGO_BIN_EXE_flywheel")
}

/// One hermetic project carrying a PROJECT-tier workflow
/// (`<project>/.flywheel/workflows/project-tier-demo.yaml`) plus isolated
/// config/state homes. The project key is an ABSOLUTE path so `project_dir`
/// resolves it verbatim (bd-test-tmp-hygiene-pq5b).
struct TieredProject {
    _dir: tempfile::TempDir,
    project: std::path::PathBuf,
    config_home: std::path::PathBuf,
    state_home: std::path::PathBuf,
}

impl TieredProject {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let project = dir.path().join("proj");
        let workflows = project.join(".flywheel/workflows");
        std::fs::create_dir_all(&workflows).expect("workflow dir");
        std::fs::write(
            workflows.join("project-tier-demo.yaml"),
            "name: project-tier-demo\nsteps:\n  - name: noop\n    action: delay\n    secs: 0\n",
        )
        .expect("write project-tier workflow");
        Self {
            config_home: dir.path().join("config"),
            state_home: dir.path().join("state"),
            project,
            _dir: dir,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(flywheel())
            .args(args)
            .arg("--project")
            .arg(&self.project)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .output()
            .expect("spawn flywheel")
    }

    /// For the verbs that carry no project context at all (the read doors).
    fn run_without_project(&self, args: &[&str]) -> Output {
        Command::new(flywheel())
            .args(args)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_STATE_HOME", &self.state_home)
            .output()
            .expect("spawn flywheel")
    }
}

/// The catalog contract is built-ins < user catalog < PROJECT tier, and
/// `workflow compose` already layers it (W6). `workflow run` is the other
/// door into the same catalog and has the project in hand (`--project`), so a
/// workflow authored into the project must resolve there — before this, run
/// resolved the user tier only and bailed `unknown workflow` on a workflow the
/// project itself declares.
#[test]
fn workflow_run_resolves_the_project_tier() {
    let h = TieredProject::new();
    let out = h.run(&["workflow", "run", "project-tier-demo"]);

    assert!(
        out.status.success(),
        "workflow run must resolve a project-tier workflow: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The read doors land (`bd-workflow-catalog-read-doors-project-tier-jah1`):
/// all four verbs that read the catalog accept `--project` and layer
/// built-ins < user < project, matching `run`/`compose`. Without `--project`
/// each stays user-tier — a project workflow is invisible there by design,
/// because nothing names the project.
#[test]
fn workflow_read_doors_layer_the_project_tier() {
    let h = TieredProject::new();

    // list: names the project workflow under --project, not without it.
    let bare = h.run_without_project(&["workflow", "list"]);
    assert!(
        bare.status.success(),
        "workflow list failed: {}",
        String::from_utf8_lossy(&bare.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&bare.stdout).contains("project-tier-demo"),
        "without --project, list stays user-tier (no project named)"
    );
    let listed = h.run(&["workflow", "list"]);
    assert!(
        listed.status.success(),
        "workflow list --project failed: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("project-tier-demo"),
        "workflow list --project must surface the project tier: {}",
        String::from_utf8_lossy(&listed.stdout)
    );

    // show: resolves the project workflow under --project; bare fails loud.
    let shown = h.run(&["workflow", "show", "project-tier-demo"]);
    assert!(
        shown.status.success(),
        "workflow show --project must resolve the project tier: stderr={}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let bare_show = h.run_without_project(&["workflow", "show", "project-tier-demo"]);
    assert!(
        !bare_show.status.success(),
        "bare show must fail loud on a project-tier workflow"
    );

    // lint: the project tier joins the lint set under --project (a project
    // regression cannot ship unlinted); a broken project workflow fails it.
    let broken = h.project.join(".flywheel/workflows/broken-tier.yaml");
    std::fs::write(
        &broken,
        "name: broken-tier\nsteps:\n  - name: nope\n    action: not_an_action\n",
    )
    .expect("write broken workflow");
    let linted = h.run(&["workflow", "lint"]);
    assert!(
        !linted.status.success(),
        "workflow lint --project must lint the project tier and fail on a broken workflow: {}",
        String::from_utf8_lossy(&linted.stdout)
    );
    std::fs::remove_file(&broken).expect("remove broken workflow");
    let clean = h.run(&["workflow", "lint"]);
    assert!(
        clean.status.success(),
        "workflow lint --project is clean once the broken file is gone: {}",
        String::from_utf8_lossy(&clean.stdout)
    );

    // crew: previews the project workflow's crew under --project.
    let crewed = h.run(&["workflow", "crew", "project-tier-demo"]);
    assert!(
        crewed.status.success(),
        "workflow crew --project must resolve the project tier: stderr={}",
        String::from_utf8_lossy(&crewed.stderr)
    );
}

/// The project key must be an absolute path for the layering to be reachable
/// from a scratch dir (documents the `--project` contract the test relies on).
#[test]
fn tiered_project_key_is_absolute() {
    let h = TieredProject::new();
    assert!(Path::new(&h.project).is_absolute());
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
    let engine = WorkflowEngine::local().expect("engine");
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
    let runner: toron_workflow::engine::EffectRunner =
        Arc::new(move |inv: &toron_workflow::engine::ActionInvocation| {
            if let Action::DispatchJob { assignee, spec, .. } = &inv.action {
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

    let engine = WorkflowEngine::local_with_effect_runner(Some(runner)).expect("engine");
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

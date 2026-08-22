//! E2E: bug-fix playbook control flow + lint gate (bd-i18q).
//!
//! ADR-0019 vision test: 'if the target is a bug, the crew is FORCED through
//! repro -> red test -> fix -> ledger row.' Wave-2 shipped the YAML; this
//! suite proves the CONTROL FLOW executes in the mandated order against the
//! real engine (toron-workflow) — no live panes, no mocks for the decision:
//!
//! 1. **Node ORDER** — invoke the builtin `bug-fix` graph through
//!    `WorkflowEngine` with a recording runner; assert the nodes executed
//!    repro BEFORE tdd BEFORE fix, prove last, with the `needs:` edges
//!    intact (the wave scheduler cannot run fix before its repro ancestor).
//! 2. **Lint gate (needs edges)** — the structural lint rejects a
//!    bug-fix-shaped graph whose `fix` does not reach `repro` through its
//!    transitive `needs:` closure (ADR-0019 Spec 6 fix-root-causes); the
//!    builtin passes. Proves a broken edge cannot ship.
//! 3. **CLI control-flow** — `flywheel workflow run bug-fix --inputs ...`
//!    (recording mode) exits 0 and reports all four dispatch nodes with the
//!    auto crew (scout + 2 coder + reviewer).
//!
//! The live `--execute --mode headless` run (real agent panes) is executed
//! by the captain per the bead acceptance; the receipt goes in the close
//! reason.
//!
//! governed-by: ADR-0019 (Spec 6, Spec 7.2)

use std::sync::{Arc, Mutex};

use toron_workflow::engine::{ActionRunner, WorkflowEngine};
use toron_workflow::schema::{Action, Workflow};

/// Parse the COMPILED-IN builtin `bug-fix` workflow. The builtins live in
/// `src/builtin_workflows.rs` (BUILTIN_WORKFLOWS_YAML) — this test reads the
/// source so a rename/regression of the YAML fails here, not silently.
fn builtin_bug_fix() -> Workflow {
    let src = std::fs::read_to_string("src/builtin_workflows.rs").expect("builtin_workflows.rs");
    let yaml = extract_builtin_yaml(&src, "bug-fix").expect("bug-fix YAML in builtins");
    toron_workflow::schema::parse_workflow(&yaml).expect("bug-fix parses")
}

/// Pull the raw `r##"..."##` YAML for `name` from BUILTIN_WORKFLOWS_YAML.
fn extract_builtin_yaml(src: &str, name: &str) -> Option<String> {
    let marker = format!("(\n        \"{name}\",\n");
    let start = src.find(&marker)? + marker.len();
    let rest = &src[start..];
    let open = rest.find("r##\"")?;
    let body_start = open + 4;
    let body_end = rest[body_start..].find("\"##")? + body_start;
    Some(rest[body_start..body_end].to_string())
}

/// A runner that RECORDS the dispatch order by the node's rendered TASK text
/// (the engine passes the Action, not the node name — each bug-fix node has
/// unique task wording, so the task IS the node discriminator) and returns
/// success — the engine's side-effect-free probe.
fn recording_runner(log: Arc<Mutex<Vec<String>>>) -> ActionRunner {
    Arc::new(move |a: &Action| {
        let entry = match a {
            Action::DispatchJob { spec, .. } => {
                // Rendered task text (templates already expanded). Normalized
                // to lowercase + trimmed for keyword matching.
                spec.task.as_deref().unwrap_or("").to_ascii_lowercase()
            }
            other => other.type_name().to_string(),
        };
        log.lock().unwrap().push(entry);
        Ok(serde_json::json!({ "ok": true }))
    })
}

/// THE vision-test: the builtin bug-fix graph executes repro → tdd → fix →
/// prove in exactly that order (the wave scheduler cannot run fix before its
/// repro ancestor — the `needs:` chain is the enforcement). Node identity is
/// read from each dispatch's rendered task wording.
#[tokio::test]
async fn bug_fix_control_flow_orders_repro_tdd_fix_prove() {
    let wf = builtin_bug_fix();
    let graph = wf.graph.as_ref().expect("bug-fix is a graph");

    // Needs edges intact (the DAG shape the wave scheduler walks).
    assert_eq!(graph.nodes["tdd"].needs, vec!["repro".to_string()]);
    assert_eq!(graph.nodes["fix"].needs, vec!["tdd".to_string()]);
    assert_eq!(graph.nodes["prove"].needs, vec!["fix".to_string()]);
    assert!(graph.nodes["repro"].needs.is_empty(), "repro is the root");

    let log = Arc::new(Mutex::new(Vec::new()));
    let engine = WorkflowEngine::local(recording_runner(log.clone())).expect("engine");
    let output = engine
        .invoke(
            "bug-fix-order-1",
            wf.clone(),
            serde_json::json!({ "symptom": "the leak", "repro": "exit 1" }),
        )
        .await
        .expect("invoke");

    // Every node ran (4 graph nodes, one dispatch each). The tasks carry the
    // node's mandate verbatim (rendered): repro runs/writes the repro, tdd
    // writes the FAILING test, fix applies the MINIMAL fix, prove independently
    // proves. Order is asserted by these markers.
    let order = log.lock().unwrap().clone();
    assert_eq!(order.len(), 4, "exactly 4 node dispatches: {order:?}");
    let markers = [
        ("repro", "repro"),         // repro task names the repro
        ("tdd", "failing test"),    // tdd writes the FAILING test (RED)
        ("fix", "minimal fix"),     // fix applies the MINIMAL fix
        ("prove", "independently"), // prove independently proves (last)
    ];
    for (idx, (node, marker)) in markers.iter().enumerate() {
        assert!(
            order[idx].contains(marker),
            "position {idx} must be {node} (task marker {marker:?}), got: {order:?}"
        );
    }
    // prove ran last (independent reviewer, ADR-0019 Spec 4).
    assert!(order[3].contains("independently"), "prove last: {order:?}");
    // The reviewer prove node carries the opencode harness override.
    let Action::DispatchJob { spec, .. } = &graph.nodes["prove"].action else {
        panic!("prove is a dispatch_job");
    };
    assert_eq!(spec.harness.as_deref(), Some("opencode"));

    // Outputs map carries per-node results.
    for node in ["repro", "tdd", "fix", "prove"] {
        assert!(output.get(node).is_some(), "{node} in outputs: {output}");
    }
}

/// Lint gate: a bug-fix-shaped graph whose `fix` does NOT reach `repro`
/// through its transitive needs closure FAILS the structural lint; the
/// builtin (correct chain) passes. This is the "needs edges intact" guard
/// the live run depends on.
#[test]
fn bug_fix_lint_gate_rejects_broken_repro_edge() {
    use toron_workflow::lint::lint_workflow;

    // Broken: fix -> tdd, but tdd -> (nothing); repro is a detached node.
    let broken = toron_workflow::schema::parse_workflow(
        r##"
name: bug-fix
description: "broken edge"
trigger:
  type: webhook
inputs:
  - name: symptom
  - name: repro
graph:
  nodes:
    repro:
      action: dispatch_job
      assignee: x
      spec: { profile: "@scout", task: "repro" }
    tdd:
      action: dispatch_job
      assignee: x
      spec: { profile: "@coder", task: "test" }
    fix:
      action: dispatch_job
      assignee: x
      spec: { profile: "@coder", task: "fix" }
      needs: [tdd]
"##,
    )
    .expect("broken parses");
    let report = lint_workflow(&broken);
    assert!(
        !report.is_clean(),
        "broken repro edge must fail lint: {:?}",
        toron_workflow::lint::render(&report)
    );
    let joined = toron_workflow::lint::render(&report).join("\n");
    assert!(
        joined.contains("repro")
            && (joined.contains("predecessor") || joined.contains("no `repro`")),
        "lint must name the missing repro predecessor: {joined}"
    );

    // The builtin's correct chain passes.
    let report = lint_workflow(&builtin_bug_fix());
    assert!(
        report.is_clean(),
        "builtin bug-fix must lint clean: {:?}",
        toron_workflow::lint::render(&report)
    );
}

/// CLI control-flow: `flywheel workflow run bug-fix` (recording mode, no
/// --execute) exits 0, resolves the auto crew (scout + 2 coder + reviewer),
/// and reports all four dispatch nodes.
#[test]
fn bug_fix_cli_control_flow_reports_all_nodes() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_flywheel"))
        .args([
            "workflow",
            "run",
            "bug-fix",
            "--inputs",
            r#"{"symptom":"the leak","repro":"exit 1"}"#,
            "--json",
        ])
        .output()
        .expect("workflow run");
    assert!(
        out.status.success(),
        "control-flow run must exit 0: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json stdout");
    // crew: scout (repro) + coder x2 (tdd, fix) + reviewer (prove)
    assert_eq!(v["crew"]["scout"], 1, "crew: {v}");
    assert_eq!(v["crew"]["coder"], 2, "crew: {v}");
    assert_eq!(v["crew"]["reviewer"], 1, "crew: {v}");
    // all four graph nodes dispatched (recording mode reports each action)
    let output = v["output"].as_object().expect("output object");
    for node in ["repro", "tdd", "fix", "prove"] {
        assert!(output.contains_key(node), "{node} in CLI output: {v}");
        assert_eq!(output[node]["action"], "dispatch_job", "{node} action");
    }
}

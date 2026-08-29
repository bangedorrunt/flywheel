//! E2E: bug-fix playbook control flow + lint gate (bd-i18q).
//!
//! ADR-0019 vision test: 'if the target is a bug, the crew is FORCED through
//! repro -> repair loop -> ledger row.' ADR-0024 Scenario B wires the fix
//! path through the repair-until-verified recipe (unrolled repair-N/evaluate-N/verify-N).
//!
//! 1. **Node ORDER** — check the builtin `bug-fix` graph has repro BEFORE
//!    fix (call_workflow) BEFORE prove, with the `needs:` edges intact.
//! 2. **Lint gate (needs edges)** — the structural lint rejects a
//!    bug-fix-shaped graph whose `fix` does not reach `repro` through its
//!    transitive `needs:` closure (ADR-0019 Spec 6 fix-root-causes); the
//!    builtin passes. Proves a broken edge cannot ship.
//! 3. **CLI control-flow** — `flywheel workflow run bug-fix --inputs ...`
//!    (recording mode) exits 0 and reports repro + fix (recipe) + prove with the
//!    expanded crew (scout + recipe coders + reviewers).
//!
//! The live `--execute --mode headless` run (real agent panes) is executed
//! by the captain per the bead acceptance; the receipt goes in the close
//! reason.
//!
//! governed-by: ADR-0019 (Spec 6, Spec 7.2), ADR-0024 Scenario B

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

/// ADR-0024 Scenario B: bug-fix graph has repro → fix (repair recipe) → prove
#[test]
fn bug_fix_control_flow_orders_repro_tdd_fix_prove() {
    let wf = builtin_bug_fix();
    let graph = wf.graph.as_ref().expect("bug-fix is a graph");

    // Needs edges intact (the DAG shape the wave scheduler walks) — Scenario B: repro -> fix -> prove
    assert_eq!(graph.nodes["fix"].needs, vec!["repro".to_string()]);
    assert_eq!(graph.nodes["prove"].needs, vec!["fix".to_string()]);
    assert!(graph.nodes["repro"].needs.is_empty(), "repro is the root");
    // tdd no longer exists as a separate node — the repair loop handles RED→GREEN
    assert!(
        !graph.nodes.contains_key("tdd"),
        "tdd collapsed into repair recipe"
    );

    // fix must be the repair-until-verified recipe
    match &graph.nodes["fix"].action {
        Action::CallWorkflow {
            workflow, inputs, ..
        } => {
            assert_eq!(workflow, "repair-until-verified");
            assert!(inputs.get("verify").is_some(), "verify input");
            assert_eq!(
                inputs.get("max_iterations").and_then(|v| v.as_u64()),
                Some(3)
            );
        }
        other => panic!("fix must be call_workflow repair-until-verified, got {other:?}"),
    }

    // prove is @reviewer dispatch
    let Action::DispatchJob { spec, .. } = &graph.nodes["prove"].action else {
        panic!("prove is a dispatch_job");
    };
    assert_eq!(spec.harness.as_deref(), Some("opencode"));
    assert_eq!(graph.nodes.len(), 3, "exactly 3 nodes: repro, fix, prove");
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
/// --execute) exits 0, resolves the expanded crew (scout + recipe coders/reviewers),
/// and reports repro + fix (recipe) + prove.
#[test]
fn bug_fix_cli_control_flow_reports_all_nodes() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_flywheel"))
        .args([
            "workflow",
            "run",
            "bug-fix",
            "--inputs",
            r#"{"symptom":"the leak","repro":"exit 1","verify":"cargo test --offline --bin flywheel bug_fix_recipe"}"#,
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
    // crew: scout (repro) + recipe coders/reviewers + reviewer (prove)
    // repair-until-verified has 3 coder + 3 reviewer nodes, plus prove reviewer
    assert_eq!(v["crew"]["scout"], 1, "crew: {v}");
    assert_eq!(v["crew"]["coder"], 3, "crew: {v}");
    assert_eq!(v["crew"]["reviewer"], 4, "crew: {v}");
    // repro, fix (recipe), prove dispatched (recording mode reports each node)
    let output = v["output"].as_object().expect("output object");
    for node in ["repro", "fix", "prove"] {
        assert!(output.contains_key(node), "{node} in CLI output: {v}");
    }
    // repro and prove are dispatch_job; fix is the recipe (call_workflow expanded)
    assert_eq!(output["repro"]["action"], "dispatch_job", "repro action");
    // fix output is the recipe's result — in recording mode it may be call_workflow or the child's result
    assert!(output["fix"].is_object(), "fix output present: {v}");
    assert_eq!(output["prove"]["action"], "dispatch_job", "prove action");
}

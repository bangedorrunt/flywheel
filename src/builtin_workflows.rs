//! Compiled-in built-in workflows (ADR-0005 Spec 2b / P3.5).
//!
//! Zero-config: `flywheel workflow run debate --inputs '{proposal:"..."}'`
//! works on a fresh install. Each built-in carries the EXACT YAML from ADR
//! Spec 2b — purpose-named, composing profiles (Spec 1b) + stages (Spec 2a)
//! + control-flow (Spec 4). The catalog loads these as the compiled-in
//! defaults, overridable by `~/.config/flywheel/workflows/*.yaml`.
//!
//! - `brainstorm` — graph: parallel spawn @oracle (over breadth) → @planner
//!   reduce.
//! - `plan` — spawn @planner → request_approval gate.
//! - `beads` — spawn @planner stage:systematic-execute.
//! - `debate` — graph: parallel spawn @reviewer (over panel) → @oracle
//!   decide.
//! - `goal` — meta: call_workflow brainstorm→plan→beads→spawn @coder→
//!   @reviewer→triage.
//! - `triage` — the canonical message_posted reactive workflow.
//!
//! governed-by: ADR-0005 Spec 2b, P3.5

use std::collections::BTreeMap;

use toron_workflow::schema::{Workflow, parse_workflow};

/// The compiled-in built-in workflows, name → YAML text.
pub const BUILTIN_WORKFLOWS_YAML: &[(&str, &str)] = &[
    (
        "brainstorm",
        r##"
name: brainstorm
description: Divergent ideation — fan out oracles, a planner reduces to ranked options.
trigger:
  type: webhook
crew: auto
inputs:
  - name: question
  - name: breadth
graph:
  nodes:
    diverge:
      action: parallel
      over: "1"
      each: { action: dispatch_job, assignee: x, spec: { profile: "@oracle" } }
    reduce:
      action: dispatch_job
      assignee: x
      spec: { profile: "@planner" }
      needs: [diverge]
"##,
    ),
    (
        "plan",
        r##"
name: plan
description: Goal → bead-ready plan via the planner, with an optional approval gate.
trigger:
  type: webhook
crew: auto
inputs:
  - name: goal
steps:
  - name: decompose
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner" }
  - name: gate
    action: request_approval
    message: "Approve plan for {{inputs.goal}}?"
"##,
    ),
    (
        "beads",
        r##"
name: beads
description: Plan → tracker. The planner runs the systematic-execute stage.
trigger:
  type: webhook
crew: auto
inputs:
  - name: plan
steps:
  - name: create
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", stage: "systematic-execute" }
"##,
    ),
    (
        "debate",
        r##"
name: debate
description: Adversarial synthesis — reviewers brief, an oracle decides.
trigger:
  type: webhook
crew: auto
inputs:
  - name: proposal
  - name: panel
graph:
  nodes:
    briefs:
      action: parallel
      over: "1"
      each: { action: dispatch_job, assignee: x, spec: { profile: "@reviewer" } }
    decide:
      action: dispatch_job
      assignee: x
      spec: { profile: "@oracle" }
      needs: [briefs]
"##,
    ),
    (
        "goal",
        r##"
name: goal
description: The meta-workflow — brainstorm → plan → beads → build → review → ship.
trigger:
  type: webhook
crew: auto
inputs:
  - name: objective
steps:
  - name: ideate
    action: call_workflow
    workflow: brainstorm
    inputs: { question: "{{inputs.objective}}" }
  - name: plan
    action: call_workflow
    workflow: plan
    inputs: { goal: "{{inputs.objective}}" }
  - name: track
    action: call_workflow
    workflow: beads
    inputs: { plan: "{{steps.plan}}" }
  - name: build
    action: dispatch_job
    assignee: x
    spec: { profile: "@coder", stage: "systematic-execute" }
  - name: review
    action: dispatch_job
    assignee: x
    spec: { profile: "@reviewer" }
  - name: ship
    action: call_workflow
    workflow: triage
    inputs: { target: "{{inputs.objective}}" }
"##,
    ),
    (
        "triage",
        r##"
name: triage
description: Triage inbound messages — the canonical reactive workflow.
trigger:
  type: message_posted
  channel: "#support"
guard: "trigger.channel == '#support'"
steps:
  - name: ack
    action: send_dm
    to: "pi-alpha"
    body: "Received {{trigger.subject}}"
  - name: dispatch
    action: dispatch_job
    assignee: "oc-beta"
    spec: { task: "triage {{trigger.id}}" }
"##,
    ),
];

/// Load the compiled-in built-in workflows as parsed `Workflow`s.
pub fn builtin_workflows() -> BTreeMap<String, Workflow> {
    let mut map = BTreeMap::new();
    for (name, yaml) in BUILTIN_WORKFLOWS_YAML {
        match parse_workflow(yaml) {
            Ok(wf) => {
                map.insert(name.to_string(), wf);
            }
            Err(e) => {
                // A built-in that fails to parse is a crate bug — surface it
                // loudly rather than silently dropping the workflow.
                panic!("built-in workflow {name} failed to parse: {e}");
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_workflow::schema::CrewSpec;
    use toron_workflow::lint_yaml;

    /// All built-ins parse (ADR-0005 Spec 2b acceptance).
    #[test]
    fn all_builtins_parse() {
        let wfs = builtin_workflows();
        assert_eq!(wfs.len(), BUILTIN_WORKFLOWS_YAML.len(), "all built-ins load");
        for name in ["brainstorm", "plan", "beads", "debate", "goal", "triage"] {
            assert!(wfs.contains_key(name), "built-in {name} present");
        }
    }

    /// The five purpose-named workflows declare `crew: auto` (zero-config,
    /// auto-crew — Spec 4c). `triage` is the reactive one (no crew).
    #[test]
    fn purpose_named_workflows_are_crew_auto() {
        let wfs = builtin_workflows();
        for name in ["brainstorm", "plan", "beads", "debate", "goal"] {
            assert_eq!(
                wfs[name].crew,
                Some(CrewSpec::Auto),
                "{name} is crew: auto (zero-config auto-crew)"
            );
        }
        assert!(wfs["triage"].crew.is_none(), "triage has no crew");
    }

    /// `goal` is the meta-workflow: its steps are call_workflow chains.
    #[test]
    fn goal_is_call_workflow_chain() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let goal = &wfs["goal"];
        let chain: Vec<&str> = goal
            .steps
            .iter()
            .filter_map(|s| match &s.action {
                Action::CallWorkflow { workflow, .. } => Some(workflow.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            chain.contains(&"brainstorm") && chain.contains(&"plan") && chain.contains(&"beads"),
            "goal chains the built-ins: {chain:?}"
        );
    }

    /// Every built-in lints clean (the lint gate over all compiled-in
    /// workflows — ADR-0005 Lint-time checks).
    #[test]
    fn all_builtins_lint_clean() {
        for (name, yaml) in BUILTIN_WORKFLOWS_YAML {
            let report = lint_yaml(yaml).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(
                report.is_clean(),
                "built-in {name} must lint clean: {:?}",
                report.issues
            );
        }
    }
}

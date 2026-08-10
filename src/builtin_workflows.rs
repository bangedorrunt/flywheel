//! Compiled-in built-in workflows (ADR-0005 Spec 2b / P3.5).
//!
//! Zero-config: `flywheel workflow run debate --inputs '{proposal:"..."}'`
//! works on a fresh install. Each built-in carries the EXACT YAML from ADR
//! Spec 2b — purpose-named, composing profiles (Spec 1b) + stages (Spec 2a)
//! + control-flow (Spec 4). The catalog loads these as the compiled-in
//!   defaults, overridable by `~/.config/flywheel/workflows/*.yaml`.
//!
//! - `brainstorm` — graph: parallel dispatch @oracle (over 3) → @planner
//!   reduce (task consumes {{steps.diverge}}; fail_run on reduce).
//! - `plan` — dispatch @planner (task carries {{inputs.goal}}; fail_run on
//!   decompose) → request_approval gate.
//! - `beads` — dispatch @planner stage:systematic-execute (spec.plan carries
//!   the plan; fail_run on create).
//! - `debate` — graph: parallel dispatch @reviewer (over 2) → @oracle decide
//!   (task consumes {{steps.briefs}}; fail_run on decide).
//! - `goal` — meta: call_workflow brainstorm→plan→beads→dispatch @coder
//!   stage:systematic-execute→@reviewer stage:code-review→@housekeeper
//!   stage:commit-and-push; each dispatch carries the prior step's output via
//!   spec.plan, each fails the run on error.
//! - `triage` — the canonical message_posted reactive workflow (payload
//!   fields only: {{trigger.content}} / {{trigger.id}} — no subject).
//! - `onboard` / `triage-deep` — cold-start + deep-backlog chains; each step
//!   consumes the prior step's output via spec.plan, fail_run on every step.
//!
//! governed-by: ADR-0005 Spec 2b, P3.5

use std::collections::BTreeMap;

use toron_workflow::schema::{parse_workflow, Workflow};

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
graph:
  nodes:
    diverge:
      action: parallel
      over: "3"
      each: { action: dispatch_job, assignee: x, spec: { profile: "@oracle", task: "Diverge on the question — surface ideas, angles, and options: {{inputs.question}}" } }
    reduce:
      action: dispatch_job
      assignee: x
      spec: { profile: "@planner", task: "Reduce these oracle outputs into ranked options for: {{inputs.question}}\n\nOracle outputs: {{steps.diverge}}" }
      needs: [diverge]
      on_error: fail_run
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
    spec: { profile: "@planner", task: "Decompose this goal into a bead-ready plan (tasks, subtasks, dependency structure): {{inputs.goal}}" }
    on_error: fail_run
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
    spec: { profile: "@planner", stage: "systematic-execute", plan: "{{inputs.plan}}" }
    on_error: fail_run
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
graph:
  nodes:
    briefs:
      action: parallel
      over: "2"
      each: { action: dispatch_job, assignee: x, spec: { profile: "@reviewer", harness: "opencode", task: "Brief on the proposal from your adversarial angle: {{inputs.proposal}}" } }
    decide:
      action: dispatch_job
      assignee: x
      spec: { profile: "@oracle", task: "Adjudicate the briefs and decide on: {{inputs.proposal}}\n\nReviewer briefs: {{steps.briefs}}" }
      needs: [briefs]
      on_error: fail_run
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
    spec: { profile: "@coder", stage: "systematic-execute", plan: "{{steps.track}}" }
    on_error: fail_run
  - name: review
    action: dispatch_job
    assignee: x
    spec: { profile: "@reviewer", harness: "opencode", stage: "code-review", plan: "{{steps.build}}" }
    on_error: fail_run
  - name: ship
    action: dispatch_job
    assignee: x
    spec: { profile: "@housekeeper", stage: "commit-and-push", plan: "{{steps.build}}" }
    on_error: fail_run
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
    to: "operator"
    body: "Received {{trigger.content}}"
  - name: dispatch
    action: dispatch_job
    assignee: "x"
    spec: { profile: "@housekeeper", task: "triage {{trigger.id}}" }
"##,
    ),
    (
        "plan-review",
        r##"
name: plan-review
description: Review a plan bead-by-bead before implementation — the p5 stage.
trigger:
  type: webhook
crew: auto
inputs:
  - name: plan
steps:
  - name: review
    action: dispatch_job
    assignee: x
    spec: { profile: "@reviewer", harness: "opencode", stage: "plan-review", plan: "{{inputs.plan}}" }
    on_error: fail_run
"##,
    ),
    (
        "onboard",
        r##"
name: onboard
description: Cold-start a newcomer — map the repo, seed context, propose the first task.
trigger:
  type: webhook
crew: auto
inputs:
  - name: project
steps:
  - name: map
    action: dispatch_job
    assignee: x
    spec: { profile: "@scout", stage: "repo-map", project: "{{inputs.project}}" }
    on_error: fail_run
  - name: seed
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", stage: "seed-first-task", plan: "{{steps.map}}", project: "{{inputs.project}}" }
    on_error: fail_run
"##,
    ),
    (
        "triage-deep",
        r##"
name: triage-deep
description: Deep backlog triage — bv robot-triage, prioritize + assign, spawn the crew.
trigger:
  type: webhook
crew: auto
inputs:
  - name: project
steps:
  - name: triage
    action: dispatch_job
    assignee: x
    spec: { profile: "@scout", stage: "bv-triage", project: "{{inputs.project}}" }
    on_error: fail_run
  - name: prioritize
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", stage: "prioritize-assign", plan: "{{steps.triage}}", project: "{{inputs.project}}" }
    on_error: fail_run
  - name: spawn
    action: dispatch_job
    assignee: x
    spec: { profile: "@housekeeper", stage: "spawn-crew", plan: "{{steps.prioritize}}", project: "{{inputs.project}}" }
    on_error: fail_run
"##,
    ),
    (
        "weekly-review",
        r##"
name: weekly-review
description: "Weekly repo review — scout fan-out, oracle synthesis, exactly ONE follow-up bead (bd-m7ac)."
trigger:
  type: schedule
  cron: "0 4 * * 0"
  owner: oracle
  scope: project
policy:
  on_failure: bead
  on_repeat_failure: escalate
  repeat_threshold: 3
  on_success: log
  max_retries: 1
  no_claim_timeout: 600
inputs:
  - name: goal
graph:
  nodes:
    scout-fanout:
      action: parallel
      over: "3"
      each:
        action: dispatch_job
        assignee: x
        spec:
          profile: "@scout"
          task: "Scout the toron + flywheel repos: (1) internal gaps — code health, deferred ADR-0005/0010/0011 items still open, open seams, test gaps; (2) upstream parity — Dicklesworthstone/mcp_agent_mail_rust and block/buzz patterns not yet ported. Return COMPRESSED findings JSON: [{area, finding, evidence, ev}]. No academic literature."
    oracle:
      action: dispatch_job
      assignee: x
      spec:
        profile: "@oracle"
        task: "Synthesize the scout findings: {{steps.scout-fanout}}. Rank by EV + project-fit. Pick the SINGLE best improvement idea (feature / design pattern / bug fix). Output JSON: {\"idea\": {\"title\": ..., \"what\": ..., \"why\": ..., \"ev\": ..., \"blast_radius\": ..., \"acceptance\": ...}} — or {\"idea\": null} if nothing worth proposing (honest, not forced)."
      needs: [scout-fanout]
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
    use serde_json::Value;
    use toron_workflow::lint_yaml;
    use toron_workflow::schema::CrewSpec;

    /// All built-ins parse (ADR-0005 Spec 2b acceptance).
    #[test]
    fn all_builtins_parse() {
        let wfs = builtin_workflows();
        assert_eq!(
            wfs.len(),
            BUILTIN_WORKFLOWS_YAML.len(),
            "all built-ins load"
        );
        for name in [
            "brainstorm",
            "plan",
            "beads",
            "debate",
            "goal",
            "triage",
            "plan-review",
            "onboard",
            "triage-deep",
        ] {
            assert!(wfs.contains_key(name), "built-in {name} present");
        }
    }

    /// The purpose-named workflows declare `crew: auto` (zero-config,
    /// auto-crew — Spec 4c). `triage` is the reactive one (no crew).
    #[test]
    fn purpose_named_workflows_are_crew_auto() {
        let wfs = builtin_workflows();
        for name in [
            "brainstorm",
            "plan",
            "beads",
            "debate",
            "goal",
            "plan-review",
            "onboard",
            "triage-deep",
        ] {
            assert_eq!(
                wfs[name].crew,
                Some(CrewSpec::Auto),
                "{name} is crew: auto (zero-config auto-crew)"
            );
        }
        assert!(wfs["triage"].crew.is_none(), "triage has no crew");
    }

    /// bd-42fs step 5: every built-in REVIEWER dispatch step carries the
    /// per-step `harness: opencode` override (the decided lane for review/
    /// debate) — the profile itself stays omp; the override is per-step.
    #[test]
    fn reviewer_steps_override_harness_to_opencode() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        fn scan_action(action: &Action, name: &str, found: &mut u32) {
            match action {
                Action::DispatchJob { spec, .. }
                    if spec.get("profile").and_then(Value::as_str) == Some("@reviewer") =>
                {
                    assert_eq!(
                        spec.get("harness").and_then(Value::as_str),
                        Some("opencode"),
                        "{name}: reviewer step must override harness to opencode"
                    );
                    *found += 1;
                }
                Action::DispatchJob { .. } => {}
                Action::Parallel { each, .. } => scan_action(&each.action, name, found),
                Action::Retry { step, .. } => scan_action(&step.action, name, found),
                Action::Branch { then, else_steps, .. } => {
                    for s in then.iter().chain(else_steps.iter()) {
                        scan_action(&s.action, name, found);
                    }
                }
                Action::Foreach { each, .. } => scan_action(&each.action, name, found),
                _ => {}
            }
        }
        for name in ["debate", "goal", "plan-review"] {
            let mut found = 0;
            for step in &wfs[name].steps {
                scan_action(&step.action, name, &mut found);
            }
            if let Some(g) = &wfs[name].graph {
                for node in g.nodes.values() {
                    scan_action(&node.action, name, &mut found);
                }
            }
            assert!(found > 0, "{name}: expected at least one @reviewer step");
        }
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

    /// bd-m7ac: weekly-review is a schedule-triggered graph — parallel
    /// scouts (3 elastic copies) feeding an oracle node that needs them.
    #[test]
    fn weekly_review_is_scout_fanout_to_oracle() {
        use toron_workflow::schema::{Action, Trigger};
        let wfs = builtin_workflows();
        let wr = &wfs["weekly-review"];
        // schedule trigger, project scope, owner oracle
        match &wr.trigger {
            Some(Trigger::Schedule { cron, owner, scope }) => {
                assert_eq!(cron, "0 4 * * 0");
                assert_eq!(owner.as_deref(), Some("oracle"));
                assert_eq!(*scope, toron_workflow::schema::CronScope::Project);
            }
            other => panic!("weekly-review must be schedule-triggered: {other:?}"),
        }
        // graph: scout-fanout (parallel @scout) -> oracle (@oracle, needs scouts)
        let g = wr.graph.as_ref().expect("graph present");
        let fanout = &g.nodes["scout-fanout"];
        assert!(matches!(fanout.action, Action::Parallel { .. }));
        let oracle = &g.nodes["oracle"];
        assert!(matches!(oracle.action, Action::DispatchJob { .. }));
        assert!(oracle.needs.contains(&"scout-fanout".to_string()));
        // the oracle's task templates the scout outputs (the synthesis input)
        let Action::DispatchJob { spec, .. } = &oracle.action else {
            unreachable!()
        };
        let task = spec
            .get("task")
            .and_then(serde_json::Value::as_str)
            .expect("oracle task present");
        assert!(
            task.contains("{{steps.scout-fanout}}"),
            "oracle consumes scout findings"
        );
        assert!(task.contains("idea"), "oracle outputs the idea contract");
    }

    /// bd-oj9o #2: the cold-start + deep-triage workflows exist, and the
    /// plan-review workflow (slash target) exists.
    #[test]
    fn onboard_triage_deep_plan_review_present() {
        let wfs = builtin_workflows();
        for name in ["onboard", "triage-deep", "plan-review"] {
            assert!(wfs.contains_key(name), "built-in {name} present (bd-oj9o)");
        }
    }

    /// bd-oj9o #1: brainstorm/debate actually fan out (over > 1) — the dead
    /// `breadth`/`panel` inputs were replaced by a real parallel fan-out.
    #[test]
    fn brainstorm_and_debate_fan_out() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        for (name, node) in [("brainstorm", "diverge"), ("debate", "briefs")] {
            let wf = &wfs[name];
            let g = wf
                .graph
                .as_ref()
                .unwrap_or_else(|| panic!("{name} has a graph"));
            let n = g
                .nodes
                .get(node)
                .unwrap_or_else(|| panic!("{name}.{node} node exists"));
            let Action::Parallel { over, .. } = &n.action else {
                panic!("{name}.{node} is a parallel action");
            };
            let copies: usize = over.trim().parse().unwrap_or(1);
            assert!(
                copies > 1,
                "{name} must fan out over {copies} (parallelism is the point)"
            );
        }
    }

    /// bd-oj9o #1: the beads workflow carries the plan into the dispatch
    /// spec ({{inputs.plan}} → rendered into the headless prompt).
    #[test]
    fn beads_dispatch_carries_plan_input() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let beads = &wfs["beads"];
        let Action::DispatchJob { spec, .. } = &beads.steps[0].action else {
            panic!("beads step 0 is a dispatch_job");
        };
        assert_eq!(
            spec.get("plan").and_then(Value::as_str),
            Some("{{inputs.plan}}"),
            "beads dispatch spec carries the plan input"
        );
    }

    /// bd-oj9o #1: the reactive triage workflow has no hardcoded agent
    /// names (pi-alpha/oc-beta are spawn-dependent and break any real
    /// swarm) — ack goes to the operator, dispatch is profile-based.
    #[test]
    fn triage_has_no_hardcoded_agent_names() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let triage = &wfs["triage"];
        let yaml = BUILTIN_WORKFLOWS_YAML
            .iter()
            .find(|(n, _)| *n == "triage")
            .map(|(_, y)| *y)
            .unwrap();
        assert!(
            !yaml.contains("pi-alpha") && !yaml.contains("oc-beta"),
            "triage must not hardcode agent names"
        );
        let Action::DispatchJob { spec, .. } = &triage.steps[1].action else {
            panic!("triage step 1 is a dispatch_job");
        };
        assert_eq!(
            spec.get("profile").and_then(Value::as_str),
            Some("@housekeeper"),
            "triage dispatches via a profile, not a hardcoded name"
        );
    }

    /// bd-h4fc: the reducer/decider steps consume the fan-out outputs
    /// ({{steps.diverge}} / {{steps.briefs}}) — without the reference the
    /// planner/oracle decide blind (the ADR-0005 Spec 2b `inputs.ideas` /
    /// `inputs.arguments` parity under the dispatch_job shape).
    #[test]
    fn fanout_outputs_reach_reducers() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        for (name, node, token) in [
            ("brainstorm", "reduce", "{{steps.diverge}}"),
            ("debate", "decide", "{{steps.briefs}}"),
        ] {
            let g = wfs[name].graph.as_ref().expect("graph present");
            let Action::DispatchJob { spec, .. } = &g.nodes[node].action else {
                panic!("{name}.{node} is a dispatch_job");
            };
            let task = spec
                .get("task")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{name}.{node} has a task"));
            assert!(
                task.contains(token),
                "{name}.{node} consumes the fan-out ({token}): {task}"
            );
        }
    }

    /// bd-h4fc: no dispatch spec uses the dead `prompt` key — headless
    /// dispatch reads task/stage/base only, so `spec.prompt` would silently
    /// drop the mission (the brainstorm/diverge + debate/briefs regression).
    #[test]
    fn no_dispatch_spec_uses_dead_prompt_key() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        fn scan_action(action: &Action, wf: &str, dead: &mut Vec<String>) {
            match action {
                Action::DispatchJob { spec, .. } => {
                    if spec.get("prompt").is_some() {
                        dead.push(wf.to_string());
                    }
                }
                Action::Parallel { each, .. } => scan_action(&each.action, wf, dead),
                Action::Retry { step, .. } => scan_action(&step.action, wf, dead),
                Action::Branch { then, else_steps, .. } => {
                    for s in then.iter().chain(else_steps.iter()) {
                        scan_action(&s.action, wf, dead);
                    }
                }
                Action::Foreach { each, .. } => scan_action(&each.action, wf, dead),
                _ => {}
            }
        }
        let mut dead = Vec::new();
        for (name, wf) in &wfs {
            for step in &wf.steps {
                scan_action(&step.action, name, &mut dead);
            }
            if let Some(g) = &wf.graph {
                for node in g.nodes.values() {
                    scan_action(&node.action, name, &mut dead);
                }
            }
        }
        assert!(dead.is_empty(), "dispatch specs with dead prompt key: {dead:?}");
    }

    /// bd-h4fc: the goal meta-chain carries context forward — build gets the
    /// tracked plan ({{steps.track}}), review gets the build output
    /// ({{steps.build}}) with the opencode harness, and ship is a real
    /// commit-and-push dispatch (NOT a triage call — triage templates
    /// trigger.subject/id which do not exist on the invoke path).
    #[test]
    fn goal_carries_context_through_chain() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        let goal = &wfs["goal"];
        for (step_name, plan_token, harness) in [
            ("build", "{{steps.track}}", None),
            ("review", "{{steps.build}}", Some("opencode")),
            ("ship", "{{steps.build}}", None),
        ] {
            let step = goal
                .steps
                .iter()
                .find(|s| s.name.as_deref() == Some(step_name))
                .unwrap_or_else(|| panic!("goal.{step_name} present"));
            let Action::DispatchJob { spec, .. } = &step.action else {
                panic!("goal.{step_name} is a dispatch_job");
            };
            assert_eq!(
                spec.get("plan").and_then(Value::as_str),
                Some(plan_token),
                "goal.{step_name} carries the prior output"
            );
            if let Some(h) = harness {
                assert_eq!(
                    spec.get("harness").and_then(Value::as_str),
                    Some(h),
                    "goal.{step_name} keeps the harness override"
                );
            }
            assert_eq!(
                spec.get("profile").and_then(Value::as_str).unwrap(),
                match step_name {
                    "build" => "@coder",
                    "review" => "@reviewer",
                    _ => "@housekeeper",
                }
            );
        }
        let ship = goal
            .steps
            .iter()
            .find(|s| s.name.as_deref() == Some("ship"))
            .unwrap();
        let Action::DispatchJob { spec, .. } = &ship.action else {
            panic!("ship is a dispatch_job, not call_workflow triage");
        };
        assert_eq!(
            spec.get("stage").and_then(Value::as_str),
            Some("commit-and-push"),
            "ship reuses the commit-and-push stage"
        );
        assert!(
            !BUILTIN_WORKFLOWS_YAML
                .iter()
                .find(|(n, _)| *n == "goal")
                .unwrap()
                .1
                .contains("workflow: triage"),
            "goal.ship must not call triage (invoke-path templates would render empty)"
        );
    }

    /// bd-h4fc: every decisive step fails the run on error (on_error:
    /// fail_run) — a failed synthesis/tracking/build must not pass silently
    /// as a success.
    #[test]
    fn decisive_steps_fail_run() {
        use toron_workflow::schema::StepOnError;
        let wfs = builtin_workflows();
        fn step_errors(wf: &toron_workflow::schema::Workflow) -> Vec<(String, bool)> {
            let mut out = Vec::new();
            for s in &wf.steps {
                out.push((
                    s.name.clone().unwrap_or_else(|| s.action.type_name().into()),
                    s.on_error == StepOnError::FailRun,
                ));
            }
            if let Some(g) = &wf.graph {
                for (n, node) in &g.nodes {
                    out.push((n.clone(), node.on_error == StepOnError::FailRun));
                }
            }
            out
        }
        let checks: &[(&str, &[&str])] = &[
            ("brainstorm", &["reduce"]),
            ("plan", &["decompose"]),
            ("beads", &["create"]),
            ("debate", &["decide"]),
            ("goal", &["build", "review", "ship"]),
            ("plan-review", &["review"]),
            ("onboard", &["map", "seed"]),
            ("triage-deep", &["triage", "prioritize", "spawn"]),
        ];
        for (name, must_fail) in checks {
            let errors = step_errors(&wfs[*name]);
            for m in *must_fail {
                let (_, fail) = errors
                    .iter()
                    .find(|(n, _)| n == m)
                    .unwrap_or_else(|| panic!("{name}.{m} exists"));
                assert!(*fail, "{name}.{m} must be on_error: fail_run");
            }
        }
    }

    /// bd-h4fc: the reactive triage workflow templates ONLY payload fields
    /// that always exist on message_posted (content/id) — the former
    /// {{trigger.subject}} rendered an empty ack body even reactively.
    #[test]
    fn triage_uses_resolvable_payload_fields() {
        let yaml = BUILTIN_WORKFLOWS_YAML
            .iter()
            .find(|(n, _)| *n == "triage")
            .map(|(_, y)| *y)
            .unwrap();
        assert!(
            !yaml.contains("{{trigger.subject}}"),
            "triage must not template trigger.subject (absent from the payload)"
        );
        assert!(yaml.contains("{{trigger.content}}"), "ack uses content");
        assert!(yaml.contains("{{trigger.id}}"), "dispatch uses id");
    }

    /// bd-h4fc: onboard + triage-deep chains consume the prior step's output
    /// via spec.plan — a cold-start seed without the map, or a crew spawn
    /// without the prioritized list, was a disconnected dispatch.
    #[test]
    fn onboard_and_triage_deep_chain_outputs() {
        use toron_workflow::schema::Action;
        let wfs = builtin_workflows();
        for (name, step_name, token) in [
            ("onboard", "seed", "{{steps.map}}"),
            ("triage-deep", "prioritize", "{{steps.triage}}"),
            ("triage-deep", "spawn", "{{steps.prioritize}}"),
        ] {
            let wf = &wfs[name];
            let step = wf
                .steps
                .iter()
                .find(|s| s.name.as_deref() == Some(step_name))
                .unwrap_or_else(|| panic!("{name}.{step_name} present"));
            let Action::DispatchJob { spec, .. } = &step.action else {
                panic!("{name}.{step_name} is a dispatch_job");
            };
            assert_eq!(
                spec.get("plan").and_then(Value::as_str),
                Some(token),
                "{name}.{step_name} consumes the prior step output"
            );
        }
    }
}

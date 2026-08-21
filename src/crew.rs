//! Auto-crew / smart steering (ADR-0005 Spec 4c / P3.6).
//!
//! The orchestrator smartly steers the crew FROM the workflow. The captain
//! writes INTENT (roles + fan-out); this module computes the crew the
//! orchestrator spawns. The ENGINE never spawns — it consumes fan-out counts
//! via the injected `ProfileResolver` seam. This is the flywheel-side crew
//! planner:
//!
//! - **Auto** (`crew: auto`): infer members from the workflow's `profile:`
//!   refs. Count = max parallelism the graph demands for that role (each
//!   distinct `profile: "@X"` becomes a crew member; fan-out in `parallel
//!   over:` raises the count).
//! - **Explicit** (`crew: { role: count }`): the squad is declared; `@crew.r`
//!   binds to that role's members.
//! - **`@crew.<role>` fan-out count**: how many concurrent copies of a
//!   `parallel over: "@crew.<role>"` step the engine should run (translated
//!   to a concrete N before invoke — staged: sequential at P3, durable at P5).
//!
//! governed-by: ADR-0005 Spec 4c, P3.6

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use toron_workflow::schema::{Action, CrewSpec, InputDecl, Step, StepOnError, Workflow};

/// A computed crew: role → member count, ready for the orchestrator to spawn
/// via `ProfileCatalog::resolve` + `Backend::spawn`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrewPlan {
    /// role → member count.
    pub members: BTreeMap<String, u32>,
}

impl CrewPlan {
    /// Resolve the fan-out count for a `parallel over: "@crew.<role>"` step.
    /// `@crew.coder` → the coder member count (1 if the role isn't in the
    /// crew); a bare `N` stays `N` (elastic).
    pub fn fanout_count(&self, over: &str) -> u32 {
        if let Some(role) = over.trim().strip_prefix("@crew.") {
            self.members.get(role).copied().unwrap_or(1).max(1)
        } else {
            over.trim().parse().unwrap_or(1).max(1)
        }
    }
}

/// Resolve a workflow's elastic fan-out refs (`parallel over: "@crew.<role>"`)
/// to concrete counts from the crew plan (bd-gsvn.3). The engine's
/// `run_parallel` parses `over` as a number and defaults to 1 — an unresol-
/// ved `@crew.X` would silently degrade to a single copy. This rewrite makes
/// the fan-out elastic BEFORE invoke; each parallel copy is a durable child
/// promise (crash-safe mid-fanout resume is the engine's existing `ctx.run`
/// semantics). Bare `N` and non-parallel actions pass through untouched.
pub fn resolve_crew_fanouts(workflow: &Workflow, plan: &CrewPlan) -> Workflow {
    fn resolve_over(over: &str, plan: &CrewPlan) -> String {
        if over.trim().starts_with("@crew.") {
            plan.fanout_count(over).to_string()
        } else {
            over.to_string()
        }
    }
    fn resolve_step(step: &Step, plan: &CrewPlan) -> Step {
        let mut step = step.clone();
        step.action = resolve_action(&step.action, plan);
        step
    }
    fn resolve_steps(steps: &[Step], plan: &CrewPlan) -> Vec<Step> {
        steps.iter().map(|s| resolve_step(s, plan)).collect()
    }
    fn resolve_action(action: &Action, plan: &CrewPlan) -> Action {
        match action {
            Action::Parallel { over, each } => Action::Parallel {
                over: resolve_over(over, plan),
                each: Box::new(resolve_step(each, plan)),
            },
            Action::Retry {
                max_attempts,
                backoff_secs,
                step,
            } => Action::Retry {
                max_attempts: *max_attempts,
                backoff_secs: *backoff_secs,
                step: Box::new(resolve_step(step, plan)),
            },
            Action::Branch {
                if_guard,
                then,
                else_steps,
            } => Action::Branch {
                if_guard: if_guard.clone(),
                then: resolve_steps(then, plan),
                else_steps: resolve_steps(else_steps, plan),
            },
            Action::Foreach { over, each } => Action::Foreach {
                over: over.clone(),
                each: Box::new(resolve_step(each, plan)),
            },
            other => other.clone(),
        }
    }

    let mut wf = workflow.clone();
    wf.steps = resolve_steps(&wf.steps, plan);
    if let Some(graph) = &mut wf.graph {
        for node in graph.nodes.values_mut() {
            node.action = resolve_action(&node.action, plan);
        }
    }
    wf
}

/// Compute the crew plan for a workflow (ADR-0005 Spec 4c).
///
/// - `crew: { role: count }` → that squad.
/// - `crew: auto` → infer from the workflow's own `profile:` refs (a distinct
///   `profile: "@X"` adds a member of role X; `parallel over: "@crew.X"`
///   raises the count to the declared/needed parallelism). Explicit opt-in:
///   the built-in workflows ship with `crew: auto`.
/// - `crew` omitted → EMPTY plan (each spawn standalone). A crew-less
///   workflow must NOT silently spawn a crew even when its steps carry
///   `profile: "@X"` refs — auto-inference is an explicit request (is1.1).
#[allow(dead_code)]
pub fn plan_crew(workflow: &Workflow) -> CrewPlan {
    plan_crew_with(workflow, &|_| None)
}

/// One compose part: a dispatch part (`role:stage` — a `@role` agent runs the
/// named stage) or a workflow part (`workflow:name` — a `call_workflow`
/// sub-run). Parsed from `flywheel workflow compose --parts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposePart {
    /// `role:stage` → a dispatch_job step (profile @role, stage prompt).
    Dispatch { role: String, stage: String },
    /// `workflow:name` → a call_workflow step (resolved at invoke).
    CallWorkflow { name: String },
}

/// Parse one compose part token (`role:stage` | `workflow:name`). Pure —
/// the unit surface for the compose CLI's deterministic arm (bd-8xca).
pub fn parse_compose_part(token: &str) -> Result<ComposePart, String> {
    let t = token.trim();
    let Some((kind, value)) = t.split_once(':') else {
        return Err(format!(
            "compose part {token:?} must be `role:stage` or `workflow:name`"
        ));
    };
    match kind.trim() {
        "workflow" => {
            let name = value.trim();
            if name.is_empty() {
                return Err(format!("compose part {token:?}: empty workflow name"));
            }
            Ok(ComposePart::CallWorkflow {
                name: name.to_string(),
            })
        }
        role => {
            let role = role.trim();
            let stage = value.trim();
            if role.is_empty() || stage.is_empty() {
                return Err(format!(
                    "compose part {token:?}: role and stage must be non-empty"
                ));
            }
            Ok(ComposePart::Dispatch {
                role: role.to_string(),
                stage: stage.to_string(),
            })
        }
    }
}

/// bd-8xca deterministic arm: assemble a NEW workflow from existing parts.
/// `base` (optional) contributes its steps FIRST (the captain extends an
/// existing shape); then each part becomes a step — `role:stage` → a
/// dispatch_job (profile `@role`, stage `stage`), `workflow:name` → a
/// call_workflow. `crew: auto` (the composed fleet is inferred from the
/// parts). Pure + testable.
pub fn compose_workflow(
    name: &str,
    objective: &str,
    base: Option<&Workflow>,
    parts: &[ComposePart],
) -> Workflow {
    let mut steps: Vec<Step> = base.map(|b| b.steps.clone()).unwrap_or_default();
    let mut ordinal = 0u32;
    for part in parts {
        ordinal += 1;
        let step_name = match part {
            ComposePart::Dispatch { role, stage } => {
                let slug = format!("{role}-{stage}")
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                    .collect::<String>();
                let mut s = Step {
                    name: Some(slug),
                    guard: None,
                    on_error: StepOnError::Continue,
                    build_slot: false,
                    action: Action::DispatchJob {
                        assignee: "x".to_string(),
                        spec: toron_workflow::schema::DispatchSpec {
                            profile: Some(format!("@{role}")),
                            stage: Some(stage.clone()),
                            task: Some(objective.to_string()),
                            ..Default::default()
                        },
                        channel: None,
                    },
                };
                // deterministic unique names across repeated roles
                s.name = Some(format!("{}-{ordinal}", s.name.unwrap_or_default()));
                steps.push(s);
                continue;
            }
            ComposePart::CallWorkflow { name: sub } => Action::CallWorkflow {
                workflow: sub.clone(),
                inputs: serde_json::json!({ "goal": objective }),
            },
        };
        steps.push(Step {
            name: Some(format!("part-{ordinal}")),
            guard: None,
            on_error: StepOnError::Continue,
            build_slot: false,
            action: step_name,
        });
    }
    Workflow {
        name: name.to_string(),
        description: Some(format!(
            "Runtime-composed workflow for: {objective} (bd-8xca deterministic arm)"
        )),
        trigger: None,
        policy: None,
        guard: None,
        steps,
        graph: None,
        inputs: vec![InputDecl {
            name: "objective".to_string(),
            default: None,
        }],
        crew: Some(CrewSpec::Auto),
        principles: vec![],
    }
}

/// bd-u4cu planner arm: the headless planner's authoring prompt — the target
/// plus the catalog (workflows, profiles, stages) the authored YAML may
/// reference, so it can lint clean against the same gate as the
/// deterministic arm. Pure + testable; the harness subprocess stays in the
/// CLI shell.
pub fn planner_authoring_prompt(
    objective: &str,
    workflows: &[(&str, &str)],
    profiles: &[&str],
    stages: &[(&str, &str)],
) -> String {
    let mut wf_rows = String::new();
    for (name, desc) in workflows {
        wf_rows.push_str(&format!("- {name}: {desc}\n"));
    }
    let mut profile_rows = String::new();
    for p in profiles {
        profile_rows.push_str(&format!("- @{p}\n"));
    }
    let mut stage_rows = String::new();
    for (name, desc) in stages {
        stage_rows.push_str(&format!("- {name}: {desc}\n"));
    }
    format!(
        "You are the workflow composer. Author a NEW flywheel workflow YAML for this target:\n\
         TARGET: {objective}\n\
         \n\
         Building blocks:\n\
         - dispatch_job steps: `action: dispatch_job, assignee: x, spec: {{ profile: \"@<profile>\", stage: \"<stage>\", task: \"...\" }}`\n\
         - call_workflow steps: `action: call_workflow, workflow: <name>, inputs: {{ goal: \"...\" }}`\n\
         - send_dm / delay steps are also available\n\
         \n\
         AVAILABLE PROFILES:\n{profile_rows}\
         AVAILABLE STAGES:\n{stage_rows}\
         AVAILABLE WORKFLOWS (call_workflow targets):\n{wf_rows}\
         \n\
         Reply with ONE YAML document only (a ```yaml fenced block is fine):\n\
         - top-level keys: name, description, crew: auto, inputs, steps\n\
         - inputs MUST be a list of objects, never a map: `inputs: [{{ name: objective }}]`\n\
         - reference the target via {{{{inputs.objective}}}}\n\
         - only use profiles/stages/workflows listed above — anything else fails lint\n"
    )
}

/// bd-u4cu planner arm: extract the authored Workflow YAML from the
/// planner's response (fenced ```yaml block or bare document), parse it, and
/// stamp the caller's composed name over the authored one. Pure + testable.
/// Errors are caller-facing (the compose gate bails, never dispatches).
pub fn parse_planner_workflow(response: &str, name: &str) -> Result<Workflow, String> {
    let response = response.trim();
    if response.is_empty() {
        return Err("planner returned an empty response".into());
    }
    let yaml = extract_fenced_yaml(response).unwrap_or_else(|| response.to_string());
    let yaml = normalize_planner_inputs_map(&yaml)?;
    let mut wf = toron_workflow::schema::parse_workflow(&yaml)
        .map_err(|e| format!("planner-authored YAML does not parse as a workflow: {e}"))?;
    // The run's composed name is decided by the caller (composed-<ts>); the
    // authored doc's own name is discarded so lint + preview + run_id all
    // key on one identity.
    wf.name = name.to_string();
    Ok(wf)
}

/// Planners often emit `inputs: { objective: "..." }` (a map). The schema
/// wants `inputs: [{ name: objective }]`. Normalize so a live --agent run
/// does not die on that one shape error.
fn normalize_planner_inputs_map(yaml: &str) -> Result<String, String> {
    let mut v: serde_yaml::Value = serde_yaml::from_str(yaml)
        .map_err(|e| format!("planner-authored YAML does not parse as a workflow: {e}"))?;
    let Some(map) = v.as_mapping_mut() else {
        return Ok(yaml.to_string());
    };
    let key = serde_yaml::Value::from("inputs");
    let Some(inputs) = map.get_mut(&key) else {
        return Ok(yaml.to_string());
    };
    if inputs.as_sequence().is_some() {
        return Ok(yaml.to_string());
    }
    let Some(obj) = inputs.as_mapping() else {
        return Ok(yaml.to_string());
    };
    let seq: Vec<serde_yaml::Value> = obj
        .keys()
        .filter_map(|k| k.as_str())
        .map(|name| {
            let mut m = serde_yaml::Mapping::new();
            m.insert(
                serde_yaml::Value::from("name"),
                serde_yaml::Value::from(name),
            );
            serde_yaml::Value::Mapping(m)
        })
        .collect();
    *inputs = serde_yaml::Value::Sequence(seq);
    serde_yaml::to_string(&v).map_err(|e| format!("normalize inputs: {e}"))
}

/// The first ```fenced block of a response, or `None` when unfenced.
fn extract_fenced_yaml(response: &str) -> Option<String> {
    let lines: Vec<&str> = response.lines().collect();
    let mut open: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if line.trim().starts_with("```") {
            match open {
                None => open = Some(i),
                Some(o) => {
                    let body = lines[o + 1..i].join("\n");
                    return Some(body.trim().to_string());
                }
            }
        }
    }
    None
}

/// bd-t86p: like [`plan_crew`] but recurses into `call_workflow` targets so a
/// COMPOSED workflow's crew includes the sub-workflows' roles (goal-direct
/// composes brainstorm → oracle+planner → plan-direct → planner → own
/// coder/reviewer/housekeeper; crew:auto must spawn the WHOLE fleet or the
/// first @planner/@oracle step fails "no crew pane"). The resolver returns the
/// target workflow by name (catalog + builtins); `None` = unresolvable
/// sub-workflow (its roles are unknowable — skip, today's behavior).
/// Cycle-guarded: a visited set prevents infinite recursion on workflow
/// cycles (goal → plan → ... → goal).
pub fn plan_crew_with(workflow: &Workflow, resolve: &dyn Fn(&str) -> Option<Workflow>) -> CrewPlan {
    match &workflow.crew {
        Some(CrewSpec::Explicit(map)) => CrewPlan {
            members: map.clone(),
        },
        Some(CrewSpec::Auto) => {
            let mut visited = std::collections::HashSet::new();
            infer_auto(workflow, resolve, &mut visited)
        }
        None => CrewPlan::default(),
    }
}

/// Auto-infer the crew from the workflow's own steps/graph (zero-config:
/// `flywheel workflow run debate` spawns reviewers + oracle automatically),
/// PLUS the roles of composed sub-workflows (bd-t86p — see
/// [`plan_crew_with`]). Cycle-guarded via `visited`.
fn infer_auto(
    workflow: &Workflow,
    resolve: &dyn Fn(&str) -> Option<Workflow>,
    visited: &mut std::collections::HashSet<String>,
) -> CrewPlan {
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();

    fn bump_role(role: &str, counts: &mut BTreeMap<String, u32>) {
        if let Some(role) = role.strip_prefix('@') {
            *counts.entry(role.to_string()).or_insert(0) += 1;
        }
    }
    fn bump_fanout(over: &str, counts: &mut BTreeMap<String, u32>) {
        if let Some(role) = over.trim().strip_prefix("@crew.") {
            // A fan-out step needs at least one member per role; parallel
            // over the whole role means count >= 1 (the orchestrator may
            // raise it).
            counts.entry(role.to_string()).or_insert(0);
        }
    }
    fn scan_action(
        action: &Action,
        counts: &mut BTreeMap<String, u32>,
        resolve: &dyn Fn(&str) -> Option<Workflow>,
        visited: &mut std::collections::HashSet<String>,
    ) {
        match action {
            Action::DispatchJob { spec, .. } => {
                if let Some(role) = spec.profile.as_deref() {
                    bump_role(role, counts);
                }
            }
            // bd-t86p: the composed sub-workflow's roles belong to the crew.
            // Guard against cycles (visited) so goal → plan → … → goal cannot
            // recurse forever.
            Action::CallWorkflow { workflow, .. } => {
                if visited.insert(workflow.clone())
                    && let Some(sub) = resolve(workflow)
                {
                    let sub_plan = infer_auto(&sub, resolve, visited);
                    for (role, count) in sub_plan.members {
                        *counts.entry(role).or_insert(0) =
                            counts.get(&role).copied().unwrap_or(0).max(count);
                    }
                }
            }
            Action::Parallel { over, each } => {
                bump_fanout(over, counts);
                scan_action(&each.action, counts, resolve, visited);
            }
            Action::Retry { step, .. } => scan_action(&step.action, counts, resolve, visited),
            Action::Branch {
                then, else_steps, ..
            } => {
                for s in then.iter().chain(else_steps.iter()) {
                    scan_action(&s.action, counts, resolve, visited);
                }
            }
            Action::Foreach { each, .. } => scan_action(&each.action, counts, resolve, visited),
            _ => {}
        }
    }

    for step in &workflow.steps {
        scan_action(&step.action, &mut counts, resolve, visited);
    }
    if let Some(graph) = &workflow.graph {
        for node in graph.nodes.values() {
            scan_action(&node.action, &mut counts, resolve, visited);
        }
    }

    CrewPlan { members: counts }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_workflow::schema::parse_workflow;

    /// `crew: auto` infers members from the workflow's profile refs
    /// (Behavior Test Matrix row `crew_auto_infer`).
    #[test]
    fn crew_auto_infers_from_profile_refs() {
        let wf = parse_workflow(
            r##"
name: debate
trigger:
  type: webhook
crew: auto
steps:
  - name: brief_a
    action: dispatch_job
    assignee: x
    spec: { profile: "@reviewer" }
  - name: brief_b
    action: dispatch_job
    assignee: x
    spec: { profile: "@reviewer" }
  - name: decide
    action: dispatch_job
    assignee: x
    spec: { profile: "@oracle" }
"##,
        )
        .expect("parse");
        let plan = plan_crew(&wf);
        assert_eq!(plan.members.get("reviewer"), Some(&2), "{:?}", plan.members);
        assert_eq!(plan.members.get("oracle"), Some(&1), "{:?}", plan.members);
    }

    /// bd-t86p: crew:auto on a COMPOSED workflow (call_workflow steps) must
    /// include the sub-workflows' roles — goal-direct composes brainstorm
    /// (oracle+planner) and plan-direct (planner) on top of its own
    /// coder/reviewer/housekeeper; without recursion the first @oracle/@planner
    /// dispatch fails "no crew pane".
    #[test]
    fn crew_auto_recurses_into_call_workflow_targets() {
        let wf = parse_workflow(
            r##"
name: goal-direct
trigger:
  type: webhook
crew: auto
steps:
  - name: ideate
    action: call_workflow
    workflow: brainstorm
  - name: build
    action: dispatch_job
    assignee: x
    spec: { profile: "@coder" }
"##,
        )
        .expect("parse");
        let resolve = |sub: &str| match sub {
            "brainstorm" => Some(
                parse_workflow(
                    r##"
name: brainstorm
trigger: { type: webhook }
crew: auto
steps:
  - name: diverge
    action: dispatch_job
    assignee: x
    spec: { profile: "@oracle" }
  - name: reduce
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner" }
"##,
                )
                .expect("sub"),
            ),
            _ => None,
        };
        let plan = plan_crew_with(&wf, &resolve);
        assert_eq!(plan.members.get("coder"), Some(&1), "{:?}", plan.members);
        assert_eq!(
            plan.members.get("oracle"),
            Some(&1),
            "sub-workflow oracle: {:?}",
            plan.members
        );
        assert_eq!(
            plan.members.get("planner"),
            Some(&1),
            "sub-workflow planner: {:?}",
            plan.members
        );
    }

    /// bd-t86p cycle guard: a workflow that composes itself (or a cycle
    /// goal → plan → … → goal) must terminate, not recurse forever.
    #[test]
    fn crew_auto_cycle_guard_terminates() {
        let wf = parse_workflow(
            r##"
name: a
trigger: { type: webhook }
crew: auto
steps:
  - name: self
    action: call_workflow
    workflow: b
"##,
        )
        .expect("parse");
        let b = parse_workflow(
            r##"
name: b
trigger: { type: webhook }
crew: auto
steps:
  - name: back
    action: call_workflow
    workflow: a
  - name: leaf
    action: dispatch_job
    assignee: x
    spec: { profile: "@coder" }
"##,
        )
        .expect("b");
        let resolve = |sub: &str| match sub {
            "a" => Some(wf.clone()),
            "b" => Some(b.clone()),
            _ => None,
        };
        // Would stack-overflow without the visited guard; must terminate with
        // the leaf role found exactly once.
        let plan = plan_crew_with(&wf, &resolve);
        assert_eq!(plan.members.get("coder"), Some(&1), "{:?}", plan.members);
    }

    // ---- bd-8xca: runtime composition (deterministic arm) ----

    /// Parse: `role:stage` → dispatch; `workflow:name` → call_workflow;
    /// malformed tokens error.
    #[test]
    fn compose_part_parses_dispatch_and_workflow() {
        assert_eq!(
            parse_compose_part("scout:fresh-eyes-explore").unwrap(),
            ComposePart::Dispatch {
                role: "scout".into(),
                stage: "fresh-eyes-explore".into(),
            }
        );
        assert_eq!(
            parse_compose_part("workflow:weekly-review").unwrap(),
            ComposePart::CallWorkflow {
                name: "weekly-review".into(),
            }
        );
        assert!(parse_compose_part("nonsense").is_err(), "no colon errors");
        assert!(parse_compose_part(":stage").is_err(), "empty role errors");
        assert!(
            parse_compose_part("workflow:").is_err(),
            "empty name errors"
        );
    }

    /// The deterministic arm assembles a NEW workflow from parts: dispatch
    /// parts become dispatch_job steps (@role + stage), workflow parts become
    /// call_workflow steps, the objective flows into each; crew:auto + the
    /// objective input declared. Lint-clean (the compose gate).
    #[test]
    fn compose_workflow_assembles_parts_and_lints_clean() {
        use toron_workflow::schema::Action;
        let parts = vec![
            ComposePart::Dispatch {
                role: "scout".into(),
                stage: "fresh-eyes-explore".into(),
            },
            ComposePart::CallWorkflow {
                name: "weekly-review".into(),
            },
        ];
        let wf = compose_workflow("composed-x", "validate the new exchange", None, &parts);
        assert_eq!(wf.name, "composed-x");
        assert_eq!(wf.steps.len(), 2);
        assert_eq!(wf.crew, Some(CrewSpec::Auto));
        assert!(matches!(
            &wf.steps[0].action,
            Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@scout")
                    && spec.stage.as_deref() == Some("fresh-eyes-explore")
        ));
        assert!(matches!(
            &wf.steps[1].action,
            Action::CallWorkflow { workflow, .. } if workflow == "weekly-review"
        ));
        // The compose gate: lint clean.
        let report = toron_workflow::lint_workflow(&wf);
        assert!(
            report.is_clean(),
            "composed workflow must lint clean: {:?}",
            report.issues
        );
        // crew:auto infers the fleet from the parts (scout + weekly-review's
        // oracle/scout fan-out).
        let plan = plan_crew_with(&wf, &|_| None);
        assert!(plan.members.contains_key("scout"), "{:?}", plan.members);
    }

    /// A base workflow seeds the composition: base steps first, then parts.
    #[test]
    fn compose_workflow_seeds_from_base() {
        use toron_workflow::schema::Action;
        let base = parse_workflow(
            r##"
name: triage
trigger: { type: webhook }
crew: auto
steps:
  - name: gate
    action: send_dm
    to: operator
    body: "start"
"##,
        )
        .expect("base");
        let parts = vec![ComposePart::Dispatch {
            role: "coder".into(),
            stage: "systematic-execute".into(),
        }];
        let wf = compose_workflow("composed-base", "harden the loop", Some(&base), &parts);
        assert_eq!(wf.steps.len(), 2, "base step + one part");
        assert!(matches!(&wf.steps[0].action, Action::SendDm { .. }));
        assert!(matches!(
            &wf.steps[1].action,
            Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@coder")
        ));
    }

    /// Explicit `crew: { role: count }` spawns the squad (row `crew_explicit`).
    #[test]
    fn crew_explicit_squad() {
        let wf = parse_workflow(
            r##"
name: ship
trigger:
  type: webhook
crew: { planner: 1, coder: 2, reviewer: 1 }
steps:
  - name: a
    action: send_dm
    to: x
    body: y
"##,
        )
        .expect("parse");
        let plan = plan_crew(&wf);
        assert_eq!(plan.members.get("coder"), Some(&2));
        assert_eq!(plan.members.get("planner"), Some(&1));
    }

    /// `parallel over: "@crew.coder"` fans across the role's members (row
    /// `crew_explicit` binding); the count resolves from the plan.
    #[test]
    fn crew_fanout_count_resolves() {
        let wf = parse_workflow(
            r##"
name: ship
trigger:
  type: webhook
crew: { coder: 2 }
steps:
  - name: build
    action: parallel
    over: "@crew.coder"
    each: { action: send_dm, to: x, body: y }
"##,
        )
        .expect("parse");
        let plan = plan_crew(&wf);
        assert_eq!(
            plan.fanout_count("@crew.coder"),
            2,
            "@crew.coder -> 2 coders"
        );
        // Elastic N stays N.
        assert_eq!(plan.fanout_count("4"), 4);
        // Unknown crew role → 1 (safe default).
        assert_eq!(plan.fanout_count("@crew.ghost"), 1);
    }

    /// bd-gsvn.3: `resolve_crew_fanouts` substitutes `@crew.<role>` refs with
    /// the plan's concrete counts (steps + graph, nested in retry/branch),
    /// leaving elastic `N` and non-parallel actions untouched.
    #[test]
    fn resolve_crew_fanouts_substitutes_crew_refs() {
        use toron_workflow::schema::Action;
        let wf = parse_workflow(
            r##"
name: ship
trigger:
  type: webhook
crew: { coder: 2, reviewer: 1 }
steps:
  - name: build
    action: parallel
    over: "@crew.coder"
    each: { action: send_dm, to: x, body: y }
  - name: elastic
    action: parallel
    over: "4"
    each: { action: send_dm, to: x, body: y }
  - name: retry-build
    action: retry
    max_attempts: 2
    backoff_secs: 1
    step:
      name: inner
      action: parallel
      over: "@crew.reviewer"
      each: { action: send_dm, to: x, body: y }
  - name: gate
    action: branch
    if: "1 == 1"
    then:
      - name: t
        action: parallel
        over: "@crew.coder"
        each: { action: send_dm, to: x, body: y }
    else:
      - name: e
        action: parallel
        over: "@crew.ghost"
        each: { action: send_dm, to: x, body: y }
"##,
        )
        .expect("parse");
        let plan = plan_crew(&wf);
        let resolved = resolve_crew_fanouts(&wf, &plan);

        fn parallel_overs(step: &Step, out: &mut Vec<String>) {
            match &step.action {
                Action::Parallel { over, each } => {
                    out.push(over.clone());
                    parallel_overs(each, out);
                }
                Action::Retry { step, .. } => parallel_overs(step, out),
                Action::Branch {
                    then, else_steps, ..
                } => {
                    for s in then.iter().chain(else_steps.iter()) {
                        parallel_overs(s, out);
                    }
                }
                Action::Foreach { each, .. } => parallel_overs(each, out),
                _ => {}
            }
        }
        let mut overs = Vec::new();
        for step in &resolved.steps {
            parallel_overs(step, &mut overs);
        }
        assert_eq!(
            overs,
            vec![
                "2".to_string(), // @crew.coder -> 2
                "4".to_string(), // elastic N untouched
                "1".to_string(), // @crew.reviewer -> 1 (nested retry)
                "2".to_string(), // @crew.coder (branch then)
                "1".to_string(), // @crew.ghost -> 1 (safe default)
            ],
            "all @crew refs substituted, elastic N preserved: {overs:?}"
        );
        // The ORIGINAL workflow is untouched (resolve is a pure rewrite).
        let mut orig = Vec::new();
        for step in &wf.steps {
            parallel_overs(step, &mut orig);
        }
        assert_eq!(orig[0], "@crew.coder");
    }

    /// bd-gsvn.3: graph nodes are resolved too, not just linear steps.
    #[test]
    fn resolve_crew_fanouts_applies_to_graph_nodes() {
        use toron_workflow::schema::Action;
        let wf = parse_workflow(
            r##"
name: ship
trigger:
  type: webhook
crew: { coder: 3 }
graph:
  nodes:
    build:
      action: parallel
      over: "@crew.coder"
      each: { action: send_dm, to: x, body: y }
    ship:
      action: send_dm
      to: x
      body: done
      needs: [build]
"##,
        )
        .expect("parse");
        let plan = plan_crew(&wf);
        let resolved = resolve_crew_fanouts(&wf, &plan);
        let graph = resolved.graph.as_ref().expect("graph");
        match &graph.nodes["build"].action {
            Action::Parallel { over, .. } => assert_eq!(over, "3"),
            other => panic!("build should stay parallel, got {other:?}"),
        }
        // A graph WITHOUT the fan-out ref resolves to the same shape.
        let bare = parse_workflow(
            r##"
name: ship
trigger:
  type: webhook
crew: { coder: 3 }
graph:
  nodes:
    build:
      action: parallel
      over: "2"
      each: { action: send_dm, to: x, body: y }
"##,
        )
        .expect("parse");
        let r2 = resolve_crew_fanouts(&bare, &plan_crew(&bare));
        match &r2.graph.as_ref().unwrap().nodes["build"].action {
            Action::Parallel { over, .. } => assert_eq!(over, "2", "elastic N untouched"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// No crew → empty plan (each spawn standalone).
    #[test]
    fn no_crew_empty_plan() {
        let wf = parse_workflow(
            r##"
name: plain
trigger:
  type: webhook
steps:
  - name: a
    action: send_dm
    to: x
    body: y
"##,
        )
        .expect("parse");
        let plan = plan_crew(&wf);
        assert!(plan.members.is_empty());
    }

    /// is1.1 regression: a crew-LESS workflow whose steps carry
    /// `profile: "@X"` refs must produce an EMPTY plan — auto-inference is an
    /// explicit `crew: auto` opt-in, never a silent side effect of profile
    /// refs (documented standalone contract).
    #[test]
    fn omitted_crew_with_profile_refs_stays_standalone() {
        let wf = parse_workflow(
            r##"
name: plain-with-refs
trigger:
  type: webhook
steps:
  - name: a
    action: dispatch_job
    assignee: x
    spec: { profile: "@reviewer" }
  - name: b
    action: dispatch_job
    assignee: x
    spec: { profile: "@oracle" }
"##,
        )
        .expect("parse");
        assert!(wf.crew.is_none(), "fixture has no crew");
        let plan = plan_crew(&wf);
        assert!(
            plan.members.is_empty(),
            "crew omitted => standalone, even with profile refs: {:?}",
            plan.members
        );
    }

    /// 193.3: `infer_auto` recurses into Branch / Foreach / Retry — a crew
    /// nested three actions deep still resolves every distinct profile ref.
    #[test]
    fn infer_auto_recurses_branch_foreach_retry() {
        use toron_workflow::schema::{Action, Step, StepOnError, Trigger, Workflow};
        let dj = |profile: &str| Step {
            name: None,
            guard: None,
            on_error: StepOnError::Continue,
            build_slot: false,
            action: Action::DispatchJob {
                assignee: "x".into(),
                spec: toron_workflow::schema::DispatchSpec {
                    profile: Some(profile.to_string()),
                    ..Default::default()
                },
                channel: None,
            },
        };
        let mk = |name: &str, steps: Vec<Step>| Workflow {
            name: name.into(),
            description: None,
            trigger: Some(Trigger::MessagePosted {
                channel: None,
                project: None,
            }),
            policy: None,
            guard: None,
            crew: Some(CrewSpec::Auto),
            principles: vec![],
            steps,
            graph: None,
            inputs: vec![],
        };
        let wf = mk(
            "nested",
            vec![Step {
                name: Some("outer-branch".into()),
                guard: None,
                on_error: StepOnError::Continue,
                build_slot: false,
                action: Action::Branch {
                    if_guard: "true".into(),
                    then: vec![dj("@reviewer")],
                    else_steps: vec![dj("@oracle")],
                },
            }],
        );
        // Foreach + Retry (the other recursion arms).
        let wf2 = mk(
            "nested2",
            vec![
                Step {
                    name: Some("loop".into()),
                    guard: None,
                    on_error: StepOnError::Continue,
                    build_slot: false,
                    action: Action::Foreach {
                        over: "items".into(),
                        each: Box::new(dj("@coder")),
                    },
                },
                Step {
                    name: Some("retry".into()),
                    guard: None,
                    on_error: StepOnError::Continue,
                    build_slot: false,
                    action: Action::Retry {
                        max_attempts: 3,
                        backoff_secs: 1,
                        step: Box::new(dj("@scout")),
                    },
                },
            ],
        );
        let plan = plan_crew(&wf);
        assert_eq!(plan.members.get("reviewer"), Some(&1), "{:?}", plan.members);
        assert_eq!(plan.members.get("oracle"), Some(&1), "{:?}", plan.members);
        let plan2 = plan_crew(&wf2);
        assert_eq!(plan2.members.get("coder"), Some(&1), "{:?}", plan2.members);
        assert_eq!(plan2.members.get("scout"), Some(&1), "{:?}", plan2.members);
    }

    // ---- bd-u4cu: workflow compose --agent (planner arm) ----

    /// The planner's authoring prompt carries the target + the catalog
    /// (workflows, profiles, stages) the authored YAML may reference.
    #[test]
    fn planner_authoring_prompt_carries_target_and_catalog() {
        let prompt = planner_authoring_prompt(
            "harden the loop",
            &[
                ("weekly-review", "periodic review"),
                ("plan", "goal to plan"),
            ],
            &["planner", "coder"],
            &[
                ("fresh-eyes-explore", "fresh eyes"),
                ("plan-workflow", "plan a workflow"),
            ],
        );
        assert!(prompt.contains("harden the loop"), "{prompt}");
        assert!(
            prompt.contains("weekly-review: periodic review"),
            "{prompt}"
        );
        assert!(prompt.contains("@planner"), "{prompt}");
        assert!(prompt.contains("fresh-eyes-explore"), "{prompt}");
        assert!(prompt.contains("call_workflow"), "{prompt}");
    }

    /// Valid planner YAML (fenced) parses; the composed run name is stamped
    /// over the authored name; the engine lint gate is clean.
    #[test]
    fn planner_valid_yaml_parses_and_lints_clean() {
        let response = "Here is the workflow:\n\n```yaml\n\
            name: planner-draft\n\
            description: authored\n\
            crew: auto\n\
            inputs:\n  - name: objective\n\
            steps:\n  - name: plan\n\
            \x20   action: dispatch_job\n\
            \x20   assignee: x\n\
            \x20   spec:\n\
            \x20     profile: \"@planner\"\n\
            \x20     task: \"plan the target\"\n\
            ```\n";
        let wf = parse_planner_workflow(response, "composed-x").expect("valid yaml parses");
        assert_eq!(wf.name, "composed-x");
        let report = toron_workflow::lint_workflow(&wf);
        assert!(
            report.is_clean(),
            "authored workflow must lint clean: {:?}",
            report.issues
        );
    }

    /// Bare (unfenced) YAML from the planner is accepted too.
    #[test]
    fn planner_bare_yaml_parses() {
        let response = "name: draft\ndescription: \"\"\ncrew: auto\nsteps:\n  - name: s1\n    action: send_dm\n    to: operator\n    body: start\n";
        let wf = parse_planner_workflow(response, "composed-x").expect("bare yaml parses");
        assert_eq!(wf.name, "composed-x");
        assert_eq!(wf.steps.len(), 1);
    }

    /// Garbage from the planner bails — never dispatched.
    #[test]
    fn planner_garbage_rejected() {
        assert!(parse_planner_workflow("this is not yaml: [", "composed-x").is_err());
        assert!(parse_planner_workflow("", "composed-x").is_err());
        assert!(parse_planner_workflow("   \n\t\n", "composed-x").is_err());
        // Valid YAML that is NOT a workflow (no name) is rejected too.
        assert!(parse_planner_workflow("foo: bar\nbaz: 1\n", "composed-x").is_err());
    }

    #[test]
    fn planner_map_inputs_normalized_to_list() {
        let response = "name: draft\ncrew: auto\ninputs:\n  objective: do the thing\nsteps:\n  - name: s1\n    action: send_dm\n    to: operator\n    body: start\n";
        let wf = parse_planner_workflow(response, "composed-x").expect("map inputs ok");
        assert_eq!(wf.inputs.len(), 1);
        assert_eq!(wf.inputs[0].name, "objective");
    }
}

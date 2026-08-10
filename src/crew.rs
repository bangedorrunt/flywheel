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

use toron_workflow::schema::{Action, CrewSpec, Step, Workflow};

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
pub fn plan_crew(workflow: &Workflow) -> CrewPlan {
    match &workflow.crew {
        Some(CrewSpec::Explicit(map)) => CrewPlan {
            members: map.clone(),
        },
        Some(CrewSpec::Auto) => infer_auto(workflow),
        None => CrewPlan::default(),
    }
}

/// Auto-infer the crew from the workflow's own steps/graph (zero-config:
/// `flywheel workflow run debate` spawns reviewers + oracle automatically).
fn infer_auto(workflow: &Workflow) -> CrewPlan {
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
    fn scan_action(action: &Action, counts: &mut BTreeMap<String, u32>) {
        match action {
            Action::DispatchJob { spec, .. } => {
                if let Some(role) = spec.get("profile").and_then(|p| p.as_str()) {
                    bump_role(role, counts);
                }
            }
            Action::Parallel { over, each } => {
                bump_fanout(over, counts);
                scan_action(&each.action, counts);
            }
            Action::Retry { step, .. } => scan_action(&step.action, counts),
            Action::Branch { then, else_steps, .. } => {
                for s in then.iter().chain(else_steps.iter()) {
                    scan_action(&s.action, counts);
                }
            }
            Action::Foreach { each, .. } => scan_action(&each.action, counts),
            _ => {}
        }
    }

    for step in &workflow.steps {
        scan_action(&step.action, &mut counts);
    }
    if let Some(graph) = &workflow.graph {
        for node in graph.nodes.values() {
            scan_action(&node.action, &mut counts);
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
        assert_eq!(plan.fanout_count("@crew.coder"), 2, "@crew.coder -> 2 coders");
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
                Action::Branch { then, else_steps, .. } => {
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
                "2".to_string(),          // @crew.coder -> 2
                "4".to_string(),          // elastic N untouched
                "1".to_string(),          // @crew.reviewer -> 1 (nested retry)
                "2".to_string(),          // @crew.coder (branch then)
                "1".to_string(),          // @crew.ghost -> 1 (safe default)
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
            action: Action::DispatchJob {
                assignee: "x".into(),
                spec: serde_json::json!({ "profile": profile }),
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
                    action: Action::Foreach {
                        over: "items".into(),
                        each: Box::new(dj("@coder")),
                    },
                },
                Step {
                    name: Some("retry".into()),
                    guard: None,
                    on_error: StepOnError::Continue,
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
}

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

use anyhow::Result;
use serde::{Deserialize, Serialize};

use toron_workflow::schema::{Action, CrewSpec, Workflow};

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

/// Compute the crew plan for a workflow (ADR-0005 Spec 4c).
///
/// - `crew: { role: count }` → that squad.
/// - `crew: auto` → infer from the workflow's own `profile:` refs (a distinct
///   `profile: "@X"` adds a member of role X; `parallel over: "@crew.X"`
///   raises the count to the declared/needed parallelism).
/// - `crew` omitted → empty plan (each spawn standalone).
pub fn plan_crew(workflow: &Workflow) -> CrewPlan {
    match &workflow.crew {
        Some(CrewSpec::Explicit(map)) => CrewPlan {
            members: map.clone(),
        },
        Some(CrewSpec::Auto) | None => infer_auto(workflow),
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
}

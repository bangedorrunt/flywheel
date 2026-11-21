// bd-3ann1: pure planner for composing a workflow IN FLIGHT onto seats that
// already exist. No I/O, no clock, no env: it reads the workflow + live seats
// and decides which step rides which seat and which roles need a new pane.
// The engine applies the decisions; the preview prints them before consent.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use toron_workflow::schema::{Action, Step, Workflow};

use crate::seat_binder::{Binding, LiveSeat, StepNeed, bind};
use crate::stages::StageContext;

/// Step role → seat role. A live fleet calls its workers `crew`; a workflow
/// calls them `coder`. A reviewer seat must never be the step's author.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRoles {
    pub aliases: BTreeMap<String, String>,
}

impl Default for JoinRoles {
    fn default() -> Self {
        Self {
            aliases: BTreeMap::from([("crew".into(), "coder".into())]),
        }
    }
}

impl JoinRoles {
    fn seat_role(&self, role: &str) -> String {
        self.aliases
            .get(role)
            .cloned()
            .unwrap_or_else(|| role.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepBinding {
    pub step: String,
    pub role: String,
    /// `Some(pin)` when a live seat can take the step.
    pub pin: Option<String>,
    pub stage: Option<String>,
    pub spawn: bool,
    pub wait: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinPlan {
    pub steps: Vec<StepBinding>,
    /// Roles with no live seat — the only panes an in-flight compose starts.
    pub spawns: Vec<String>,
    /// True when the plan would start a pane, so it needs --yes.
    pub needs_yes: bool,
}

fn assignee_role(assignee: &str) -> Option<String> {
    assignee
        .strip_prefix("@crew.")
        .or_else(|| assignee.strip_prefix('@'))
        .filter(|s| !s.is_empty() && *s != "x")
        .map(str::to_string)
}

/// The role a step dispatches to, if any. Engine steps (verify, gate) have no
/// seat role and are left to the engine.
fn step_role(step: &toron_workflow::schema::Step) -> Option<String> {
    match &step.action {
        Action::DispatchJob { assignee, spec, .. } => spec
            .profile
            .as_deref()
            .and_then(assignee_role)
            .or_else(|| assignee_role(assignee)),
        _ => None,
    }
}

fn stage_of(step: &toron_workflow::schema::Step) -> Option<String> {
    match &step.action {
        Action::DispatchJob { spec, .. } => spec.stage.clone(),
        _ => None,
    }
}

/// Review/verify stages need a fresh context and must not inspect their own
/// author's work; execution rides a warm seat.
fn context_of(stage: Option<&str>) -> StageContext {
    let s = stage.unwrap_or("").to_ascii_lowercase();
    if s.contains("review") || s.contains("verify") || s.contains("audit") {
        StageContext::Fresh
    } else {
        StageContext::Warm
    }
}

fn normalized_seats(seats: &[LiveSeat], roles: &JoinRoles) -> Vec<LiveSeat> {
    seats
        .iter()
        .map(|s| {
            let mut s = s.clone();
            s.role = roles.seat_role(s.role.as_str()).into();
            s
        })
        .collect()
}

fn step(action: Action, name: Option<&str>) -> Step {
    Step {
        name: name.map(str::to_string),
        guard: None,
        on_error: toron_workflow::schema::StepOnError::Continue,
        build_slot: false,
        action,
    }
}

/// Every dispatch step, including nested foreach/parallel/retry/branch steps
/// and graph nodes (bd-sfkm8 F14). A plan must bind and count all of them.
fn collect_steps(steps: &[Step], out: &mut Vec<Step>) {
    for step in steps {
        out.push(step.clone());
        match &step.action {
            Action::Retry { step, .. }
            | Action::Foreach { each: step, .. }
            | Action::Parallel { each: step, .. } => collect_steps(std::slice::from_ref(step), out),
            Action::Branch {
                then, else_steps, ..
            } => {
                collect_steps(then, out);
                collect_steps(else_steps, out);
            }
            _ => {}
        }
    }
}

fn apply_to_steps(steps: &mut [Step], bound: &BTreeMap<String, String>, bead: Option<&str>) {
    for step in steps {
        let name = step.name.clone().unwrap_or_default();
        if let Some(pin) = bound.get(&name)
            && let Action::DispatchJob { assignee, spec, .. } = &mut step.action
        {
            *assignee = pin.clone();
            spec.extra.insert(
                "target_pin".to_string(),
                serde_json::Value::String(pin.clone()),
            );
            if let Some(id) = bead.filter(|b| !b.is_empty()) {
                let header =
                    format!("BEAD: {id} (the bound work order; `br show {id}` is the contract)");
                spec.task = Some(match spec.task.take() {
                    Some(task) => format!("{header}\n{task}"),
                    None => header,
                });
            }
        }
        match &mut step.action {
            Action::Retry { step, .. }
            | Action::Foreach { each: step, .. }
            | Action::Parallel { each: step, .. } => {
                apply_to_steps(std::slice::from_mut(step), bound, bead)
            }
            Action::Branch {
                then, else_steps, ..
            } => {
                apply_to_steps(then, bound, bead);
                apply_to_steps(else_steps, bound, bead);
            }
            _ => {}
        }
    }
}

/// Decide every step's seat without touching the fleet. Deterministic: seats
/// are sorted by pin inside the binder, so a replay makes the same choices.
pub fn plan_join(
    workflow: &Workflow,
    seats: &[LiveSeat],
    roles: &JoinRoles,
    crew_max: usize,
) -> JoinPlan {
    let seats = normalized_seats(seats, roles);
    let policy = crate::seat_binder::BindPolicy {
        crew_max,
        headless_allowed: false,
    };
    let mut steps = Vec::new();
    let mut spawns = Vec::new();
    let mut all_steps = Vec::new();
    collect_steps(&workflow.steps, &mut all_steps);
    if let Some(graph) = &workflow.graph {
        for node in graph.nodes.values() {
            collect_steps(
                std::slice::from_ref(&step(node.action.clone(), None)),
                &mut all_steps,
            );
        }
    }
    for step in &all_steps {
        let Some(raw_role) = step_role(step) else {
            continue;
        };
        let context = context_of(stage_of(step).as_deref());
        let stage = stage_of(step);
        // A fresh-stage seat never reviews the work it authored. All same-role
        // seats are candidate authors until the caller narrows them.
        let reviews_author: BTreeSet<String> = if context == StageContext::Fresh {
            seats
                .iter()
                .filter(|s| s.role.as_str() == raw_role)
                .map(|s| s.pin.clone())
                .collect()
        } else {
            BTreeSet::new()
        };
        let need = StepNeed {
            step: step.name.clone().unwrap_or_default(),
            role: raw_role.clone(),
            harness: None,
            model: None,
            mode: None,
            context,
            reviews_author,
        };
        let binding = bind(&need, &seats, &policy);
        let (pin, spawn, wait) = match binding {
            Binding::Reuse { pin }
            | Binding::FreshSession { pin }
            | Binding::Other { pin }
            | Binding::Patch { pin, .. } => (Some(pin), false, false),
            Binding::Spawn => (None, true, false),
            Binding::Headless => (None, false, true),
            Binding::Wait { .. } => (None, false, true),
        };
        if spawn && !spawns.contains(&raw_role) {
            spawns.push(raw_role.clone());
        }
        steps.push(StepBinding {
            step: need.step,
            role: raw_role,
            pin,
            stage,
            spawn,
            wait,
        });
    }
    let needs_yes = !spawns.is_empty();
    JoinPlan {
        steps,
        spawns,
        needs_yes,
    }
}

/// Apply a plan's seat choices to the workflow's dispatch actions: the bound
/// step really dispatches to that pin (no profile re-spawn) and the mission
/// carries the BEAD header, so the receiving seat starts at the right work
/// order through `send --stage <the step's stage>` (bd-3ann1 slice 2).
pub fn apply_join_bindings(workflow: &Workflow, plan: &JoinPlan, bead: Option<&str>) -> Workflow {
    let bound: BTreeMap<String, String> = plan
        .steps
        .iter()
        .filter_map(|b| b.pin.as_ref().map(|pin| (b.step.clone(), pin.clone())))
        .collect();
    let mut wf = workflow.clone();
    apply_to_steps(&mut wf.steps, &bound, bead);
    if let Some(graph) = wf.graph.as_mut() {
        let mut actions: Vec<Action> = graph.nodes.values().map(|n| n.action.clone()).collect();
        let mut steps: Vec<Step> = actions.iter().map(|a| step(a.clone(), None)).collect();
        apply_to_steps(&mut steps, &bound, bead);
        for (node, step) in graph.nodes.values_mut().zip(steps.iter()) {
            node.action = step.action.clone();
        }
        actions.clear();
    }
    wf
}

/// Preview lines for `--dry-run`: every binding is visible before consent.
pub fn render_preview(plan: &JoinPlan) -> Vec<String> {
    plan.steps
        .iter()
        .map(|b| {
            let target = if let Some(pin) = &b.pin {
                format!("seat {pin}")
            } else if b.spawn {
                "NEW PANE (needs --yes)".to_string()
            } else if b.wait {
                "no available seat (wait)".to_string()
            } else {
                "no seat".to_string()
            };
            format!(
                "{} ({}): {}",
                if b.step.is_empty() { "<step>" } else { &b.step },
                b.role,
                target
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_workflow::schema::{DispatchSpec, Step};

    fn seat(pin: &str, role: &str, idle: bool) -> LiveSeat {
        LiveSeat {
            pin: pin.into(),
            role: role.into(),
            harness: "opencode".into(),
            model: None,
            mode: None,
            effort: None,
            placement: crate::seat_binder::SeatPlacement::Pane,
            idle,
            fresh_session: true,
            idle_since_ms: None,
        }
    }

    fn dispatch_step(name: &str, role: &str, stage: &str) -> Step {
        Step {
            name: Some(name.into()),
            guard: None,
            on_error: toron_workflow::schema::StepOnError::Continue,
            build_slot: false,
            action: Action::DispatchJob {
                assignee: "x".into(),
                spec: DispatchSpec {
                    profile: Some(format!("@{role}")),
                    stage: Some(stage.into()),
                    ..Default::default()
                },
                channel: None,
                await_: None,
                no_claim_timeout: 600,
                result_timeout: None,
            },
        }
    }

    fn workflow(steps: Vec<Step>) -> Workflow {
        Workflow {
            name: "joined".into(),
            description: None,
            trigger: None,
            policy: None,
            guard: None,
            steps,
            graph: None,
            inputs: Vec::new(),
            crew: Some(toron_workflow::schema::CrewSpec::Auto),
            principles: Vec::new(),
        }
    }

    fn four_crew() -> Vec<LiveSeat> {
        ["Alpha", "Bravo", "Charlie", "Delta"]
            .into_iter()
            .map(|p| seat(p, "crew", true))
            .collect()
    }

    /// bd-3ann1: a job-bearing compose with four live crews binds execute to a
    /// crew seat and asks only for a reviewer pane, gated by --yes.
    #[test]
    fn join_binds_execute_to_crew_and_spawns_only_a_reviewer() {
        let wf = workflow(vec![
            dispatch_step("execute", "coder", "ce-execute"),
            dispatch_step("review", "reviewer", "ce-review"),
        ]);
        let plan = plan_join(&wf, &four_crew(), &JoinRoles::default(), 8);
        assert_eq!(plan.steps[0].pin.as_deref(), Some("Alpha"));
        assert!(plan.steps[0].stage.as_deref() == Some("ce-execute"));
        assert!(plan.steps[1].spawn, "no reviewer seat: {plan:?}");
        assert_eq!(plan.spawns, vec!["reviewer".to_string()]);
        assert!(plan.needs_yes);
    }

    /// bd-3ann1: a fresh-stage seat never reviews its own author.
    #[test]
    fn a_reviewer_seat_is_never_the_author() {
        let wf = workflow(vec![dispatch_step("review", "reviewer", "ce-review")]);
        let seats = vec![
            seat("Author", "reviewer", true),
            seat("Auditor", "reviewer", true),
        ];
        let mut roles = JoinRoles::default();
        roles.aliases.insert("reviewer".into(), "reviewer".into());
        let plan = plan_join(&wf, &seats, &roles, 8);
        // Both seats are candidate authors, so neither is eligible: the binder
        // waits rather than letting a reviewer audit its own work.
        assert!(plan.steps[0].pin.is_none(), "{plan:?}");
    }

    /// bd-3ann1: preview decides without dispatching — it is a pure fold.
    #[test]
    fn preview_lists_bindings_and_names_the_new_pane() {
        let wf = workflow(vec![
            dispatch_step("execute", "coder", "ce-execute"),
            dispatch_step("review", "reviewer", "ce-review"),
        ]);
        let plan = plan_join(&wf, &four_crew(), &JoinRoles::default(), 8);
        let lines = render_preview(&plan);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("seat Alpha"), "{lines:?}");
        assert!(lines[1].contains("NEW PANE"), "{lines:?}");
        assert!(lines[1].contains("reviewer"), "{lines:?}");
    }

    /// bd-3ann1 slice 2: a bound step dispatches to the chosen pin, keeps its
    /// stage, and carries the BEAD header instead of spawning a profile.
    #[test]
    fn a_bound_step_dispatches_to_the_pin_with_the_bead_header() {
        let wf = workflow(vec![dispatch_step("execute", "coder", "ce-execute")]);
        let plan = plan_join(&wf, &four_crew(), &JoinRoles::default(), 8);
        let bound = apply_join_bindings(&wf, &plan, Some("bd-42"));
        let Action::DispatchJob { assignee, spec, .. } = &bound.steps[0].action else {
            panic!("the step must stay a dispatch: {:?}", bound.steps[0]);
        };
        assert_eq!(assignee, "Alpha");
        assert_eq!(
            spec.profile.as_deref(),
            Some("@coder"),
            "profile must resolve: {spec:?}"
        );
        assert_eq!(
            spec.extra.get("target_pin").and_then(|v| v.as_str()),
            Some("Alpha"),
            "the pin must be a typed dispatch override: {spec:?}"
        );
        assert_eq!(spec.stage.as_deref(), Some("ce-execute"));
        assert!(
            spec.task
                .as_deref()
                .is_some_and(|t| t.starts_with("BEAD: bd-42")),
            "the bound work order must be named: {spec:?}"
        );
    }

    /// bd-3ann1: no bound pin leaves the original dispatch untouched.
    #[test]
    fn an_unbound_step_keeps_its_original_dispatch() {
        let wf = workflow(vec![dispatch_step("review", "reviewer", "ce-review")]);
        let plan = plan_join(&wf, &four_crew(), &JoinRoles::default(), 8);
        let bound = apply_join_bindings(&wf, &plan, Some("bd-42"));
        assert_eq!(
            format!("{:?}", bound.steps[0]),
            format!("{:?}", wf.steps[0])
        );
    }

    /// bd-sfkm8 F14/F18: a nested dispatch is bound and counted, and a wait
    /// binding is named in the preview instead of being silently dropped.
    #[test]
    fn nested_steps_are_bound_and_waits_are_previewed() {
        let nested = Step {
            name: Some("review".into()),
            guard: None,
            on_error: toron_workflow::schema::StepOnError::Continue,
            build_slot: false,
            action: Action::Foreach {
                over: "1".into(),
                each: Box::new(dispatch_step("execute", "coder", "ce-execute")),
            },
        };
        let wf = workflow(vec![nested]);
        let plan = plan_join(&wf, &four_crew(), &JoinRoles::default(), 8);
        let names: Vec<&str> = plan.steps.iter().map(|b| b.step.as_str()).collect();
        assert_eq!(
            names,
            vec!["execute"],
            "the nested dispatch is the bindable step: {names:?}"
        );
        assert_eq!(plan.steps[0].pin.as_deref(), Some("Alpha"));

        let waiting = JoinPlan {
            steps: vec![StepBinding {
                step: "blocked".into(),
                role: "coder".into(),
                pin: None,
                stage: None,
                spawn: false,
                wait: true,
            }],
            spawns: Vec::new(),
            needs_yes: false,
        };
        assert!(
            render_preview(&waiting)[0].contains("wait"),
            "a wait must be visible: {:?}",
            render_preview(&waiting)
        );
    }

    /// bd-3ann1: always spawning ignores four live crews.
    #[test]
    fn a_join_without_live_seats_spawns_every_role() {
        let wf = workflow(vec![dispatch_step("execute", "coder", "ce-execute")]);
        let plan = plan_join(&wf, &[], &JoinRoles::default(), 8);
        assert!(plan.steps[0].spawn);
        assert_eq!(plan.spawns, vec!["coder".to_string()]);
    }
}

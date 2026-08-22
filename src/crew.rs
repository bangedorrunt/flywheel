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
//! governed-by: ADR-0022 §3.6 (compose versatility ladder T1/T2 + safety
//! envelope: full-grammar part forms, --from-yaml, budget caps fail closed,
//! structural dry-run rendering)

use std::collections::BTreeMap;

use anyhow::Result;

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
/// named stage), a stage-first part (a bare stage name — dispatched to the
/// stage's declared role affinity, §3.8), a phase-tagged part (`phase:stage`
/// — affinity-resolved + the §4 lifecycle phase, W3), or a workflow part
/// (`workflow:name` — a `call_workflow` sub-run). Parsed from
/// `flywheel workflow compose --parts`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposePart {
    /// `role:stage` → a dispatch_job step (profile @role, stage prompt).
    Dispatch { role: String, stage: String },
    /// bare `stage` → dispatch to the stage's declared role affinity.
    StageFirst { stage: String },
    /// `phase:stage` → affinity-resolved dispatch carrying the phase.
    PhaseStage { phase: String, stage: String },
    /// `workflow:name` → a call_workflow step (resolved at invoke).
    CallWorkflow { name: String },
    /// `each:<over>:<part>` → foreach sub-runs, one per list item
    /// (`over` is a template ref to the list). ADR-0022 §3.6 T1.
    Each {
        over: String,
        inner: Box<ComposePart>,
    },
    /// `when:<guard>:<part>` → a branch running the part as the single
    /// `then` step when the guard holds. ADR-0022 §3.6 T1.
    When {
        guard: String,
        inner: Box<ComposePart>,
    },
    /// `retry(n):<part>` → bounded retries (n ≥ 1) around the part.
    /// ADR-0022 §3.6 T1.
    Retry {
        max_attempts: u32,
        inner: Box<ComposePart>,
    },
    /// `parallel:<over>:<part>` → elastic fan-out across N copies or a
    /// crew role (`@crew.<role>`). ADR-0022 §3.6 T1.
    Parallel {
        over: String,
        inner: Box<ComposePart>,
    },
    /// ADR-0006 D3: a builtin recipe token (`wave-swarm`) → its compiled-in
    /// multi-phase steps, expanded where the token sits.
    Recipe { name: String },
}

/// Parse one compose part token (`role:stage` | `phase:stage` | `workflow:name`
/// | bare `stage`). Pure — the unit surface for the compose CLI's
/// deterministic arm (bd-8xca, §3.8 stage-first).
pub fn parse_compose_part(token: &str) -> Result<ComposePart, String> {
    let t = token.trim();
    let Some((kind, value)) = t.split_once(':') else {
        // ADR-0006 D3: a builtin recipe token (`wave-swarm`) is checked
        // BEFORE the stage-first fallback — a recipe name shadows any stage
        // literally named the same (recipes are crate-reserved words).
        if t.split(':').count() == 1 && crate::builtin_workflows::is_builtin_recipe(t) {
            return Ok(ComposePart::Recipe {
                name: t.to_string(),
            });
        }
        // §3.8 stage-first: a bare stage name is a valid part.
        let stage = t.trim();
        if stage.is_empty() {
            return Err(format!("compose part {token:?}: empty stage name"));
        }
        // Fail closed (§3.6 envelope #1): a colon-less token SHAPED like a
        // T1 control form — a lone keyword or `retry(n)` with no inner part
        // — is a malformed structured form, never a stage literally named
        // like one. Keywords shadow stage names the way phases shadow roles.
        if matches!(stage, "each" | "when" | "parallel" | "retry") || stage.starts_with("retry(") {
            return Err(format!(
                "compose part {token:?}: incomplete compose form — \
                 control parts need an inner part (`retry(n):<part>` / \
                 `<keyword>:<param>:<part>`)"
            ));
        }
        return Ok(ComposePart::StageFirst {
            stage: stage.to_string(),
        });
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
        // §3.6 T1 structured forms: `each:` / `parallel:` take <over> +
        // inner part; `when:` takes a guard + inner part. Strict — missing
        // or malformed pieces are parse errors, never silently dropped.
        form @ ("each" | "parallel" | "when") => {
            let (param, rest) = value.split_once(':').ok_or_else(|| {
                format!(
                    "compose part {token:?}: {form} needs `{form}:<param>:<part>` — \
                     param and inner part required"
                )
            })?;
            let param = param.trim();
            if param.is_empty() {
                return Err(format!("compose part {token:?}: empty {form} parameter"));
            }
            if rest.trim().is_empty() {
                return Err(format!(
                    "compose part {token:?}: {form} is missing its inner part"
                ));
            }
            let inner = Box::new(
                parse_compose_part(rest)
                    .map_err(|e| format!("compose part {token:?}: invalid inner part ({e})"))?,
            );
            match form {
                "each" => Ok(ComposePart::Each {
                    over: param.to_string(),
                    inner,
                }),
                "parallel" => Ok(ComposePart::Parallel {
                    over: param.to_string(),
                    inner,
                }),
                _ => Ok(ComposePart::When {
                    guard: param.to_string(),
                    inner,
                }),
            }
        }
        // `retry(n):<part>` — the count rides the keyword, so the value is
        // the inner part directly. `retry` without `(n)` is an error (a role
        // named e.g. `retry-team` still parses as a dispatch role).
        kind if kind == "retry" || kind.starts_with("retry(") => {
            let attempts = if kind == "retry" {
                None
            } else {
                kind.strip_prefix("retry(")
                    .and_then(|rest| rest.strip_suffix(')'))
                    .and_then(|n| n.trim().parse::<u32>().ok())
                    .filter(|n| *n >= 1)
            };
            let Some(max_attempts) = attempts else {
                return Err(format!(
                    "compose part {token:?}: expected `retry(n):<part>` with numeric n ≥ 1"
                ));
            };
            if value.trim().is_empty() {
                return Err(format!(
                    "compose part {token:?}: retry({max_attempts}) is missing its inner part"
                ));
            }
            let inner = Box::new(
                parse_compose_part(value)
                    .map_err(|e| format!("compose part {token:?}: invalid inner part ({e})"))?,
            );
            Ok(ComposePart::Retry {
                max_attempts,
                inner,
            })
        }
        phase if crate::stages::PHASES.contains(&phase) => {
            let stage = value.trim();
            if stage.is_empty() {
                return Err(format!("compose part {token:?}: empty stage name"));
            }
            Ok(ComposePart::PhaseStage {
                phase: phase.to_string(),
                stage: stage.to_string(),
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
/// dispatch_job (profile `@role`, stage `stage`), a bare stage → dispatch to
/// the stage's declared affinity, `phase:stage` → affinity + §4 phase,
/// `workflow:name` → a call_workflow. `crew: auto` (the composed fleet is
/// inferred from the parts). Stage-first parts resolve their role through
/// `affinity`; a stage with NO declared affinity is an error (use the
/// explicit `role:stage` form). Pure + testable.
/// [`compose_workflow_with`] without stage affinities — bare-name parts
/// refuse (the §3.8 stage-first resolver is required for them).
#[cfg(test)]
pub fn compose_workflow(
    name: &str,
    objective: &str,
    base: Option<&Workflow>,
    parts: &[ComposePart],
) -> Result<Workflow> {
    compose_workflow_with(name, objective, base, parts, &|_| None)
}

/// Assemble with a stage-affinity resolver (§3.8 stage-first).
pub fn compose_workflow_with(
    name: &str,
    objective: &str,
    base: Option<&Workflow>,
    parts: &[ComposePart],
    affinity: &dyn Fn(&str) -> Option<String>,
) -> Result<Workflow> {
    let base_inputs_snapshot: Vec<InputDecl> = base
        .map(|b| b.inputs.clone())
        .unwrap_or_else(|| vec![InputDecl {
            name: "objective".to_string(),
            default: None,
        }]);
    let mut steps: Vec<Step> = base.map(|b| b.steps.clone()).unwrap_or_default();
    let mut extra_inputs: Vec<InputDecl> = Vec::new();
    let mut ordinal = 0u32;
    for part in parts {
        // ADR-0006 D3: a builtin recipe token expands to its compiled-in
        // steps right where it sits; its declared inputs merge into the
        // composed workflow so the recipe's template refs lint clean.
        if let ComposePart::Recipe { name } = part {
            let expanded = crate::builtin_workflows::builtin_recipe(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "compose part {name:?}: unknown builtin recipe (known: wave-swarm)"
                )
            })?;
            for input in expanded.inputs {
                if !steps_has_input(&extra_inputs, &input.name)
                    && !steps_has_input(&base_inputs_snapshot, &input.name)
                {
                    extra_inputs.push(input);
                }
            }
            steps.extend(expanded.steps);
            ordinal += 1;
            continue;
        }
        ordinal += 1;
        steps.push(build_part_step(part, ordinal, objective, affinity)?);
    }
    Ok(Workflow {
        name: name.to_string(),
        description: Some(format!(
            "Runtime-composed workflow for: {objective} (bd-8xca deterministic arm)"
        )),
        trigger: None,
        policy: None,
        guard: None,
        steps,
        graph: None,
        inputs: {
            let mut inputs = base_inputs_snapshot;
            for input in extra_inputs {
                inputs.push(input);
            }
            inputs
        },
        crew: Some(CrewSpec::Auto),
        principles: vec![],
    })
}

/// Does `inputs` already declare `name`? (recipe-input merge dedup)
fn steps_has_input(inputs: &[InputDecl], name: &str) -> bool {
    inputs.iter().any(|i| i.name == name)
}

/// One part → one step (§3.6 T1): leaf parts (dispatch / stage-first /
/// phase / workflow) build their dispatch or call step; control parts
/// (`each:`/`when:`/`retry(n)`/`parallel:`) wrap the leaf in the matching
/// composite `Action`. Pure + testable.
fn build_part_step(
    part: &ComposePart,
    ordinal: u32,
    objective: &str,
    affinity: &dyn Fn(&str) -> Option<String>,
) -> Result<Step> {
    fn slugify(s: &str) -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    }
    /// The leaf under a control part (or the whole step for a bare leaf).
    fn leaf(
        part: &ComposePart,
        ordinal: u32,
        objective: &str,
        affinity: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Step> {
        let base = |name: String, action: Action| Step {
            name: Some(name),
            guard: None,
            on_error: StepOnError::Continue,
            build_slot: false,
            action,
        };
        Ok(match part {
            ComposePart::Dispatch { role, stage } => {
                // deterministic unique names across repeated roles
                base(
                    format!("{}-{ordinal}", slugify(&format!("{role}-{stage}"))),
                    Action::DispatchJob {
                        assignee: "x".to_string(),
                        spec: toron_workflow::schema::DispatchSpec {
                            profile: Some(format!("@{role}")),
                            stage: Some(stage.clone()),
                            task: Some(objective.to_string()),
                            ..Default::default()
                        },
                        channel: None,
                    },
                )
            }
            ComposePart::StageFirst { stage } | ComposePart::PhaseStage { stage, phase: _ } => {
                let phase = match part {
                    ComposePart::PhaseStage { phase, .. } => Some(phase.clone()),
                    _ => None,
                };
                let role = affinity(stage).ok_or_else(|| {
                    anyhow::anyhow!(
                        "stage {stage:?} declares no role affinity — use role:{stage} or \
                         declare `role` in the stage (stage show {stage})"
                    )
                })?;
                base(
                    format!("{}-{ordinal}", slugify(&format!("{role}-{stage}"))),
                    Action::DispatchJob {
                        assignee: "x".to_string(),
                        spec: toron_workflow::schema::DispatchSpec {
                            profile: Some(format!("@{role}")),
                            stage: Some(stage.clone()),
                            phase,
                            task: Some(objective.to_string()),
                            ..Default::default()
                        },
                        channel: None,
                    },
                )
            }
            ComposePart::CallWorkflow { name: sub } => base(
                format!("part-{ordinal}"),
                Action::CallWorkflow {
                    workflow: sub.clone(),
                    inputs: serde_json::json!({ "goal": objective }),
                },
            ),
            // A control form directly inside another control form wraps a
            // synthetic dispatch-less leaf — unreachable via the parser
            // (inner is always re-parsed to a concrete part), so treat it as
            // a hard error instead of inventing a shape.
            other => anyhow::bail!("compose part: unexpected nested form {other:?}"),
        })
    }
    let wrapper = |action: Action| Step {
        name: Some(format!("part-{ordinal}")),
        guard: None,
        on_error: StepOnError::Continue,
        build_slot: false,
        action,
    };
    match part {
        ComposePart::Each { over, inner } => Ok(wrapper(Action::Foreach {
            over: over.clone(),
            each: Box::new(leaf(inner, ordinal, objective, affinity)?),
        })),
        ComposePart::Parallel { over, inner } => Ok(wrapper(Action::Parallel {
            over: over.clone(),
            each: Box::new(leaf(inner, ordinal, objective, affinity)?),
        })),
        ComposePart::When { guard, inner } => Ok(wrapper(Action::Branch {
            if_guard: guard.clone(),
            then: vec![leaf(inner, ordinal, objective, affinity)?],
            else_steps: vec![],
        })),
        ComposePart::Retry {
            max_attempts,
            inner,
        } => Ok(wrapper(Action::Retry {
            max_attempts: *max_attempts,
            backoff_secs: 0,
            step: Box::new(leaf(inner, ordinal, objective, affinity)?),
        })),
        // ADR-0006 D3: a recipe is multi-step — it never reaches the
        // one-step builder; compose_workflow_with expands it inline.
        ComposePart::Recipe { name } => {
            anyhow::bail!("compose part: recipe `{name}` must expand in the assemble loop")
        }
        leaf_part @ (ComposePart::Dispatch { .. }
        | ComposePart::StageFirst { .. }
        | ComposePart::PhaseStage { .. }
        | ComposePart::CallWorkflow { .. }) => leaf(leaf_part, ordinal, objective, affinity),
    }
}

/// §3.6 safety envelope #3 — budget caps per composed run, fail closed at
/// the compose gate (before lint/run). Dispatch-shaped leaves (`dispatch_job`
/// AND `call_workflow` sub-runs — both spend crew attention) are capped, as
/// is composite nesting depth (retry/branch/foreach/parallel chains).
/// Constants, not config: a composed run that needs more should be split
/// into base + parts (the base-seed mechanism), not tuned past the cap.
pub const MAX_COMPOSED_DISPATCHES: usize = 32;
pub const MAX_COMPOSED_NESTING: usize = 4;

pub fn enforce_budget_caps(workflow: &Workflow) -> Result<()> {
    fn walk(action: &Action, depth: usize, dispatches: &mut usize, max_depth: &mut usize) {
        *max_depth = (*max_depth).max(depth);
        match action {
            Action::DispatchJob { .. } | Action::CallWorkflow { .. } => *dispatches += 1,
            Action::Parallel { each, .. } | Action::Foreach { each, .. } => {
                walk(&each.action, depth + 1, dispatches, max_depth)
            }
            Action::Retry { step, .. } => walk(&step.action, depth + 1, dispatches, max_depth),
            Action::Branch {
                then, else_steps, ..
            } => {
                for s in then.iter().chain(else_steps.iter()) {
                    walk(&s.action, depth + 1, dispatches, max_depth);
                }
            }
            _ => {}
        }
    }
    let mut dispatches = 0usize;
    let mut max_depth = 0usize;
    for s in &workflow.steps {
        walk(&s.action, 1, &mut dispatches, &mut max_depth);
    }
    if let Some(graph) = &workflow.graph {
        for node in graph.nodes.values() {
            walk(&node.action, 1, &mut dispatches, &mut max_depth);
        }
    }
    if dispatches > MAX_COMPOSED_DISPATCHES {
        anyhow::bail!(
            "budget cap exceeded: {dispatches} dispatch/sub-run leaves > max \
             {MAX_COMPOSED_DISPATCHES} per composed run — split into base + parts"
        );
    }
    if max_depth > MAX_COMPOSED_NESTING {
        anyhow::bail!(
            "budget cap exceeded: composite nesting depth {max_depth} > max \
             {MAX_COMPOSED_NESTING} per composed run"
        );
    }
    Ok(())
}

/// T2 author-from-scratch: parse a FULL authored Workflow from YAML text
/// (`compose --from-yaml -` reads stdin). Fail closed on garbage or an
/// empty body — an unrunnable workflow must never reach the lint gate
/// looking green.
pub fn workflow_from_yaml(src: &str) -> Result<Workflow> {
    let wf: Workflow = serde_yaml::from_str(src)
        .map_err(|e| anyhow::anyhow!("compose --from-yaml: invalid workflow YAML: {e}"))?;
    let has_steps = !wf.steps.is_empty() || wf.graph.as_ref().is_some_and(|g| !g.nodes.is_empty());
    if !has_steps {
        anyhow::bail!("compose --from-yaml: workflow has no steps");
    }
    Ok(wf)
}

/// §3.6 safety envelope #2 — structural dry-run rendering: every dispatch/
/// call leaf flattened with its action kind, phase tag, step guard, and the
/// RESOLVED fanout count for enclosing parallel/foreach scopes (@crew.<role>
/// resolved through plan_crew). No dispatch happens; this is pure.
pub fn render_dry_run(
    workflow: &Workflow,
    resolver: &dyn Fn(&str) -> Option<Workflow>,
) -> serde_json::Value {
    #[derive(serde::Serialize)]
    struct DryLeaf {
        name: String,
        path: String,
        action: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        phase: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        guard: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        fanout: Option<u32>,
    }

    let plan = plan_crew(workflow);
    let resolved = resolve_crew_fanouts(workflow, &plan);

    /// Statically RESOLVABLE fan-out only: @crew.<role> members or a bare
    /// N. A foreach over a runtime list ref stays None — rendering a fake
    /// "1" would lie about the shape.
    fn fanout_of(over: &str, plan: &CrewPlan) -> Option<u32> {
        if let Some(role) = over.trim().strip_prefix("@crew.") {
            Some(plan.members.get(role).copied().unwrap_or(1).max(1))
        } else {
            over.trim().parse::<u32>().ok().map(|n| n.max(1))
        }
    }

    fn collect(
        prefix: &str,
        steps: &[Step],
        scope_fanout: Option<u32>,
        plan: &CrewPlan,
        resolver: &dyn Fn(&str) -> Option<Workflow>,
        depth: usize,
        out: &mut Vec<DryLeaf>,
    ) {
        for s in steps {
            collect_action(
                s.name.as_deref().unwrap_or("<unnamed>"),
                prefix,
                &s.action,
                s.guard.as_deref(),
                scope_fanout,
                plan,
                resolver,
                depth,
                out,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_action(
        name: &str,
        prefix: &str,
        action: &Action,
        guard: Option<&str>,
        scope_fanout: Option<u32>,
        plan: &CrewPlan,
        resolver: &dyn Fn(&str) -> Option<Workflow>,
        depth: usize,
        out: &mut Vec<DryLeaf>,
    ) {
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        match action {
            Action::DispatchJob { spec, .. } => out.push(DryLeaf {
                name: name.to_string(),
                path,
                action: "dispatch_job".into(),
                phase: spec.phase.clone(),
                guard: guard.map(str::to_string),
                fanout: scope_fanout,
            }),
            // bd-cvv9 (§8 W3): a RESOLVABLE call_workflow link expands into
            // its linked steps (nested path), so chain built-ins like `ship`
            // render their full 5-link graph with phase tags. Unresolvable
            // links keep the flat leaf; depth-capped against self-referential
            // catalogs.
            Action::CallWorkflow { workflow, .. } if depth < 8 => match resolver(workflow) {
                Some(linked) => {
                    collect(
                        &path,
                        &linked.steps,
                        scope_fanout,
                        plan,
                        resolver,
                        depth + 1,
                        out,
                    );
                }
                None => out.push(DryLeaf {
                    name: name.to_string(),
                    path,
                    action: format!("call_workflow:{workflow}"),
                    phase: None,
                    guard: guard.map(str::to_string),
                    fanout: scope_fanout,
                }),
            },
            Action::CallWorkflow { workflow, .. } => out.push(DryLeaf {
                name: name.to_string(),
                path,
                action: format!("call_workflow:{workflow}"),
                phase: None,
                guard: guard.map(str::to_string),
                fanout: scope_fanout,
            }),
            Action::Parallel { over, each } => {
                let f = fanout_of(over, plan);
                collect_action(
                    name,
                    prefix,
                    &each.action,
                    guard,
                    f,
                    plan,
                    resolver,
                    depth,
                    out,
                )
            }
            Action::Foreach { over, each } => {
                let f = fanout_of(over, plan);
                collect_action(
                    name,
                    prefix,
                    &each.action,
                    guard,
                    f,
                    plan,
                    resolver,
                    depth,
                    out,
                )
            }
            Action::Retry { step, .. } => collect_action(
                name,
                prefix,
                &step.action,
                guard,
                scope_fanout,
                plan,
                resolver,
                depth,
                out,
            ),
            Action::Branch {
                then, else_steps, ..
            } => {
                let then_path = format!("{path}/then");
                let else_path = format!("{path}/else");
                collect(&then_path, then, scope_fanout, plan, resolver, depth, out);
                collect(
                    &else_path,
                    else_steps,
                    scope_fanout,
                    plan,
                    resolver,
                    depth,
                    out,
                );
            }
            other => out.push(DryLeaf {
                name: name.to_string(),
                path,
                action: other.type_name().to_string(),
                phase: None,
                guard: guard.map(str::to_string),
                fanout: None,
            }),
        }
    }

    let mut leaves = Vec::new();
    collect("", &resolved.steps, None, &plan, resolver, 0, &mut leaves);
    serde_json::json!({
        "workflow": resolved.name,
        "crew": plan.members,
        "steps": leaves,
    })
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

/// §3.6 safety envelope #3, wall-clock half (ADR-0022): a composed run is
/// bounded in time. The bound errors THIS process attempt only — the
/// durable run stays resumable on the engine store, and `--keep` sees a
/// non-green outcome and refuses, so a timed-out composition can never
/// graduate into the catalog.
pub async fn bounded_invoke(
    engine: &toron_workflow::WorkflowEngine,
    run_id: &str,
    wf: Workflow,
    inputs: serde_json::Value,
    max_seconds: u64,
) -> Result<serde_json::Value> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(max_seconds),
        engine.invoke(run_id, wf, inputs),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "composed run {run_id} exceeded the wall-clock budget ({max_seconds}s) — the durable \
             run stays resumable; raise FLYWHEEL_COMPOSE_MAX_SECONDS or slim the composition"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_workflow::schema::parse_workflow;

    /// §3.6 envelope #3 (wall-clock): a composed run whose leaves outlive
    /// the cap fails with the wall-clock error naming the remedy — never a
    /// silent hang. RED before `bounded_invoke` existed: the compose arm
    /// invoked with no time bound at all.
    #[tokio::test(flavor = "multi_thread")]
    async fn bounded_invoke_enforces_wall_clock_cap() {
        let runner: toron_workflow::ActionRunner = std::sync::Arc::new(|_a: &Action| {
            std::thread::sleep(std::time::Duration::from_millis(700));
            Ok(serde_json::json!({ "ok": true }))
        });
        let engine = toron_workflow::WorkflowEngine::local(runner).expect("local engine");
        let wf = parse_workflow(
            "name: t\nsteps:\n  - name: a\n    action: send_message\n    to: [x]\n    subject: s\n    body: b\n  - name: b\n    action: send_message\n    to: [x]\n    subject: s\n    body: b\n",
        )
        .expect("parse");
        let err = bounded_invoke(&engine, "wallclock-cap-test", wf, serde_json::json!({}), 1)
            .await
            .expect_err("two 700ms leaves cannot fit a 1s cap");
        assert!(
            err.to_string().contains("wall-clock budget"),
            "error names the cap: {err:#}"
        );
        // Control: the same helper under a generous cap completes normally —
        // the bound is a budget, not a blanket failure.
        let fast: toron_workflow::ActionRunner =
            std::sync::Arc::new(|_a: &Action| Ok(serde_json::json!({ "ok": true })));
        let engine2 = toron_workflow::WorkflowEngine::local(fast).expect("local engine 2");
        let wf2 = parse_workflow(
            "name: t\nsteps:\n  - name: a\n    action: send_message\n    to: [x]\n    subject: s\n    body: b\n",
        )
        .expect("parse");
        let out = bounded_invoke(&engine2, "wallclock-cap-ok", wf2, serde_json::json!({}), 60)
            .await
            .expect("under the cap completes");
        assert_eq!(out["a"]["ok"], true, "under-cap run completes: {out}");
    }

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
    /// malformed tokens error. (bd-8xca + §3.8 stage-first.)
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
        assert!(parse_compose_part(":stage").is_err(), "empty role errors");
        assert!(
            parse_compose_part("workflow:").is_err(),
            "empty name errors"
        );
    }

    /// W6 (§3.8 stage-first): a bare stage name parses to StageFirst;
    /// `phase:stage` parses to PhaseStage for the §4 phases.
    #[test]
    fn w6_parse_stage_first_and_phase_stage() {
        assert_eq!(
            parse_compose_part("fresh-eyes-explore").unwrap(),
            ComposePart::StageFirst {
                stage: "fresh-eyes-explore".into(),
            }
        );
        assert_eq!(
            parse_compose_part("review:code-review").unwrap(),
            ComposePart::PhaseStage {
                phase: "review".into(),
                stage: "code-review".into(),
            }
        );
        // A role named like a phase is shadowed by the phase grammar — the
        // explicit form for a stage under a @reviewer ROLE is reviewer:stage.
        assert_eq!(
            parse_compose_part("plan:decompose-beads").unwrap(),
            ComposePart::PhaseStage {
                phase: "plan".into(),
                stage: "decompose-beads".into(),
            }
        );
    }

    /// W6: stage-first parts resolve the role through the declared affinity;
    /// an affinity-less stage is an ERROR pointing at the explicit form.
    #[test]
    fn w6_compose_stage_first_resolves_affinity() {
        use toron_workflow::schema::Action;
        let parts = vec![
            ComposePart::StageFirst {
                stage: "fresh-eyes-explore".into(),
            },
            ComposePart::PhaseStage {
                phase: "implement".into(),
                stage: "systematic-execute".into(),
            },
        ];
        let lib = crate::stages::StageLibrary::compiled_in();
        let affinity = |s: &str| lib.get_stage(s).and_then(|st| st.role.clone());
        let wf =
            compose_workflow_with("composed-w6", "the objective", None, &parts, &affinity).unwrap();
        let roles: Vec<Option<&str>> = wf
            .steps
            .iter()
            .map(|s| match &s.action {
                Action::DispatchJob { spec, .. } => spec.profile.as_deref(),
                _ => None,
            })
            .collect();
        assert_eq!(roles, vec![Some("@scout"), Some("@coder")], "{roles:?}");
        // The phase rides the spec (W2 contract).
        let phased = match &wf.steps[1].action {
            Action::DispatchJob { spec, .. } => spec.phase.clone(),
            _ => None,
        };
        assert_eq!(phased.as_deref(), Some("implement"));
        // No affinity → hard error with remedy.
        let bare = vec![ComposePart::StageFirst {
            stage: "no-affinity-stage".into(),
        }];
        let err = compose_workflow_with("composed-x", "obj", None, &bare, &affinity)
            .expect_err("affinity-less stage refuses");
        assert!(err.to_string().contains("role:no-affinity-stage"));
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
        let wf = compose_workflow("composed-x", "validate the new exchange", None, &parts).unwrap();
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
        let wf = compose_workflow("composed-base", "harden the loop", Some(&base), &parts).unwrap();
        assert_eq!(wf.steps.len(), 2, "base step + one part");
        assert!(matches!(&wf.steps[0].action, Action::SendDm { .. }));
        assert!(matches!(
            &wf.steps[1].action,
            Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@coder")
        ));
    }

    // ---- bd-adr22-t1-grammar-residual-zjk0: §3.6 Tier-1 grammar + envelope ----

    /// T1 grammar: `each:` / `when:` / `retry(n)` / `parallel:` cover the rest
    /// of the Action vocabulary; the inner part nests recursively (a control
    /// form wraps ANY part, including another control form).
    #[test]
    fn t1_parse_grammar_part_forms() {
        assert_eq!(
            parse_compose_part("each:@items:scout:fresh-eyes-explore").unwrap(),
            ComposePart::Each {
                over: "@items".into(),
                inner: Box::new(ComposePart::Dispatch {
                    role: "scout".into(),
                    stage: "fresh-eyes-explore".into(),
                }),
            }
        );
        // Bare-name inner parts nest too (stage-first composes with T1).
        assert_eq!(
            parse_compose_part("each:items:fresh-eyes-explore").unwrap(),
            ComposePart::Each {
                over: "items".into(),
                inner: Box::new(ComposePart::StageFirst {
                    stage: "fresh-eyes-explore".into(),
                }),
            }
        );
        assert_eq!(
            parse_compose_part("when:steps.gate.ok == true:reviewer:code-review").unwrap(),
            ComposePart::When {
                guard: "steps.gate.ok == true".into(),
                inner: Box::new(ComposePart::Dispatch {
                    role: "reviewer".into(),
                    stage: "code-review".into(),
                }),
            }
        );
        assert_eq!(
            parse_compose_part("retry(3):coder:systematic-execute").unwrap(),
            ComposePart::Retry {
                max_attempts: 3,
                inner: Box::new(ComposePart::Dispatch {
                    role: "coder".into(),
                    stage: "systematic-execute".into(),
                }),
            }
        );
        assert_eq!(
            parse_compose_part("parallel:@crew.coder:workflow:weekly-review").unwrap(),
            ComposePart::Parallel {
                over: "@crew.coder".into(),
                inner: Box::new(ComposePart::CallWorkflow {
                    name: "weekly-review".into(),
                }),
            }
        );
        let nested = parse_compose_part("each:items:retry(2):deploy:app").unwrap();
        assert!(
            matches!(&nested, ComposePart::Each { inner, .. }
                if matches!(inner.as_ref(), ComposePart::Retry { max_attempts: 2, .. })),
            "{nested:?}"
        );
    }

    /// Unknown/malformed STRUCTURED forms are parse errors, never silently
    /// dropped (§3.6 envelope #1). Ordinary `role:stage` dispatch keeps
    /// working — only the new keywords gain strict grammar.
    #[test]
    fn t1_parse_grammar_errors_fail_closed() {
        for bad in [
            "each:items",       // missing inner part
            "each::stage",      // empty over
            "each:items:",      // empty inner
            "when:guard-only",  // missing inner part
            "when::reviewer:r", // empty guard
            "retry:s:stage",    // retry without (n)
            "retry(n):s:stage", // non-numeric n
            "retry(0):s:stage", // zero attempts
            "retry(2)",         // missing inner part
            "parallel:@crew.c", // missing inner part
        ] {
            assert!(
                parse_compose_part(bad).is_err(),
                "{bad:?} must be a parse error"
            );
        }
        assert!(
            parse_compose_part("lead:oncall").is_ok(),
            "role:stage unaffected"
        );
    }

    /// T1 assembly: each → Foreach, when → Branch(then=1), retry(n) → Retry,
    /// parallel → Parallel; the wrapped leaf keeps its role/stage/workflow.
    /// The result lints clean and passes budget caps.
    #[test]
    fn t1_compose_grammar_parts_assemble_actions() {
        use toron_workflow::schema::Action;
        let lib = crate::stages::StageLibrary::compiled_in();
        let affinity = |s: &str| lib.get_stage(s).and_then(|st| st.role.clone());
        let parts: Vec<ComposePart> = [
            "each:@items:scout:fresh-eyes-explore",
            "retry(3):workflow:weekly-review",
            "when:steps.a.ok:reviewer:code-review",
            "parallel:2:coder:systematic-execute",
        ]
        .iter()
        .map(|t| parse_compose_part(t).unwrap())
        .collect();
        let wf = compose_workflow_with("composed-t1", "obj", None, &parts, &affinity).unwrap();        assert_eq!(wf.steps.len(), 4);
        assert!(matches!(
            &wf.steps[0].action,
            Action::Foreach { over, each }
                if over == "@items"
                    && matches!(&each.action,
                        Action::DispatchJob { spec, .. }
                            if spec.profile.as_deref() == Some("@scout"))
        ));
        assert!(matches!(
            &wf.steps[1].action,
            Action::Retry { max_attempts: 3, step, .. }
                if matches!(&step.action,
                    Action::CallWorkflow { workflow, .. } if workflow == "weekly-review")
        ));
        assert!(matches!(
            &wf.steps[2].action,
            Action::Branch { if_guard, then, else_steps }
                if if_guard == "steps.a.ok" && then.len() == 1 && else_steps.is_empty()
                    && matches!(&then[0].action,
                        Action::DispatchJob { spec, .. }
                            if spec.profile.as_deref() == Some("@reviewer"))
        ));
        assert!(matches!(
            &wf.steps[3].action,
            Action::Parallel { over, each }
                if over == "2"
                    && matches!(&each.action,
                        Action::DispatchJob { spec, .. }
                            if spec.profile.as_deref() == Some("@coder"))
        ));
        let report = toron_workflow::lint_workflow(&wf);
        assert!(
            report.is_clean(),
            "T1-composed workflow must lint clean: {:?}",
            report.issues
        );
        enforce_budget_caps(&wf).expect("within caps");
    }

    /// ADR-0006 D3: the `wave-swarm` builtin recipe token expands to the
    /// five-phase workflow — GATE → DISPATCH → OBSERVE → VERIFY → SIGNOFF —
    /// and the expansion lints clean.
    #[test]
    fn wave_swarm_recipe_expands_five_phases_and_lints_clean() {
        use toron_workflow::schema::Action;
        let lib = crate::stages::StageLibrary::compiled_in();
        let affinity = |s: &str| lib.get_stage(s).and_then(|st| st.role.clone());
        let part = parse_compose_part("wave-swarm").expect("builtin recipe token parses");
        let wf =
            compose_workflow_with("composed-wave", "obj", None, &[part], &affinity).unwrap();
        assert_eq!(wf.steps.len(), 6, "{:?}", wf.steps.iter().map(|s| &s.name));
        assert!(
            matches!(&wf.steps[0].action, Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@planner")
                    && spec.stage.as_deref() == Some("wave-gate")),
            "GATE first: {:?}",
            wf.steps[0].action.type_name()
        );
        assert!(
            matches!(&wf.steps[1].action, Action::DispatchJob { spec, .. }
                if spec.stage.as_deref() == Some("wave-dispatch")),
            "DISPATCH second"
        );
        assert!(
            matches!(&wf.steps[2].action, Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@coder")
                    && spec.stage.as_deref() == Some("systematic-execute")),
            "EXECUTE third (bd-wave-swarm-graduation-fix-98io: the crew needs an executor)"
        );
        assert!(
            matches!(&wf.steps[3].action, Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@scout")
                    && spec.stage.as_deref() == Some("wave-observe")),
            "OBSERVE fourth"
        );
        assert!(
            matches!(&wf.steps[4].action, Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@reviewer")
                    && spec.stage.as_deref() == Some("wave-verify")),
            "VERIFY fifth"
        );
        assert!(
            matches!(&wf.steps[5].action, Action::RequestApproval { message }
                if message.contains("captain")),
            "SIGNOFF last, captain-only: {:?}",
            wf.steps[5].action.type_name()
        );
        let report = toron_workflow::lint_workflow(&wf);
        assert!(
            report.is_clean(),
            "wave-swarm expansion must lint clean: {:?}",
            report.issues
        );
    }

    /// ADR-0006 D3: a recipe token mixes with ordinary parts (recipe steps
    /// land where the token sits); unknown recipe-looking tokens stay plain
    /// role parts (the role fallback is untouched).
    #[test]
    fn wave_swarm_recipe_mixes_with_plain_parts() {
        let lib = crate::stages::StageLibrary::compiled_in();
        let affinity = |s: &str| lib.get_stage(s).and_then(|st| st.role.clone());
        let parts: Vec<ComposePart> = ["scout:fresh-eyes-explore", "wave-swarm"]
            .iter()
            .map(|t| parse_compose_part(t).unwrap())
            .collect();
        let wf = compose_workflow_with("mixed", "obj", None, &parts, &affinity).unwrap();
        assert_eq!(wf.steps.len(), 7, "1 plain + 6 recipe");
    }

    /// §3.6 envelope #3: budget caps fail closed at the compose gate.
    #[test]
    fn budget_caps_fail_closed() {
        use toron_workflow::schema::{Action, Step};
        // Over-cap dispatch leaves refuse.
        let parts: Vec<ComposePart> = (0..MAX_COMPOSED_DISPATCHES as u32 + 1)
            .map(|i| parse_compose_part(&format!("r{i}:stage{i}")).unwrap())
            .collect();
        let wf = compose_workflow("composed-cap", "obj", None, &parts).unwrap();
        let err = enforce_budget_caps(&wf).expect_err("over-cap dispatches must fail closed");
        assert!(err.to_string().contains("dispatch"), "{err}");
        // A too-deep composite chain refuses regardless of leaf count.
        let leaf = Step {
            name: Some("leaf".into()),
            guard: None,
            on_error: toron_workflow::schema::StepOnError::Continue,
            build_slot: false,
            action: Action::SendDm {
                to: "x".into(),
                body: "y".into(),
                project: None,
            },
        };
        let mut deep = leaf.clone();
        for _ in 0..MAX_COMPOSED_NESTING + 2 {
            deep = Step {
                name: Some("wrap".into()),
                guard: None,
                on_error: toron_workflow::schema::StepOnError::Continue,
                build_slot: false,
                action: Action::Retry {
                    max_attempts: 2,
                    backoff_secs: 0,
                    step: Box::new(deep),
                },
            };
        }
        let wf = Workflow {
            name: "composed-deep".into(),
            description: None,
            trigger: None,
            policy: None,
            guard: None,
            steps: vec![deep],
            graph: None,
            inputs: vec![],
            crew: None,
            principles: vec![],
        };
        let err = enforce_budget_caps(&wf).expect_err("over-depth nesting must fail closed");
        assert!(err.to_string().contains("nesting"), "{err}");
    }

    /// §3.6 envelope #2: structural dry-run renders fanout counts (resolved
    /// from the crew plan) + phase tags, without any dispatch.
    #[test]
    fn dry_run_renders_fanouts_and_phase_tags() {
        let wf = parse_workflow(
            r##"
name: ship
trigger: { type: webhook }
crew: { coder: 3 }
steps:
  - name: fan
    action: parallel
    over: "@crew.coder"
    each:
      action: dispatch_job
      assignee: x
      spec: { profile: "@coder", stage: systematic-execute, phase: implement }
"##,
        )
        .expect("parse");
        let rendered = render_dry_run(&wf, &|_| None);
        let s = serde_json::to_string(&rendered).unwrap();
        assert!(s.contains("\"fanout\":3"), "fanout count missing: {s}");
        assert!(s.contains("implement"), "phase tag missing: {s}");
    }

    /// An UNRESOLVABLE call_workflow keeps the flat leaf (never a silent
    /// expansion of nothing).
    #[test]
    fn dry_run_keeps_flat_leaf_for_unresolvable_link() {
        let wf = parse_workflow(
            r##"
name: chain
trigger: { type: webhook }
crew: auto
steps:
  - name: hop
    action: call_workflow
    workflow: not-anywhere
"##,
        )
        .expect("parse");
        let rendered = render_dry_run(&wf, &|_| None);
        let steps = rendered["steps"].as_array().expect("steps array");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0]["action"], "call_workflow:not-anywhere");
    }

    /// T2: a FULL authored Workflow round-trips through --from-yaml; an
    /// empty-bodied workflow refuses (fail closed).
    #[test]
    fn from_yaml_round_trip_full_workflow() {
        let src = r##"
name: authored
description: captain-authored
trigger: { type: webhook }
crew: auto
steps:
  - name: gate
    action: request_approval
    message: "go?"
  - name: fan
    action: parallel
    over: "@crew.coder"
    each: { action: send_dm, to: x, body: y }
"##;
        let wf = workflow_from_yaml(src).expect("round trip");
        assert_eq!(wf.name, "authored");
        assert_eq!(wf.steps.len(), 2);
        assert!(matches!(
            &wf.steps[1].action,
            toron_workflow::schema::Action::Parallel { over, .. } if over == "@crew.coder"
        ));
        assert!(
            workflow_from_yaml("name: empty\ntrigger: { type: webhook }\nsteps: []\n").is_err(),
            "step-less workflow refuses"
        );
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

// governed-by: ADR-0024
// governed-by: ADR-0011
//! Auto-crew / smart steering (ADR-0005 Spec 4c / P3.6, ADR-0011 D2).
//!
//! The orchestrator smartly steers the crew FROM the workflow. The captain
//! writes INTENT (roles); this module computes the crew the orchestrator
//! spawns. The ENGINE never spawns — it consumes fan-out counts via the
//! injected `ProfileResolver` seam. This is the flywheel-side crew planner:
//!
//! - **Auto** (`crew: auto`): unique roles from `@profile` refs (two
//!   `@planner` steps ⇒ one planner). YAML counts are a role inventory,
//!   not a spawn lock.
//! - **Pack** (`pack_crew`): unique roles into total N, cap `crew.max`
//!   (default 5). Overflow roles alias onto the first kept role.
//! - **`@crew.<role>` fan-out**: copies = packed member count for that role.
//!
//! governed-by: ADR-0005 Spec 4c, P3.6
//! governed-by: ADR-0022 §3.6 (compose versatility ladder T1/T2 + safety
//! envelope: full-grammar part forms, --from-yaml, budget caps fail closed,
//! structural dry-run rendering)
//! governed-by: ADR-0011 D2 (crew budget N)

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

    /// Sum of spawned panes.
    #[cfg(test)]
    pub fn total(&self) -> u32 {
        self.members.values().copied().sum()
    }
}

/// Operator-owned pane cap (ADR-0011 D2). Repo config must not raise this.
pub const DEFAULT_CREW_MAX: u32 = 5;

/// bd-0s0y: what a workflow's steps actually cost in crew panes.
///
/// The busy-owner fence releases on PERMIT DROP, so once a dispatch returns
/// the agent is free — verified by `fenced_permit_drop_releases_busy_holder`
/// in toron-workflow's dispatch tests. Sequential reuse of a role is
/// therefore FREE and needs no extra pane. CONCURRENCY is the only thing that
/// costs a pane, because concurrent sends aimed at one agent are refused.
///
/// That makes the old `over: "3"` over `@oracle` a guaranteed run-killer: it
/// is one node, one role, three concurrent copies, and a crew holds one pane
/// per role. `required_panes` is the honest cost of the widest wave;
/// `collisions` is the set of roles a single wave uses more than once, each of
/// which is a busy-fence refusal waiting to happen.
///
/// `CallWorkflow` children are NOT traversed: they are only reachable through
/// the injected `WorkflowResolver`, so a per-document answer is exact for its
/// own document and silent about children. Callers that have the catalog in
/// scope should check each builtin; that is how `goal`'s cost is pinned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrewDemand {
    /// Distinct roles dispatched in the widest concurrent wave.
    pub required_panes: u32,
    /// (role, times-in-one-wave) where the count exceeds one.
    pub collisions: Vec<(String, usize)>,
    /// The crew SHAPE a correct packer would have to build: role → the most
    /// copies of it any single wave dispatches to concurrently.
    pub crew_shape: BTreeMap<String, u32>,
    /// `crew_shape` summed. This — not `required_panes` — is what the cap has
    /// to cover, and it is why a role needing 3 concurrent copies costs 3
    /// panes even though it is one role.
    pub crew_total: u32,
}

/// The roles one action dispatches, each with its concurrency width. A
/// `parallel` over N contributes its inner roles N times over.
fn role_uses(action: &Action, inputs: &BTreeMap<String, String>, out: &mut Vec<String>) {
    match action {
        Action::DispatchJob { spec, .. } => {
            if let Some(p) = spec.profile.as_deref() {
                out.push(p.trim_start_matches('@').to_string());
            }
        }
        Action::Parallel { over, each } => {
            let n = resolve_over(over, inputs);
            let mut inner = Vec::new();
            role_uses(&each.action, inputs, &mut inner);
            for r in inner {
                for _ in 0..n {
                    out.push(r.clone());
                }
            }
        }
        // bd-q2gw: a `parallel` nested in a control-flow arm is still a
        // fan-out. `Branch` arms are exclusive, so the honest number is the
        // WORST arm, not their sum — a `parallel` in `then` and a different
        // role in `else` never need two panes at once. Retry/Foreach/Loop
        // bodies are not concurrent with anything, so they contribute their
        // own roles at their own width.
        Action::Branch {
            then, else_steps, ..
        } => {
            let mut worst: Vec<String> = Vec::new();
            let mut arm: Vec<String> = Vec::new();
            for s in then {
                arm.clear();
                role_uses(&s.action, inputs, &mut arm);
                if arm.len() > worst.len() {
                    worst = arm.clone();
                }
            }
            for s in else_steps {
                arm.clear();
                role_uses(&s.action, inputs, &mut arm);
                if arm.len() > worst.len() {
                    worst = arm.clone();
                }
            }
            out.extend(worst);
        }
        Action::Retry { step, .. } => role_uses(&step.action, inputs, out),
        Action::Foreach { each, .. } => role_uses(&each.action, inputs, out),
        Action::Loop { body, .. } => {
            for s in body {
                role_uses(&s.action, inputs, out);
            }
        }
        _ => {}
    }
}

/// bd-q2gw: how many copies a `parallel` really runs.
///
/// `over` is not always a literal. `over: "{{inputs.reviewers}}"` with
/// `reviewers` declared `default: "2"` genuinely runs TWO concurrent copies —
/// the engine binds the default and resolves the template — but parsing the
/// raw string as a number yields 1, so the packer seated one pane and the
/// busy-owner fence refused copy 1. That killed `interrogate`, `prototype` and
/// `arena` while the catalog fence reported them clean.
///
/// Unresolvable stays 1: a width we cannot read must not be guessed upward,
/// because over-seating wastes panes against a hard cap of 5.
fn resolve_over(over: &str, inputs: &BTreeMap<String, String>) -> u32 {
    let raw = over.trim();
    let templated = raw
        .strip_prefix("{{inputs.")
        .and_then(|rest| rest.strip_suffix("}}"))
        .map(|name| name.trim());
    let n = match templated {
        Some(name) => inputs.get(name).and_then(|d| d.trim().parse::<u32>().ok()),
        None => raw.parse::<u32>().ok(),
    };
    n.unwrap_or(1).max(1)
}

/// bd-0s0y: the crew a workflow needs, and every way it would self-collide.
/// Pure — no registry, no panes, no herdr. The catalog-level test is what
/// makes it worth having.
pub fn crew_demand(wf: &Workflow) -> CrewDemand {
    // Declared input defaults, so a templated `over: "{{inputs.reviewers}}"`
    // resolves to the width the engine will really run (bd-q2gw).
    let inputs: BTreeMap<String, String> = wf
        .inputs
        .iter()
        .map(|d| {
            (
                d.name.clone(),
                d.default
                    .as_ref()
                    .map(|v| match v {
                        serde_json::Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default(),
            )
        })
        .collect();

    // (node, needs, roles, seed wave). `steps:` is strictly ordered, so each
    // step is its own wave — two steps reusing a role there is sequential
    // reuse, not concurrency, and must never be flagged.
    let nodes: Vec<(String, Vec<String>, Vec<String>, u32)> = match &wf.graph {
        Some(g) => g
            .nodes
            .iter()
            .map(|(name, node)| {
                let mut roles = Vec::new();
                role_uses(&node.action, &inputs, &mut roles);
                (name.clone(), node.needs.clone(), roles, 0)
            })
            .collect(),
        None => wf
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut roles = Vec::new();
                role_uses(&s.action, &inputs, &mut roles);
                (
                    s.name.clone().unwrap_or_default(),
                    Vec::new(),
                    roles,
                    i as u32,
                )
            })
            .collect(),
    };

    // Longest-path layering: a node's wave is one past its deepest dependency,
    // so a node shares a wave with its peers and never with its ancestors. A
    // `steps:` seed already separates them, and a step declares no `needs`, so
    // the layering leaves the seed untouched.
    let mut wave: BTreeMap<&str, u32> = BTreeMap::new();
    let mut pending: Vec<&(String, Vec<String>, Vec<String>, u32)> = nodes.iter().collect();
    let mut guard = nodes.len() + 1;
    while !pending.is_empty() && guard > 0 {
        guard -= 1;
        let (ready, waiting): (Vec<_>, Vec<_>) = pending
            .into_iter()
            .partition(|(_, needs, _, _)| needs.iter().all(|d| wave.contains_key(d.as_str())));
        if ready.is_empty() {
            break;
        }
        for (name, needs, _, seed) in ready {
            let w = needs
                .iter()
                .filter_map(|d| wave.get(d.as_str()).copied())
                .max()
                .map_or(*seed, |m| m + 1);
            wave.insert(name.as_str(), w);
        }
        pending = waiting;
    }

    let mut per_wave: BTreeMap<u32, BTreeMap<&str, usize>> = BTreeMap::new();
    // A role's copy count is its busiest single wave, NOT its total uses:
    // sequential reuse is free, so a role used 3 times in a chain needs 1 pane.
    let mut shape: BTreeMap<String, u32> = BTreeMap::new();
    for (name, _, roles, seed) in &nodes {
        let w = wave.get(name.as_str()).copied().unwrap_or(*seed);
        let bucket = per_wave.entry(w).or_default();
        for r in roles {
            *bucket.entry(r.as_str()).or_insert(0) += 1;
        }
    }
    for bucket in per_wave.values() {
        for (role, n) in bucket {
            let n = *n as u32;
            let slot = shape.entry((*role).to_string()).or_insert(0);
            *slot = (*slot).max(n);
        }
    }
    shape.retain(|_, n| *n > 0);

    let required_panes = per_wave.values().map(|b| b.len() as u32).max().unwrap_or(0);
    let mut collisions: Vec<(String, usize)> = shape
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(r, n)| (r.clone(), *n as usize))
        .collect();
    collisions.sort();
    let crew_total = shape.values().sum();
    CrewDemand {
        required_panes,
        collisions,
        crew_shape: shape,
        crew_total,
    }
}

/// Packed spawn plan: members to spawn + overflow role aliases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackedCrew {
    pub members: BTreeMap<String, u32>,
    /// overflow role → host role (dispatch rewrites `@overflow` to `@host`)
    pub aliases: BTreeMap<String, String>,
    pub budget: u32,
    pub unique: u32,
    pub cap: u32,
}

impl PackedCrew {
    pub fn total(&self) -> u32 {
        self.members.values().copied().sum()
    }

    pub fn as_plan(&self) -> CrewPlan {
        CrewPlan {
            members: self.members.clone(),
        }
    }

    /// One-line human/robot announcement (ADR-0011 D2 loud).
    pub fn announce(&self, workflow: &str) -> String {
        let mut kept: Vec<&str> = self.members.keys().map(String::as_str).collect();
        kept.sort_unstable();
        let mut extra = String::new();
        if !self.aliases.is_empty() {
            let mut bits: Vec<String> = self
                .aliases
                .iter()
                .map(|(from, to)| format!("{from} → {to}"))
                .collect();
            bits.sort();
            extra = format!(" ({})", bits.join(", "));
        }
        format!(
            "packing {workflow} into {} panes (cap={}, unique={}): {}{extra}",
            self.total(),
            self.cap,
            self.unique,
            kept.join(", ")
        )
    }
}

/// Keep-priority for packing when unique > N (ADR-0011 D2).
pub fn role_keep_priority(role: &str) -> u32 {
    match role {
        "coder" => 0,
        "reviewer" => 1,
        "planner" => 2,
        "scout" => 3,
        "oracle" => 4,
        "housekeeper" => 5,
        _ => 100,
    }
}

/// `--crew` flag parse (ADR-0011 D2). Bare integer = total N.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrewFlag {
    Default,
    Total(u32),
    Map(BTreeMap<String, u32>),
    Named(String),
}

pub fn parse_crew_flag(s: &str) -> CrewFlag {
    let t = s.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("auto") {
        return CrewFlag::Default;
    }
    if let Ok(n) = t.parse::<u32>() {
        return CrewFlag::Total(n);
    }
    if t.contains(':') {
        let mut map = BTreeMap::new();
        for part in t.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let Some((role, count)) = part.split_once(':') else {
                return CrewFlag::Named(t.to_string());
            };
            let role = role.trim().trim_start_matches('@');
            let Ok(n) = count.trim().parse::<u32>() else {
                return CrewFlag::Named(t.to_string());
            };
            if role.is_empty() || n == 0 {
                return CrewFlag::Named(t.to_string());
            }
            map.insert(role.to_string(), n);
        }
        if map.is_empty() {
            return CrewFlag::Named(t.to_string());
        }
        return CrewFlag::Map(map);
    }
    CrewFlag::Named(t.to_string())
}

fn sort_roles(roles: &mut [String]) {
    roles.sort_by(|a, b| {
        role_keep_priority(a)
            .cmp(&role_keep_priority(b))
            .then_with(|| a.cmp(b))
    });
}

/// Pack unique roles into total N (ADR-0011 D2). Fails closed when N > max.
pub fn pack_crew(unique: &[String], n: u32, max: u32) -> Result<PackedCrew, String> {
    // Every role gets exactly one pane — the pre-bd-0s0y shape, kept as the
    // general entry point so its callers and tests are unchanged.
    let counts: BTreeMap<String, u32> = unique
        .iter()
        .map(|r| r.trim_start_matches('@').to_string())
        .filter(|r| !r.is_empty())
        .map(|r| (r, 1u32))
        .collect();
    pack_crew_shape(&counts, n, max)
}

/// bd-0s0y: pack a crew from the SHAPE a workflow actually needs — role → the
/// most copies of it any single concurrent wave dispatches to.
///
/// `pack_crew` only ever received role NAMES, so a role dispatched
/// concurrently three times (`parallel over: "3"` over `@oracle`) was packed to
/// one pane and the busy-owner fence refused copies 2..3 at run time. The count
/// was known — [`crew_demand`] computes it — it was just thrown away here.
///
/// Folding rules, and the asymmetry that matters:
/// - A role needing ONE pane can be aliased onto a host for free, because
///   sequential reuse costs nothing (the fence releases on permit drop). This
///   is the pre-existing behaviour and it is preserved exactly.
/// - A role needing SEVERAL panes must NOT be folded: two concurrent copies
///   aimed at one agent is the refusal this whole change exists to prevent.
///   So when the shape needs more panes than the budget, single-copy roles are
///   folded first and, if that is not enough, the run fails LOUD naming the
///   role and the shortfall rather than collapsing it onto a busy pane.
pub fn pack_crew_shape(
    counts: &BTreeMap<String, u32>,
    n: u32,
    max: u32,
) -> Result<PackedCrew, String> {
    if n == 0 {
        return Err("crew budget must be >= 1".into());
    }
    if n > max {
        return Err(format!(
            "crew budget {n} exceeds crew.max={max} (set FLYWHEEL_CREW_MAX or ~/.config/flywheel [crew].max)"
        ));
    }
    let mut roles: Vec<String> = counts.keys().cloned().collect();
    roles.sort();
    roles.dedup();
    if roles.is_empty() {
        return Ok(PackedCrew {
            members: BTreeMap::new(),
            aliases: BTreeMap::new(),
            budget: n,
            unique: 0,
            cap: max,
        });
    }
    sort_roles(&mut roles);
    let unique_n = roles.len() as u32;
    let host = roles
        .first()
        .cloned()
        .unwrap_or_else(|| "coder".to_string());

    let mut members: BTreeMap<String, u32> = counts.clone();
    let mut aliases = BTreeMap::new();

    // Fold single-copy roles, last first, until the crew fits the budget.
    // A role that needs more than one pane is never a fold candidate.
    let mut total: u32 = members.values().sum();
    while total > n {
        let mut ordered = roles.clone();
        sort_roles(&mut ordered);
        let victim = ordered
            .into_iter()
            .rev()
            .find(|r| members.get(r).copied().unwrap_or(0) == 1);
        let Some(victim) = victim else {
            return Err(format!(
                "crew needs {total} panes but the budget is {n} and every remaining role is \
                 needed concurrently (a role dispatched N times at once needs N panes, and \
                 folding it onto another agent is refused by the busy-owner fence). Raise the \
                 crew budget, lower the fan-out, or split the workflow."
            ));
        };
        roles.retain(|r| r != &victim);
        members.remove(&victim);
        total -= 1;
        aliases.insert(victim, host.clone());
    }
    if total > max {
        return Err(format!(
            "crew needs {total} panes, over crew.max={max} (a role dispatched concurrently needs \
             one pane per copy). Lower the fan-out or raise FLYWHEEL_CREW_MAX."
        ));
    }
    // Spare capacity goes to the worker, as before.
    if n > total {
        let extra = n - total;
        let worker = if members.contains_key("coder") {
            "coder"
        } else {
            host.as_str()
        };
        *members.entry(worker.to_string()).or_insert(1) += extra;
    }
    Ok(PackedCrew {
        members,
        aliases,
        budget: n,
        unique: unique_n,
        cap: max,
    })
}

/// Pack an inferred unique-role plan with a request.
pub fn pack_inferred(inferred: &CrewPlan, req: &CrewFlag, max: u32) -> Result<PackedCrew, String> {
    // bd-0s0y: the inferred plan already carries per-role COPY counts
    // (crew_demand raises them), so pack the counts. Passing only the keys —
    // the old behavior — silently reset every role to one pane, which is how a
    // `parallel over: 3` role ended up sharing one agent with its own copies.
    let unique: Vec<String> = inferred.members.keys().cloned().collect();
    // bd-0s0y: the budget a default crew gets is what the workflow NEEDS —
    // the sum of its per-role copy counts — not the number of distinct roles.
    let needed: u32 = inferred.members.values().sum();
    match req {
        CrewFlag::Default | CrewFlag::Named(_) => {
            if unique.is_empty() {
                return pack_crew_shape(&inferred.members, 1, max);
            }
            pack_crew_shape(&inferred.members, needed.max(1).min(max), max)
        }
        CrewFlag::Total(n) => pack_crew_shape(&inferred.members, *n, max),
        CrewFlag::Map(map) => {
            let n: u32 = map.values().copied().sum();
            if n == 0 {
                return Err("crew map must spawn at least 1 pane".into());
            }
            if n > max {
                return Err(format!(
                    "crew budget {n} exceeds crew.max={max} (set FLYWHEEL_CREW_MAX or ~/.config/flywheel [crew].max)"
                ));
            }
            let host = map
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "coder".to_string());
            let mut aliases = BTreeMap::new();
            for role in &unique {
                if !map.contains_key(role) {
                    aliases.insert(role.clone(), host.clone());
                }
            }
            Ok(PackedCrew {
                members: map.clone(),
                aliases,
                budget: n,
                unique: unique.len() as u32,
                cap: max,
            })
        }
    }
}

fn rewrite_profile(profile: &Option<String>, aliases: &BTreeMap<String, String>) -> Option<String> {
    let raw = profile.as_ref()?;
    let name = raw.strip_prefix('@').unwrap_or(raw.as_str());
    aliases
        .get(name)
        .map(|host| format!("@{host}"))
        .or_else(|| Some(raw.clone()))
}

fn rewrite_action(action: &Action, aliases: &BTreeMap<String, String>) -> Action {
    match action {
        Action::DispatchJob {
            spec,
            assignee,
            channel,
        } => {
            let mut spec = spec.clone();
            spec.profile = rewrite_profile(&spec.profile, aliases);
            Action::DispatchJob {
                spec,
                assignee: assignee.clone(),
                channel: channel.clone(),
            }
        }
        Action::Parallel { over, each } => Action::Parallel {
            over: over.clone(),
            each: Box::new(rewrite_step(each, aliases)),
        },
        Action::Retry {
            max_attempts,
            backoff_secs,
            step,
        } => Action::Retry {
            max_attempts: *max_attempts,
            backoff_secs: *backoff_secs,
            step: Box::new(rewrite_step(step, aliases)),
        },
        Action::Branch {
            if_guard,
            then,
            else_steps,
        } => Action::Branch {
            if_guard: if_guard.clone(),
            then: rewrite_steps(then, aliases),
            else_steps: rewrite_steps(else_steps, aliases),
        },
        Action::Foreach { over, each } => Action::Foreach {
            over: over.clone(),
            each: Box::new(rewrite_step(each, aliases)),
        },
        Action::Loop { max, until, body } => Action::Loop {
            max: max.clone(),
            until: until.clone(),
            body: rewrite_steps(body, aliases),
        },
        other => other.clone(),
    }
}

fn rewrite_step(step: &Step, aliases: &BTreeMap<String, String>) -> Step {
    let mut step = step.clone();
    step.action = rewrite_action(&step.action, aliases);
    step
}

fn rewrite_steps(steps: &[Step], aliases: &BTreeMap<String, String>) -> Vec<Step> {
    steps.iter().map(|s| rewrite_step(s, aliases)).collect()
}

/// Rewrite overflow `@role` dispatch profiles onto their host (ADR-0011 D2).
pub fn apply_role_aliases(workflow: &Workflow, aliases: &BTreeMap<String, String>) -> Workflow {
    if aliases.is_empty() {
        return workflow.clone();
    }
    let mut wf = workflow.clone();
    wf.steps = rewrite_steps(&wf.steps, aliases);
    if let Some(graph) = wf.graph.as_mut() {
        for node in graph.nodes.values_mut() {
            node.action = rewrite_action(&node.action, aliases);
        }
    }
    wf
}

/// bd-8c5h: carry a decided fold into every CHILD the engine resolves.
///
/// [`apply_role_aliases`] can only rewrite the document it is handed, so it
/// reaches a folded role only when the parent dispatches that role itself.
/// When the parent delegates (`call_workflow`), the dispatch lives in the
/// child, the rewrite lands in a document that dispatches nothing, and the
/// fold the packer announced in its pack line never happens — the original
/// bd-8c5h bail, one indirection deeper than the fix that already existed.
///
/// The resolver is the only thing that ever sees a child, so the fold rides
/// it. Composition, not duplication: the rewrite logic is still
/// [`apply_role_aliases`], applied one document deeper.
///
/// A child the base cannot resolve stays unresolvable — inventing an empty
/// document would let a missing sub-workflow run and report success.
pub fn aliasing_resolver(
    base: toron_workflow::WorkflowResolver,
    aliases: BTreeMap<String, String>,
) -> toron_workflow::WorkflowResolver {
    if aliases.is_empty() {
        return base;
    }
    std::sync::Arc::new(move |name: &str| {
        base(name).map(|child| apply_role_aliases(&child, &aliases))
    })
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
            Action::Loop { max, until, body } => Action::Loop {
                max: max.clone(),
                until: until.clone(),
                body: resolve_steps(body, plan),
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
    plan_crew_with(workflow, &|_| None)
}

/// One compose part: a dispatch part (`role:stage` — a `@role` agent runs the
/// named stage), a stage-first part (a bare stage name — dispatched to the
/// stage's declared role affinity, §3.8), a phase-tagged part (`phase:stage`
/// — affinity-resolved + the §4 lifecycle phase, W3), or a workflow part
/// (`workflow:name` — a `call_workflow` sub-run). Parsed from
/// `flywheel workflow compose --parts`.
pub const COMPOSE_PART_FORMS: &str = "bare <stage>, stage:<stage>, role:<role>:<stage>, <role>:<stage>, phase:<phase>:<stage>, <phase>:<stage>, workflow:<name>, kind:<harness>:<stage>, kind:<harness>:<role>:<stage>, each:<param>:<part>, parallel:<param>:<part>, when:<guard>:<part>, retry(n):<part>";

fn compose_part_error(token: &str, detail: impl std::fmt::Display) -> String {
    format!("compose part {token:?}: {detail}; supported forms: {COMPOSE_PART_FORMS}")
}

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
    Each { over: String, inner: Box<Self> },
    /// `when:<guard>:<part>` → a branch running the part as the single
    /// `then` step when the guard holds. ADR-0022 §3.6 T1.
    When { guard: String, inner: Box<Self> },
    /// `retry(n):<part>` → bounded retries (n ≥ 1) around the part.
    /// ADR-0022 §3.6 T1.
    Retry { max_attempts: u32, inner: Box<Self> },
    /// `parallel:<over>:<part>` → elastic fan-out across N copies or a
    /// crew role (`@crew.<role>`). ADR-0022 §3.6 T1.
    Parallel { over: String, inner: Box<Self> },
    /// `kind:<harness>:<stage>` uses the stage's role and pins the harness.
    /// `kind:<harness>:<role>:<stage>` names the role. Headless honors the pin.
    KindStage {
        harness: String,
        role: Option<String>,
        stage: String,
    },
    /// ADR-0006 D3 + ADR-0008 §6.7: a compose token (`wave-swarm`,
    /// `repair-until-verified`, `beads-lifecycle`) → its compiled-in
    /// multi-phase steps, expanded where the token sits.
    Recipe { name: String },
}

/// Parse one compose part token (`role:stage` | `phase:stage` | `workflow:name`
/// | bare `stage`). Pure — the unit surface for the compose CLI's
/// deterministic arm (bd-8xca, §3.8 stage-first).
pub fn parse_compose_part(token: &str) -> Result<ComposePart, String> {
    let t = token.trim();
    let Some((kind, value)) = t.split_once(':') else {
        // ADR-0006 D3 + ADR-0008 §6.7: a compose token (`wave-swarm`,
        // `repair-until-verified`, `beads-lifecycle`) is checked BEFORE the — a compose-token name
        // shadows any stage literally named the same (tokens are crate-reserved).
        if t.split(':').count() == 1 && crate::builtin_workflows::is_compose_token(t) {
            return Ok(ComposePart::Recipe {
                name: t.to_string(),
            });
        }
        // §3.8 stage-first: a bare stage name is a valid part.
        let stage = t.trim();
        if stage.is_empty() {
            return Err(compose_part_error(token, "empty stage name"));
        }
        // Fail closed (§3.6 envelope #1): a colon-less token SHAPED like a
        // T1 control form — a lone keyword or `retry(n)` with no inner part
        // — is a malformed structured form, never a stage literally named
        // like one. Keywords shadow stage names the way phases shadow roles.
        if matches!(stage, "each" | "when" | "parallel" | "retry") || stage.starts_with("retry(") {
            return Err(compose_part_error(
                token,
                "incomplete compose form; control parts need an inner part",
            ));
        }
        return Ok(ComposePart::StageFirst {
            stage: stage.to_string(),
        });
    };
    match kind.trim() {
        "stage" => {
            let stage = value.trim();
            if stage.is_empty() || stage.contains(':') {
                return Err(compose_part_error(
                    token,
                    "stage form needs one non-empty stage name",
                ));
            }
            Ok(ComposePart::StageFirst {
                stage: stage.to_string(),
            })
        }
        "role" => {
            let (role, stage) = value
                .split_once(':')
                .ok_or_else(|| compose_part_error(token, "role form needs `<role>:<stage>`"))?;
            let role = role.trim();
            let stage = stage.trim();
            if role.is_empty() || stage.is_empty() || stage.contains(':') {
                return Err(compose_part_error(
                    token,
                    "role form needs one non-empty role and one non-empty stage",
                ));
            }
            Ok(ComposePart::Dispatch {
                role: role.to_string(),
                stage: stage.to_string(),
            })
        }
        "workflow" => {
            let name = value.trim();
            if name.is_empty() {
                return Err(format!("compose part {token:?}: empty workflow name"));
            }
            Ok(ComposePart::CallWorkflow {
                name: name.to_string(),
            })
        }
        "kind" => {
            let parts: Vec<&str> = value
                .split(':')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            match parts.as_slice() {
                [harness, stage] => Ok(ComposePart::KindStage {
                    harness: (*harness).to_string(),
                    role: None,
                    stage: (*stage).to_string(),
                }),
                [harness, role, stage] => Ok(ComposePart::KindStage {
                    harness: (*harness).to_string(),
                    role: Some((*role).to_string()),
                    stage: (*stage).to_string(),
                }),
                _ => Err(compose_part_error(
                    token,
                    "kind form is `kind:<harness>:<stage>` or `kind:<harness>:<role>:<stage>`",
                )),
            }
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
        phase if crate::stages::PHASES.contains(&phase) && !value.contains(':') => {
            let stage = value.trim();
            if stage.is_empty() {
                return Err(compose_part_error(token, "empty stage name"));
            }
            Ok(ComposePart::PhaseStage {
                phase: phase.to_string(),
                stage: stage.to_string(),
            })
        }
        "phase" => {
            let (phase, stage) = match value.split_once(':') {
                Some((phase, stage)) => (phase.trim(), stage.trim()),
                None => (value.trim(), value.trim()),
            };
            if !crate::stages::PHASES.contains(&phase) || stage.is_empty() || stage.contains(':') {
                return Err(compose_part_error(
                    token,
                    format!(
                        "unknown phase {phase:?}; valid phases: {}",
                        crate::stages::PHASES.join(", ")
                    ),
                ));
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
                return Err(compose_part_error(
                    token,
                    "role and stage must be non-empty",
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

/// [`compose_workflow_with`] plus a compose-token resolver: the same
/// catalog-first rule `workflow run <name>` follows (bd-qd87). A user or
/// project override of a token (e.g. `wave-swarm`) now expands on compose
/// exactly as it resolves on run — two paths, one truth. Pure over the
/// injected resolver.
pub fn compose_workflow_with_resolvers(
    name: &str,
    objective: &str,
    base: Option<&Workflow>,
    parts: &[ComposePart],
    affinity: &dyn Fn(&str) -> Option<String>,
    recipe_resolver: &dyn Fn(&str) -> Option<Workflow>,
) -> Result<Workflow> {
    let base_inputs_snapshot: Vec<InputDecl> =
        base.map(|b| b.inputs.clone()).unwrap_or_else(|| {
            vec![InputDecl {
                name: "objective".to_string(),
                default: None,
            }]
        });
    let mut steps: Vec<Step> = base.map(|b| b.steps.clone()).unwrap_or_default();
    let mut graph_nodes: BTreeMap<String, toron_workflow::schema::GraphNode> = base
        .and_then(|b| b.graph.as_ref().map(|g| g.nodes.clone()))
        .unwrap_or_default();
    let mut extra_inputs: Vec<InputDecl> = Vec::new();
    let mut ordinal = 0u32;
    // GS-039 P0-3: preserve Explicit crew from compose-token recipes
    // (repair-until-verified declares coder:1+reviewer:1). Forcing Auto here
    // used to balloon dry-run/plan to coder:3+reviewer:3 from 3 repair + 3
    // evaluate graph nodes. Auto recipes / non-recipe parts still force Auto.
    let mut explicit_from_recipes: Option<BTreeMap<String, u32>> = None;
    let mut recipe_requested_auto = false;
    let mut non_recipe_parts = false;
    for part in parts {
        // ADR-0006 D3 + ADR-0008 §6.7: a compose token expands to its
        // compiled-in steps (or graph nodes) right where it sits; its declared inputs merge into the
        // composed workflow so the token's template refs lint clean.
        // The builtin's own on_error (e.g. bug-fix fail_run) is preserved
        // verbatim; bare non-token parts are FailRun via build_part_step (bd-gs005).
        if let ComposePart::Recipe { name } = part {
            let expanded = recipe_resolver(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "compose part {name:?}: unknown compose token (known: wave-swarm, repair-until-verified, beads-lifecycle)"
                )
            })?;
            match expanded.crew {
                Some(CrewSpec::Explicit(map)) => {
                    let dst = explicit_from_recipes.get_or_insert_with(BTreeMap::new);
                    for (role, count) in map {
                        let e = dst.entry(role).or_insert(0);
                        *e = (*e).max(count);
                    }
                }
                Some(CrewSpec::Auto) | None => {
                    recipe_requested_auto = true;
                }
            }
            for input in expanded.inputs {
                if !steps_has_input(&extra_inputs, &input.name)
                    && !steps_has_input(&base_inputs_snapshot, &input.name)
                {
                    extra_inputs.push(input);
                }
            }
            // Preserve the builtin's own step on_error verbatim.
            steps.extend(expanded.steps.clone());
            if let Some(g) = expanded.graph {
                for (k, v) in g.nodes {
                    let mut key = k.clone();
                    let mut ctr = 1u32;
                    while graph_nodes.contains_key(&key) {
                        key = format!("{k}-{ctr}");
                        ctr += 1;
                    }
                    graph_nodes.insert(key, v);
                }
            }
            ordinal += 1;
            continue;
        }
        non_recipe_parts = true;
        ordinal += 1;
        steps.push(build_part_step(part, ordinal, objective, affinity)?);
    }
    let graph = if graph_nodes.is_empty() {
        None
    } else {
        Some(toron_workflow::schema::Graph { nodes: graph_nodes })
    };
    let crew = if !non_recipe_parts && !recipe_requested_auto {
        if let Some(map) = explicit_from_recipes {
            Some(CrewSpec::Explicit(map))
        } else {
            Some(CrewSpec::Auto)
        }
    } else {
        Some(CrewSpec::Auto)
    };
    Ok(Workflow {
        name: name.to_string(),
        description: Some(format!(
            "Runtime-composed workflow for: {objective} (bd-8xca deterministic arm)"
        )),
        trigger: None,
        policy: None,
        guard: None,
        steps,
        graph,
        inputs: {
            let mut inputs = base_inputs_snapshot;
            for input in extra_inputs {
                inputs.push(input);
            }
            inputs
        },
        crew,
        principles: vec![],
    })
}

/// Does `inputs` already declare `name`? (compose-token input merge dedup)
fn steps_has_input(inputs: &[InputDecl], name: &str) -> bool {
    inputs.iter().any(|i| i.name == name)
}

/// bd-compose-plumbing-zdzi: the channel a composed run's stages post to —
/// the workflow's declared `steer_channel`, resolved the way the run will
/// resolve it (provided inputs win, the declared default is the fallback).
///
/// `Ok(None)` = this workflow declares no steer channel (nothing to ensure).
/// `Err` = it DOES declare one and this run cannot resolve a non-empty value:
/// compose refuses BEFORE the crew spawns, because join-before-post means
/// every spawned pin needs the room and the strict renderer would otherwise
/// fail mid-run with panes already up.
pub fn resolve_steer_channel(wf: &Workflow, inputs: &serde_json::Value) -> Result<Option<String>> {
    let Some(decl) = wf.inputs.iter().find(|i| i.name == "steer_channel") else {
        return Ok(None);
    };
    let provided = inputs
        .get("steer_channel")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(name) = provided {
        return Ok(Some(name.to_string()));
    }
    let default = decl
        .default
        .as_ref()
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(name) = default {
        return Ok(Some(name.to_string()));
    }
    // bail! formats its literal: the doubled braces below print as the JSON
    // object an operator can paste after --inputs.
    anyhow::bail!(
        "the workflow's steer_channel input cannot be resolved — declare a default or pass \
         --inputs '{{\"steer_channel\":\"<name>\"}}'; join-before-post: every spawned pin must \
         join the channel before any stage posts"
    )
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
            // bd-gs005: composed bare parts default fail_run — a red part
            // must fail the run visibly, not sail through as Continue
            // (first-prompt trust; builtins already carry their own).
            on_error: StepOnError::FailRun,
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
                    // `goal` is the compose objective. The engine also copies
                    // the parent run's inputs under this object, so a child
                    // that needs `verify` or `symptom` receives them.
                    inputs: serde_json::json!({ "goal": objective, "objective": objective }),
                },
            ),
            ComposePart::KindStage {
                harness,
                role,
                stage,
            } => {
                let role = match role {
                    Some(role) => role.clone(),
                    None => affinity(stage).ok_or_else(|| {
                        anyhow::anyhow!(
                            "stage {stage:?} declares no role affinity — use kind:{harness}:<role>:{stage}"
                        )
                    })?,
                };
                base(
                    format!("{}-{ordinal}", slugify(&format!("{role}-{stage}"))),
                    Action::DispatchJob {
                        assignee: "x".to_string(),
                        spec: toron_workflow::schema::DispatchSpec {
                            profile: Some(format!("@{role}")),
                            stage: Some(stage.clone()),
                            harness: Some(harness.clone()),
                            task: Some(objective.to_string()),
                            ..Default::default()
                        },
                        channel: None,
                    },
                )
            }
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
        // A failed when/each/retry/parallel is a failed run. Continue here
        // stored `{ok:false}` and the linear runner treated that as success.
        on_error: StepOnError::FailRun,
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
        // ADR-0006 D3: a compose token is multi-step — it never reaches the
        // one-step builder; compose_workflow_with expands it inline.
        ComposePart::Recipe { name } => {
            anyhow::bail!("compose part: compose token `{name}` must expand in the assemble loop")
        }
        leaf_part @ (ComposePart::Dispatch { .. }
        | ComposePart::StageFirst { .. }
        | ComposePart::PhaseStage { .. }
        | ComposePart::CallWorkflow { .. }
        | ComposePart::KindStage { .. }) => leaf(leaf_part, ordinal, objective, affinity),
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
                walk(&each.action, depth + 1, dispatches, max_depth);
            }
            Action::Retry { step, .. } => walk(&step.action, depth + 1, dispatches, max_depth),
            Action::Branch {
                then, else_steps, ..
            } => {
                for s in then.iter().chain(else_steps.iter()) {
                    walk(&s.action, depth + 1, dispatches, max_depth);
                }
            }
            Action::Loop { body, .. } => {
                for s in body {
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
                );
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
                );
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
            Action::Loop { body, .. } => {
                collect(
                    &format!("{path}/body"),
                    body,
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
    stages: &[&str],
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
    for name in stages {
        stage_rows.push_str(&format!("- {name}\n"));
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
            // ADR-0011 D2: YAML counts are a role inventory, not a spawn lock.
            members: map.keys().cloned().map(|k| (k, 1u32)).collect(),
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
        // bd-adv4: the profile-pick input template names the implement
        // profile decided at invoke time — it always seats a coder.
        if role.trim() == "{{inputs.coder_profile}}" {
            counts.entry("coder".to_string()).or_insert(1);
            return;
        }
        if let Some(role) = role.strip_prefix('@') {
            counts.entry(role.to_string()).or_insert(1);
        }
    }
    fn bump_fanout(over: &str, counts: &mut BTreeMap<String, u32>) {
        if let Some(role) = over.trim().strip_prefix("@crew.") {
            // A fan-out step needs at least one member per role; parallel
            // over the whole role means count >= 1 (the orchestrator may
            // raise it).
            counts.entry(role.to_string()).or_insert(1);
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
            Action::Loop { body, .. } => {
                for s in body {
                    scan_action(&s.action, counts, resolve, visited);
                }
            }
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

    // bd-0s0y: the scan above SEES roles and seats one pane each, which is
    // right for a sequential chain and wrong for a role dispatched
    // concurrently. crew_demand adds the per-role copy count for the busiest
    // wave; merged with max so a child's demand and this document's both hold.
    for (role, copies) in crew_demand(workflow).crew_shape {
        let slot = counts.entry(role).or_insert(1);
        *slot = (*slot).max(copies);
    }

    CrewPlan { members: counts }
}
/// bd-adr0008-guard-fix-16fl: bind declared input defaults onto the caller's
/// inputs before invoke. `InputDecl.default` documents "the engine binds a
/// provided value over this default at invoke time", but no engine path
/// applies it — so `{{inputs.x}}` templates and `trigger_x` guard variables
/// go unresolved (guards eval-error to skip) whenever the caller omits a
/// defaulted input. Fill ONLY missing keys; an explicit value always wins;
/// a non-object inputs value passes through untouched. Pure.
pub fn apply_input_defaults(workflow: &Workflow, inputs: &mut serde_json::Value) {
    let Some(map) = inputs.as_object_mut() else {
        return;
    };
    for decl in &workflow.inputs {
        if let Some(default) = &decl.default
            && !map.contains_key(&decl.name)
        {
            map.insert(decl.name.clone(), default.clone());
        }
    }
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
    mut inputs: serde_json::Value,
    max_seconds: u64,
) -> Result<serde_json::Value> {
    apply_input_defaults(&wf, &mut inputs);
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

/// Assemble with a stage-affinity resolver (§3.8 stage-first).
#[cfg(test)]
pub fn compose_workflow_with(
    name: &str,
    objective: &str,
    base: Option<&Workflow>,
    parts: &[ComposePart],
    affinity: &dyn Fn(&str) -> Option<String>,
) -> Result<Workflow> {
    compose_workflow_with_resolvers(
        name,
        objective,
        base,
        parts,
        affinity,
        // Test/default arm: compiled-in tokens only (bd-qd87 keeps the
        // production surface catalog-first).
        &|n| crate::builtin_workflows::builtin_recipe(n),
    )
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
        let runner: toron_workflow::engine::EffectRunner =
            std::sync::Arc::new(|_inv: &toron_workflow::engine::ActionInvocation| {
                std::thread::sleep(std::time::Duration::from_millis(700));
                Ok(serde_json::json!({ "ok": true }))
            });
        let engine = toron_workflow::WorkflowEngine::local_with_effect_runner(Some(runner))
            .expect("local engine");
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
        let fast: toron_workflow::engine::EffectRunner =
            std::sync::Arc::new(|_inv: &toron_workflow::engine::ActionInvocation| {
                Ok(serde_json::json!({ "ok": true }))
            });
        let engine2 = toron_workflow::WorkflowEngine::local_with_effect_runner(Some(fast))
            .expect("local engine 2");
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
            r#"
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
"#,
        )
        .expect("parse");
        let plan = plan_crew(&wf);
        assert_eq!(plan.members.get("reviewer"), Some(&1), "{:?}", plan.members);
        assert_eq!(plan.members.get("oracle"), Some(&1), "{:?}", plan.members);
        assert_eq!(plan.total(), 2, "unique roles, not occurrence count");
    }

    /// bd-adv4: a `crew: auto` step riding the profile-pick input template
    /// still seats a coder — the implement profile is decided at invoke
    /// time, the seat is known at plan time.
    #[test]
    fn profile_pick_template_ref_seats_coder() {
        let wf = parse_workflow(
            r#"
name: impl-token
trigger:
  type: webhook
crew: auto
steps:
  - name: execute
    action: dispatch_job
    assignee: x
    spec: { profile: "{{inputs.coder_profile}}" }
"#,
        )
        .expect("parse");
        let plan = plan_crew(&wf);
        assert_eq!(plan.members.get("coder"), Some(&1), "{:?}", plan.members);
    }

    /// bd-t86p: crew:auto on a COMPOSED workflow (call_workflow steps) must
    /// include the sub-workflows' roles — goal-direct composes brainstorm
    /// (oracle+planner) and plan-direct (planner) on top of its own
    /// coder/reviewer/housekeeper; without recursion the first @oracle/@planner
    /// dispatch fails "no crew pane".
    #[test]
    fn crew_auto_recurses_into_call_workflow_targets() {
        let wf = parse_workflow(
            r#"
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
"#,
        )
        .expect("parse");
        let resolve = |sub: &str| match sub {
            "brainstorm" => Some(
                parse_workflow(
                    r#"
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
"#,
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
            r"
name: a
trigger: { type: webhook }
crew: auto
steps:
  - name: self
    action: call_workflow
    workflow: b
",
        )
        .expect("parse");
        let b = parse_workflow(
            r#"
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
"#,
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
        // Explicit tags remove the role/phase vocabulary collision without
        // removing the compact legacy forms.
        assert_eq!(
            parse_compose_part("stage:ce-execute").unwrap(),
            ComposePart::StageFirst {
                stage: "ce-execute".into(),
            }
        );
        assert_eq!(
            parse_compose_part("role:reviewer:ce-review").unwrap(),
            ComposePart::Dispatch {
                role: "reviewer".into(),
                stage: "ce-review".into(),
            }
        );
        assert_eq!(
            parse_compose_part("phase:implement:ce-execute").unwrap(),
            ComposePart::PhaseStage {
                phase: "implement".into(),
                stage: "ce-execute".into(),
            }
        );
        let err = parse_compose_part("phase:mars:ce-execute").unwrap_err();
        assert!(err.contains("valid phases: resolve, plan, implement, review, verify, compound"));
        assert!(err.contains("supported forms:"));
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
                stage: "ce-execute".into(),
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
    /// Bare parts default FailRun (bd-gs005 — red fails the run, not silent green).
    #[test]
    fn compose_workflow_assembles_parts_and_lints_clean() {
        use toron_workflow::schema::StepOnError;
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
        // bd-gs005: bare parts default FailRun — a red part must fail the run.
        assert_eq!(
            wf.steps[0].on_error,
            StepOnError::FailRun,
            "bare dispatch is FailRun"
        );
        assert_eq!(
            wf.steps[1].on_error,
            StepOnError::FailRun,
            "bare call_workflow is FailRun"
        );
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
            r#"
name: triage
trigger: { type: webhook }
crew: auto
steps:
  - name: gate
    action: send_dm
    to: operator
    body: "start"
"#,
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
    /// Leaf on_error is FailRun (bd-gs005); outer control wrapper stays
    /// Continue — the R-E1/R-E3 downgrade handles retry-on-blocked at lint time,
    /// not by silently downgrading FailRun at compose.
    #[test]
    fn t1_compose_grammar_parts_assemble_actions() {
        use toron_workflow::schema::StepOnError;
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
        let wf = compose_workflow_with("composed-t1", "obj", None, &parts, &affinity).unwrap();
        assert_eq!(wf.steps.len(), 4);
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
        // Outer wrappers are FailRun; inner leaves are FailRun (bd-gs005).
        //
        // ADAPTED to toron-workflow 17b2eb4 ("make loop, failure, and guards
        // composable"). That commit made a step fail on `ok:false` OR
        // `status:failed`, so a wrapper left at `Continue` no longer stored a
        // red part as a harmless `{ok:false}` — the run failed anyway, just
        // without the deterministic `failed_step`. The compose builder's own
        // comment now says exactly that ("Continue here stored {ok:false} and
        // the linear runner treated that as success"), so FailRun on the
        // wrapper is the correct spelling and this assertion was the stale
        // half of that change.
        for step in &wf.steps {
            assert_eq!(
                step.on_error,
                StepOnError::FailRun,
                "outer T1 wrapper is FailRun since the engine fails a step on ok:false"
            );
            match &step.action {
                Action::Foreach { each, .. } | Action::Parallel { each, .. } => {
                    assert_eq!(each.on_error, StepOnError::FailRun);
                }
                Action::Branch { then, .. } => assert_eq!(then[0].on_error, StepOnError::FailRun),
                Action::Retry { step, .. } => assert_eq!(step.on_error, StepOnError::FailRun),
                _ => panic!("unexpected T1 outer"),
            }
        }
        let report = toron_workflow::lint_workflow(&wf);
        assert!(
            report.is_clean(),
            "T1-composed workflow must lint clean: {:?}",
            report.issues
        );
        enforce_budget_caps(&wf).expect("within caps");
    }

    /// bd-qd87: a catalog override of a compose token wins at expansion
    /// (catalog-first, matching `workflow run`), and the compiled-in token
    /// still expands when the catalog has no override.
    #[test]
    fn qd87_compose_token_catalog_first_resolution() {
        use toron_workflow::schema::Action;
        let lib = crate::stages::StageLibrary::compiled_in();
        let affinity = |s: &str| lib.get_stage(s).and_then(|st| st.role.clone());
        // A user-tier override of `wave-swarm`: one dispatch_job step
        // (same step shape as the builtin's YAML dialect).
        let override_wf = crate::crew::workflow_from_yaml(
            r#"
name: wave-swarm
description: user override
steps:
  - name: custom-gate
    action: dispatch_job
    assignee: x
    spec:
      profile: "@planner"
      stage: fresh-eyes-explore
      task: "custom"
"#,
        )
        .expect("override yaml parses");
        let resolver = |n: &str| {
            if n == "wave-swarm" {
                Some(override_wf.clone())
            } else {
                crate::builtin_workflows::builtin_recipe(n)
            }
        };
        let part = parse_compose_part("wave-swarm").expect("compose token parses");
        let wf = compose_workflow_with_resolvers(
            "composed-override",
            "obj",
            None,
            std::slice::from_ref(&part),
            &affinity,
            &resolver,
        )
        .unwrap();
        assert_eq!(
            wf.steps.len(),
            1,
            "override expands, not the 8-step builtin"
        );
        assert!(
            matches!(&wf.steps[0].action, Action::DispatchJob { spec, .. }
                if spec.stage.as_deref() == Some("fresh-eyes-explore")),
            "the override's own step ran: {:?}",
            wf.steps[0].action.type_name()
        );
        // No override in the resolver: builtin expands as before.
        let wf_builtin = compose_workflow_with_resolvers(
            "composed-builtin",
            "obj",
            None,
            &[part],
            &affinity,
            &|n| crate::builtin_workflows::builtin_recipe(n),
        )
        .unwrap();
        assert_eq!(
            wf_builtin.steps.len(),
            13,
            "builtin token unchanged (8 stages + 5 sequencing awaits, bd-compose-stage-sequencing-zgi9)"
        );
    }

    /// bd-compose-stage-sequencing-zgi9: the name is historical (ADR-0009
    /// witness script filters it); the expansion is the 8 stages PLUS one
    /// artifact-await step between each gated pair. The awaits are what make
    /// stage ORDER real: a dispatch can no longer walk past a predecessor
    /// whose completion marker is missing. See
    /// `compose_stage_awaits_completion` for the Acceptance proof.
    #[test]
    fn wave_swarm_compose_token_expands_eight_steps_and_lints_clean() {
        use toron_workflow::schema::Action;
        let lib = crate::stages::StageLibrary::compiled_in();
        let affinity = |s: &str| lib.get_stage(s).and_then(|st| st.role.clone());
        let part = parse_compose_part("wave-swarm").expect("compose token parses");
        let wf = compose_workflow_with("composed-wave", "obj", None, &[part], &affinity).unwrap();
        assert_eq!(wf.steps.len(), 13, "{:?}", wf.steps.iter().map(|s| &s.name));
        assert!(
            matches!(&wf.steps[0].action, Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@planner")
                    && spec.stage.as_deref() == Some("wave-gate")),
            "GATE first: {:?}",
            wf.steps[0].action.type_name()
        );
        assert!(
            matches!(&wf.steps[2].action, Action::DispatchJob { spec, .. }
                if spec.stage.as_deref() == Some("wave-dispatch")),
            "DISPATCH after await_gate"
        );
        assert!(
            matches!(&wf.steps[4].action, Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("{{inputs.coder_profile}}")
                    && spec.stage.as_deref() == Some("ce-execute")),
            "EXECUTE after await_dispatch (bd-wave-swarm-graduation-fix-98io: the crew needs an executor; bd-adv4: the executor profile rides the coder_profile input, default @coder)"
        );
        assert!(
            matches!(&wf.steps[6].action, Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@reviewer")
                    && spec.stage.as_deref() == Some("wave-observe")),
            "OBSERVE after the wave closed"
        );
        assert!(
            matches!(&wf.steps[7].action, Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@reviewer")
                    && spec.stage.as_deref() == Some("ce-review")),
            "REVIEW after await_execute (ADR-0009: audit the diff before VERIFY audits the receipts)"
        );
        assert!(
            matches!(&wf.steps[9].action, Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@reviewer")
                    && spec.stage.as_deref() == Some("wave-verify")),
            "VERIFY after await_review"
        );
        assert!(
            matches!(&wf.steps[11].action, Action::DispatchJob { spec, .. }
                if spec.profile.as_deref() == Some("@planner")
                    && spec.stage.as_deref() == Some("ce-compound")
                    && spec.phase.as_deref() == Some("compound")),
            "COMPOUND after await_verify (ADR-0009: phase compound, planner)"
        );
        assert!(
            matches!(&wf.steps[12].action, Action::RequestApproval { message }
                if message.contains("captain")),
            "SIGNOFF last, captain-only: {:?}",
            wf.steps[12].action.type_name()
        );
        // bd-compose-stage-sequencing-zgi9: every gated stage pair has an
        // artifact-await between them — Retry(Verify test -f <marker>) with
        // a per-step budget ≥ 1h and a loud in-flight exhaustion message.
        for (idx, marker) in [
            (1, "gate"),
            (3, "dispatch"),
            (5, "execute"),
            (8, "review"),
            (10, "verify"),
        ] {
            let Action::Retry {
                max_attempts,
                backoff_secs,
                step,
            } = &wf.steps[idx].action
            else {
                panic!(
                    "step {idx} ({:?}) must be the await for {marker}: {:?}",
                    wf.steps[idx].name,
                    wf.steps[idx].action.type_name()
                );
            };
            let Action::Verify { command } = &step.action else {
                panic!(
                    "await {marker} must wrap a verify: {}",
                    step.action.type_name()
                );
            };
            assert!(
                command.contains(&format!("{{{{inputs.wave_label}}}}-{marker}.done")),
                "await {marker} checks its completion marker: {command}"
            );
            assert!(
                command.contains("wave in flight"),
                "await {marker} fails loud with an in-flight message: {command}"
            );
            assert!(
                u64::from(*max_attempts - 1) * *backoff_secs >= 3600,
                "await {marker} carries a per-step budget of at least 1h: {max_attempts} x {backoff_secs}s"
            );
        }
        let report = toron_workflow::lint_workflow(&wf);
        assert!(
            report.is_clean(),
            "wave-swarm expansion must lint clean: {:?}",
            report.issues
        );
    }

    /// bd-compose-stage-sequencing-zgi9 Acceptance: sequence-verifiable-units
    /// — a stage fires only after the previous stage's artifact exists. Each
    /// gated pair is (stage, await), and WRITER/WAITER AGREEMENT is the
    /// property: the stage's task must instruct writing the exact marker the
    /// following await verifies. If the two ever name different paths, a green
    /// await proves nothing — which is why this is asserted (and mutation-
    /// proven red) rather than assumed. The waits carry their own budget and
    /// fail loud with a "wave in flight ... extend" message so a real wave is
    /// never silently dispatched over (boundary-discipline: per-step budgets,
    /// not one total): `--timeout` stays the run's outer bound.
    #[test]
    fn compose_stage_awaits_completion() {
        use toron_workflow::schema::Action;
        let lib = crate::stages::StageLibrary::compiled_in();
        let affinity = |s: &str| lib.get_stage(s).and_then(|st| st.role.clone());
        let part = parse_compose_part("wave-swarm").expect("compose token parses");
        let wf = compose_workflow_with("seq", "obj", None, &[part], &affinity).unwrap();

        // (stage step, await step, marker stem). Indices are composed
        // positions; every stage below appears in the expansion test too.
        let pairs: [(&str, usize, usize, &str); 5] = [
            ("gate", 0, 1, "gate"),
            ("dispatch", 2, 3, "dispatch"),
            ("execute", 4, 5, "execute"),
            ("review", 7, 8, "review"),
            ("verify", 9, 10, "verify"),
        ];
        for (stage_name, si, ai, marker) in pairs {
            assert_eq!(
                wf.steps[si].name.as_deref(),
                Some(stage_name),
                "step {si} is the {stage_name} stage: {:?}",
                wf.steps[si].name
            );
            let expected_await = format!("await_{stage_name}");
            assert_eq!(
                wf.steps[ai].name.as_deref(),
                Some(expected_await.as_str()),
                "step {ai} awaits {stage_name}"
            );
            // The stage's task tells its agent to write the completion marker.
            let Action::DispatchJob { spec, .. } = &wf.steps[si].action else {
                panic!(
                    "{stage_name} must be a dispatch_job: {:?}",
                    wf.steps[si].action.type_name()
                );
            };
            let task = spec
                .task
                .as_deref()
                .unwrap_or_else(|| panic!("{stage_name} carries no inline task"));
            let marker_path = format!(".flywheel/wave/{{{{inputs.wave_label}}}}-{marker}.done");
            assert!(
                task.contains(&marker_path),
                "{stage_name}'s task must instruct writing {marker_path}: {task}"
            );
            // The await waits on that same marker (writer/waiter agreement).
            let Action::Retry {
                max_attempts,
                backoff_secs,
                step,
            } = &wf.steps[ai].action
            else {
                panic!(
                    "await_{stage_name} must be a retry: {:?}",
                    wf.steps[ai].action.type_name()
                );
            };
            let Action::Verify { command } = &step.action else {
                panic!("await_{stage_name} must wrap a verify");
            };
            // The PROBE must be the marker path, not merely a mention of it:
            // the same command also carries a failure message naming the
            // marker, so `contains` would pass even if the probe checked a
            // different file (mutation-proven). Pin the `test -f` argument.
            assert!(
                command.starts_with(&format!("test -f {marker_path} ||")),
                "await_{stage_name} must probe {marker_path}: {command}"
            );
            // A wave still in flight must surface: the exhaustion message
            // names the wait and asks the operator to extend.
            assert!(
                command.contains("wave in flight"),
                "await_{stage_name} fails loud when the wave is in flight: {command}"
            );
            assert!(
                command.contains("--timeout"),
                "await_{stage_name} names the extension point (--timeout): {command}"
            );
            // Per-step, not one total: the retry budget is this step's own bound.
            assert!(
                u64::from(*max_attempts) * *backoff_secs >= 3600,
                "await_{stage_name} waits at least an hour before giving up: {max_attempts} x {backoff_secs}s"
            );
        }
    }

    /// bd-compose-plumbing-zdzi Acceptance: a composed run ensures its declared
    /// steer channel BEFORE anyone posts to it. Resolution is the run's own
    /// (provided inputs win, the declared default is the fallback), a workflow
    /// with no steer channel has nothing to ensure, and a declared-but-
    /// unresolvable channel refuses up front instead of failing mid-run with
    /// the crew already spawned.
    #[test]
    fn compose_ensures_steer_channel() {
        let lib = crate::stages::StageLibrary::compiled_in();
        let affinity = |s: &str| lib.get_stage(s).and_then(|st| st.role.clone());
        let part = parse_compose_part("wave-swarm").expect("compose token parses");
        let wf = compose_workflow_with("chan", "obj", None, &[part], &affinity).unwrap();

        // The declared default (`adr0006-steer`) is what the run renders, so
        // it is what compose must ensure and join every spawned pin into.
        assert_eq!(
            resolve_steer_channel(&wf, &serde_json::json!({}))
                .unwrap()
                .as_deref(),
            Some("adr0006-steer"),
            "the declared default is the channel the run renders"
        );
        // A provided --inputs value wins, exactly as the strict renderer uses it.
        assert_eq!(
            resolve_steer_channel(
                &wf,
                &serde_json::json!({"steer_channel": "elm-effect-steer"})
            )
            .unwrap()
            .as_deref(),
            Some("elm-effect-steer")
        );
        // A blank provided value counts as unset — never a nameless room.
        assert_eq!(
            resolve_steer_channel(&wf, &serde_json::json!({"steer_channel": "  "}))
                .unwrap()
                .as_deref(),
            Some("adr0006-steer")
        );
        // No declared channel: nothing to ensure (compose invents no room).
        let bare_part = parse_compose_part("coder:ce-execute").unwrap();
        let bare = compose_workflow_with("bare", "obj", None, &[bare_part], &affinity).unwrap();
        assert_eq!(
            resolve_steer_channel(&bare, &serde_json::json!({})).unwrap(),
            None
        );
        // Declared but unresolvable: refuse NOW, not mid-run with panes up.
        let mut declared = wf.clone();
        for input in &mut declared.inputs {
            if input.name == "steer_channel" {
                input.default = Some(serde_json::json!(""));
            }
        }
        let err = resolve_steer_channel(&declared, &serde_json::json!({}))
            .expect_err("declared-but-unresolvable must refuse");
        assert!(
            err.to_string().contains("cannot be resolved"),
            "refusal names the unresolvable channel: {err}"
        );
    }

    /// ADR-0006 D3: a compose token mixes with ordinary parts (token steps
    /// land where the token sits); unknown token-looking names stay plain
    /// role parts (the role fallback is untouched).
    #[test]
    fn wave_swarm_compose_token_mixes_with_plain_parts() {
        let lib = crate::stages::StageLibrary::compiled_in();
        let affinity = |s: &str| lib.get_stage(s).and_then(|st| st.role.clone());
        let parts: Vec<ComposePart> = ["scout:fresh-eyes-explore", "wave-swarm"]
            .iter()
            .map(|t| parse_compose_part(t).unwrap())
            .collect();
        let wf = compose_workflow_with("mixed", "obj", None, &parts, &affinity).unwrap();
        assert_eq!(
            wf.steps.len(),
            14,
            "1 plain + 8 compose-token stages + 5 sequencing awaits"
        );
    }

    // ---- ADR-0008 §10.C (bd-adr0008-expand-tests-7ui6) ----
    // beads-lifecycle expand-path coverage: graph-form token parses,
    // expands to its 16-node graph lint-clean, mixes with plain parts,
    // stays within budget caps; thin `beads` stays ordinary.
    #[test]
    fn beads_lifecycle_compose_token_parses_and_expands_graph_lint_clean() {
        use toron_workflow::schema::Action;
        let part = parse_compose_part("beads-lifecycle").expect("token parses");
        assert_eq!(
            part,
            ComposePart::Recipe {
                name: "beads-lifecycle".to_string()
            },
            "bare token name routes before the stage-first fallback"
        );
        let lib = crate::stages::StageLibrary::compiled_in();
        let affinity = |s: &str| lib.get_stage(s).and_then(|st| st.role.clone());
        let wf =
            compose_workflow_with("composed-lifecycle", "obj", None, &[part], &affinity).unwrap();
        let graph = wf
            .graph
            .as_ref()
            .expect("graph-form token expands to graph");
        assert_eq!(
            graph.nodes.len(),
            10,
            "8 phases + outer implement gate + existing-bead implement (guards ride ON the nodes): {:?}",
            graph.nodes.keys().collect::<Vec<_>>()
        );
        assert!(
            graph.nodes["grill"].needs.is_empty(),
            "grill first (no needs)"
        );
        let approvals = graph
            .nodes
            .values()
            .filter(|n| matches!(&n.action, Action::RequestApproval { .. }))
            .count();
        assert_eq!(
            approvals, 2,
            "signoff + the gate_before_implement outer gate (the gate node's guard skips it unless enabled)"
        );
        assert!(
            matches!(
                &graph.nodes["signoff"].action,
                Action::RequestApproval { .. }
            ),
            "signoff last (terminal approval)"
        );
        assert!(
            !graph
                .nodes
                .values()
                .any(|n| n.needs.contains(&"signoff".to_string())),
            "nothing runs after signoff"
        );

        assert!(
            wf.inputs.iter().any(|i| i.name == "until"
                && i.default == Some(serde_json::Value::String("signoff".to_string()))),
            "token inputs merge (until defaults to signoff): {:?}",
            wf.inputs.iter().map(|i| &i.name).collect::<Vec<_>>()
        );
        let report = toron_workflow::lint_workflow(&wf);
        assert!(
            report.is_clean(),
            "beads-lifecycle expansion must lint clean: {:?}",
            report.issues
        );
        enforce_budget_caps(&wf).expect("7 dispatches, depth 1: within caps");
    }
    /// bd-adr0008-guard-fix-16fl: declared defaults bind onto missing keys
    /// at the invoke door; explicit values always win; non-object inputs
    /// pass through.
    #[test]
    fn apply_input_defaults_fills_missing_preserves_explicit() {
        use toron_workflow::schema::{InputDecl, Workflow};
        let wf = Workflow {
            name: "t".into(),
            description: None,
            trigger: None,
            policy: None,
            guard: None,
            steps: vec![],
            graph: None,
            inputs: vec![
                InputDecl {
                    name: "objective".into(),
                    default: None,
                },
                InputDecl {
                    name: "until".into(),
                    default: Some(serde_json::Value::String("signoff".into())),
                },
                InputDecl {
                    name: "gate_before_implement".into(),
                    default: Some(serde_json::Value::Bool(false)),
                },
            ],
            crew: None,
            principles: vec![],
        };
        let mut inputs = serde_json::json!({ "objective": "x", "until": "create" });
        apply_input_defaults(&wf, &mut inputs);
        assert_eq!(inputs["until"], "create", "explicit value wins");
        assert_eq!(
            inputs["gate_before_implement"], false,
            "missing default binds"
        );
        let mut null = serde_json::Value::Null;
        apply_input_defaults(&wf, &mut null);
        assert!(null.is_null(), "non-object inputs untouched");
    }

    /// GS-039 P0-3: `--parts repair-until-verified` must keep the recipe's
    /// explicit crew coder:1+reviewer:1 — Auto-infer over 3 repair + 3
    /// evaluate nodes must NOT balloon dry-run to 3+3.
    #[test]
    fn repair_compose_token_preserves_explicit_crew_1_plus_1() {
        let part = parse_compose_part("repair-until-verified").expect("token");
        let wf = compose_workflow("composed-repair", "solo dry-run", None, &[part])
            .expect("compose repair");
        assert_eq!(
            wf.crew,
            Some(CrewSpec::Explicit(BTreeMap::from([
                ("coder".to_string(), 1),
                ("reviewer".to_string(), 1),
            ]))),
            "repair compose must preserve explicit crew, got {:?}",
            wf.crew
        );
        let plan = plan_crew(&wf);
        assert_eq!(plan.members.get("coder").copied(), Some(1));
        assert_eq!(plan.members.get("reviewer").copied(), Some(1));
        let rendered = render_dry_run(&wf, &|_| None);
        assert_eq!(rendered["crew"]["coder"], 1);
        assert_eq!(rendered["crew"]["reviewer"], 1);
    }

    /// A graph-form token mixes with ordinary parts (token nodes land in
    /// the graph; the plain part lands in steps); the unknown-token error
    /// names all three tokens.
    #[test]
    fn beads_lifecycle_compose_token_mixes_with_plain_parts() {
        let lib = crate::stages::StageLibrary::compiled_in();
        let affinity = |s: &str| lib.get_stage(s).and_then(|st| st.role.clone());
        let parts: Vec<ComposePart> = ["scout:fresh-eyes-explore", "beads-lifecycle"]
            .iter()
            .map(|t| parse_compose_part(t).unwrap())
            .collect();
        let wf = compose_workflow_with("mixed", "obj", None, &parts, &affinity).unwrap();
        assert_eq!(wf.steps.len(), 1, "the plain part lands in steps");
        assert_eq!(
            wf.graph.as_ref().expect("graph").nodes.len(),
            10,
            "the token lands in the graph (authoring + existing-bead implement + outer gate)"
        );
        let err = compose_workflow_with(
            "bad",
            "obj",
            None,
            &[ComposePart::Recipe {
                name: "nope".to_string(),
            }],
            &affinity,
        )
        .expect_err("unknown token refuses");
        for token in ["wave-swarm", "repair-until-verified", "beads-lifecycle"] {
            assert!(
                err.to_string().contains(token),
                "error names {token}: {err}"
            );
        }
    }

    /// Thin `beads` is an ordinary builtin: not a compose token, parses as
    /// a stage-first part (never bare `--parts` expansion).
    #[test]
    fn thin_beads_is_not_a_compose_token() {
        assert!(
            !crate::builtin_workflows::is_compose_token("beads"),
            "thin beads stays ordinary"
        );
        assert_eq!(
            parse_compose_part("beads").unwrap(),
            ComposePart::StageFirst {
                stage: "beads".to_string()
            },
            "bare beads falls through to stage-first, not Recipe"
        );
    }

    /// §3.6 envelope #3: budget caps fail closed at the compose gate.
    #[test]
    fn budget_caps_fail_closed() {
        use toron_workflow::schema::{Action, Step};
        // Over-cap dispatch leaves refuse.
        let parts: Vec<ComposePart> = (0..=(MAX_COMPOSED_DISPATCHES as u32))
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
        let mut deep = leaf;
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
            r#"
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
"#,
        )
        .expect("parse");
        let packed = pack_inferred(&plan_crew(&wf), &CrewFlag::Total(3), 5).expect("pack");
        let resolved = resolve_crew_fanouts(&wf, &packed.as_plan());
        let rendered = render_dry_run(&resolved, &|_| None);
        let s = serde_json::to_string(&rendered).unwrap();
        assert!(s.contains("\"fanout\":3"), "fanout count missing: {s}");
        assert!(s.contains("implement"), "phase tag missing: {s}");
    }

    /// An UNRESOLVABLE call_workflow keeps the flat leaf (never a silent
    /// expansion of nothing).
    #[test]
    fn dry_run_keeps_flat_leaf_for_unresolvable_link() {
        let wf = parse_workflow(
            r"
name: chain
trigger: { type: webhook }
crew: auto
steps:
  - name: hop
    action: call_workflow
    workflow: not-anywhere
",
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
        let src = r#"
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
"#;
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
            r"
name: ship
trigger:
  type: webhook
crew: { planner: 1, coder: 2, reviewer: 1 }
steps:
  - name: a
    action: send_dm
    to: x
    body: y
",
        )
        .expect("parse");
        let plan = plan_crew(&wf);
        assert_eq!(plan.members.get("coder"), Some(&1));
        assert_eq!(plan.members.get("planner"), Some(&1));
        assert_eq!(plan.members.get("reviewer"), Some(&1));
    }

    /// `parallel over: "@crew.coder"` fans across the role's members (row
    /// `crew_explicit` binding); the count resolves from the plan.
    #[test]
    fn crew_fanout_count_resolves() {
        let wf = parse_workflow(
            r#"
name: ship
trigger:
  type: webhook
crew: { coder: 2 }
steps:
  - name: build
    action: parallel
    over: "@crew.coder"
    each: { action: send_dm, to: x, body: y }
"#,
        )
        .expect("parse");
        let plan = plan_crew(&wf);
        let packed = pack_inferred(&plan, &CrewFlag::Total(2), 5).expect("pack");
        assert_eq!(
            packed.as_plan().fanout_count("@crew.coder"),
            2,
            "@crew.coder -> extra budget lands on coder"
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
            r#"
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
"#,
        )
        .expect("parse");
        let plan = pack_inferred(&plan_crew(&wf), &CrewFlag::Total(3), 5)
            .expect("pack")
            .as_plan();
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
            r#"
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
"#,
        )
        .expect("parse");
        let plan = pack_inferred(&plan_crew(&wf), &CrewFlag::Total(3), 5)
            .expect("pack")
            .as_plan();
        let resolved = resolve_crew_fanouts(&wf, &plan);
        let graph = resolved.graph.as_ref().expect("graph");
        match &graph.nodes["build"].action {
            Action::Parallel { over, .. } => assert_eq!(over, "3"),
            other => panic!("build should stay parallel, got {other:?}"),
        }
        // A graph WITHOUT the fan-out ref resolves to the same shape.
        let bare = parse_workflow(
            r#"
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
"#,
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
            r"
name: plain
trigger:
  type: webhook
steps:
  - name: a
    action: send_dm
    to: x
    body: y
",
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
            r#"
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
"#,
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
            &["fresh-eyes-explore", "ce-execute"],
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

    /// bd-gs005: bare parts default FailRun — a red dispatch must fail the
    /// composed workflow, not sail through as Continue (first-prompt trust).
    /// Verifies the t1 bare expectation is FailRun and that a failing bare
    /// part fails the run (R-E1/R-E3 downgrade handles retry-on-blocked at lint).
    #[tokio::test]
    async fn bare_dispatch_red_fails_composed_workflow_not_silent_green() {
        use toron_workflow::schema::StepOnError;
        // Single bare dispatch part via compose.
        let part = parse_compose_part("coder:systematic-execute").unwrap();
        let wf = compose_workflow("composed-bare-red", "fix the bug", None, &[part]).unwrap();
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(
            wf.steps[0].on_error,
            StepOnError::FailRun,
            "bare dispatch is FailRun"
        );
        // Runner that fails the dispatch step (red).
        let runner: toron_workflow::engine::EffectRunner = std::sync::Arc::new(
            |inv: &toron_workflow::engine::ActionInvocation| {
                if matches!(
                    &inv.action,
                    toron_workflow::schema::Action::DispatchJob { .. }
                ) {
                    Ok(
                        serde_json::json!({ "ok": false, "error": "simulated red", "kind": "unknown" }),
                    )
                } else {
                    Ok(serde_json::json!({ "ok": true }))
                }
            },
        );
        let engine =
            toron_workflow::WorkflowEngine::local_with_effect_runner(Some(runner)).expect("engine");
        let out = engine
            .run(
                "bare-red-1",
                toron_workflow::RunInput {
                    run_id: None,
                    workflow: wf,
                    trigger: serde_json::json!({}),
                },
            )
            .await
            .expect("run");
        assert_eq!(
            out["status"], "failed",
            "red bare dispatch must fail the run: {out}"
        );
        assert!(
            out["failed_step"].as_str().unwrap().contains("coder"),
            "{out}"
        );
        // Green control: same workflow with ok runner succeeds.
        let ok_runner: toron_workflow::engine::EffectRunner =
            std::sync::Arc::new(|_inv: &toron_workflow::engine::ActionInvocation| {
                Ok(serde_json::json!({ "ok": true }))
            });
        let engine2 = toron_workflow::WorkflowEngine::local_with_effect_runner(Some(ok_runner))
            .expect("engine2");
        let part2 = parse_compose_part("coder:systematic-execute").unwrap();
        let wf2 = compose_workflow("composed-bare-green", "fix the bug", None, &[part2]).unwrap();
        let out2 = engine2
            .run(
                "bare-green-1",
                toron_workflow::RunInput {
                    run_id: None,
                    workflow: wf2,
                    trigger: serde_json::json!({}),
                },
            )
            .await
            .expect("run2");
        assert!(
            out2.get("status").is_none() || out2["status"] != "failed",
            "green must not fail: {out2}"
        );
    }

    /// Compose --run VerifySink: a bare dispatch followed by a verify node
    /// must execute the verify (not "verify sink missing") and red must fail
    /// the run. Pins the make_verify_sink attachment in the compose --run arm
    /// (flywheel/src/main.rs) — the engine never shells out; the sink executes.
    #[tokio::test]
    async fn compose_dispatch_plus_verify_red_fails_and_sink_not_missing() {
        use std::sync::{Arc, Mutex};
        use toron_workflow::schema::StepOnError;
        // Compose one bare dispatch, then append a verify step (the pattern
        // `compose --run` produces when a verify stage is included).
        let part = parse_compose_part("coder:systematic-execute").unwrap();
        let mut wf = compose_workflow("composed-verify", "harden the loop", None, &[part]).unwrap();
        assert_eq!(wf.steps[0].on_error, StepOnError::FailRun);
        wf.steps.push(toron_workflow::schema::Step {
            name: Some("verify-1".into()),
            guard: None,
            on_error: StepOnError::FailRun,
            build_slot: false,
            action: Action::Verify {
                command: "cargo test --lib".into(),
            },
        });
        // Runner succeeds on dispatch; verify outcome comes from the sink.
        let runner: toron_workflow::engine::EffectRunner =
            Arc::new(|inv: &toron_workflow::engine::ActionInvocation| {
                if matches!(inv.action, Action::Verify { .. }) {
                    Ok(inv
                        .run
                        .provider_result
                        .clone()
                        .unwrap_or_else(|| serde_json::json!({ "ok": false })))
                } else {
                    Ok(serde_json::json!({ "ok": true }))
                }
            });
        let engine =
            toron_workflow::WorkflowEngine::local_with_effect_runner(Some(runner)).expect("engine");
        // Attach a RED verify sink (simulates `cargo test` failing).
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        let sink = toron_workflow::engine::VerifySink::default();
        *sink.0.lock().unwrap() = Some(Arc::new(
            move |req: &toron_workflow::engine::VerifyRequest| {
                seen_c.lock().unwrap().push(req.command.clone());
                Ok(toron_workflow::engine::VerifyResult {
                    ok: false,
                    exit: 1,
                    stdout_path: None,
                    stderr_path: None,
                    error: Some("simulated test failure".into()),
                })
            },
        ));
        *engine.verify_sink().0.lock().unwrap() = sink.0.lock().unwrap().clone();
        let out = engine
            .invoke("compose-verify-red-1", wf.clone(), serde_json::json!({}))
            .await
            .expect("run red");
        assert_eq!(seen.lock().unwrap().len(), 1, "verify must have executed");
        assert_eq!(seen.lock().unwrap()[0], "cargo test --lib");
        assert_eq!(
            out["status"], "failed",
            "red verify must fail the run: {out}"
        );
        assert_eq!(out["failed_step"], "verify-1");
        assert!(
            !out.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .contains("verify sink missing"),
            "sink must be attached, not missing: {out}"
        );
        let runner2: toron_workflow::engine::EffectRunner =
            Arc::new(|_inv: &toron_workflow::engine::ActionInvocation| {
                Ok(serde_json::json!({ "ok": true }))
            });
        let engine2 = toron_workflow::WorkflowEngine::local_with_effect_runner(Some(runner2))
            .expect("engine2");
        let sink2 = toron_workflow::engine::VerifySink::default();
        *sink2.0.lock().unwrap() =
            Some(Arc::new(|_req: &toron_workflow::engine::VerifyRequest| {
                Ok(toron_workflow::engine::VerifyResult {
                    ok: true,
                    exit: 0,
                    stdout_path: Some("/tmp/out".into()),
                    stderr_path: None,
                    error: None,
                })
            }));
        *engine2.verify_sink().0.lock().unwrap() = sink2.0.lock().unwrap().clone();
        // Need fresh workflow (steps cloned) — verify sink green path.
        let mut wf_green = compose_workflow(
            "composed-verify-green",
            "harden the loop",
            None,
            &[parse_compose_part("coder:systematic-execute").unwrap()],
        )
        .unwrap();
        wf_green.steps.push(toron_workflow::schema::Step {
            name: Some("verify-1".into()),
            guard: None,
            on_error: StepOnError::FailRun,
            build_slot: false,
            action: Action::Verify {
                command: "cargo test --lib".into(),
            },
        });
        let out2 = engine2
            .invoke("compose-verify-green-1", wf_green, serde_json::json!({}))
            .await
            .expect("run green");
        assert!(
            out2.get("status").is_none() || out2["status"] != "failed",
            "green verify must not fail: {out2}"
        );
        assert_eq!(out2["verify-1"]["ok"], true, "green verify ok: {out2}");
    }

    // ---- ADR-0011 D2: unique-role pack into total N ----

    #[test]
    fn parse_crew_flag_integer_is_total() {
        assert_eq!(parse_crew_flag("4"), CrewFlag::Total(4));
        assert_eq!(parse_crew_flag("auto"), CrewFlag::Default);
        assert_eq!(
            parse_crew_flag("coder:2,reviewer:1"),
            CrewFlag::Map(BTreeMap::from([
                ("coder".into(), 2),
                ("reviewer".into(), 1)
            ]))
        );
        assert_eq!(parse_crew_flag("trio"), CrewFlag::Named("trio".into()));
    }

    #[test]
    fn pack_crew_n_gt_max_fails_closed() {
        let err = pack_crew(&["coder".into(), "reviewer".into()], 8, 5).unwrap_err();
        assert!(err.contains("crew.max=5"), "{err}");
        assert!(err.contains("FLYWHEEL_CREW_MAX"), "{err}");
    }

    #[test]
    fn pack_five_unique_default_is_five_not_seven() {
        let unique = vec![
            "planner".into(),
            "coder".into(),
            "scout".into(),
            "reviewer".into(),
            "housekeeper".into(),
        ];
        let packed = pack_crew(&unique, 5, 5).expect("pack");
        assert_eq!(packed.total(), 5, "{:?}", packed.members);
        assert!(packed.aliases.is_empty());
        assert_eq!(packed.members.get("planner"), Some(&1));
        assert_eq!(packed.members.get("reviewer"), Some(&1));
    }

    #[test]
    fn pack_n4_of_5_aliases_housekeeper_onto_coder() {
        let unique = vec![
            "planner".into(),
            "coder".into(),
            "scout".into(),
            "reviewer".into(),
            "housekeeper".into(),
        ];
        let packed = pack_crew(&unique, 4, 5).expect("pack");
        assert_eq!(packed.total(), 4, "{:?}", packed.members);
        assert_eq!(
            packed.aliases.get("housekeeper").map(String::as_str),
            Some("coder"),
            "{:?}",
            packed.aliases
        );
        assert!(!packed.members.contains_key("housekeeper"));
        let line = packed.announce("wave-swarm");
        assert!(line.contains("into 4 panes"), "{line}");
        assert!(line.contains("housekeeper → coder"), "{line}");
    }

    #[test]
    fn pack_n_gt_unique_extra_goes_to_coder() {
        let unique = vec!["coder".into(), "reviewer".into()];
        let packed = pack_crew(&unique, 4, 5).expect("pack");
        assert_eq!(packed.total(), 4);
        assert_eq!(packed.members.get("coder"), Some(&3));
        assert_eq!(packed.members.get("reviewer"), Some(&1));
    }

    #[test]
    fn apply_role_aliases_rewrites_overflow_profile() {
        let wf = parse_workflow(
            r#"
name: wave
trigger: { type: webhook }
crew: auto
steps:
  - name: execute
    action: dispatch_job
    assignee: x
    spec: { profile: "@coder" }
  - name: compound
    action: dispatch_job
    assignee: x
    spec: { profile: "@housekeeper" }
"#,
        )
        .expect("parse");
        let aliases = BTreeMap::from([("housekeeper".into(), "coder".into())]);
        let rewritten = apply_role_aliases(&wf, &aliases);
        match &rewritten.steps[1].action {
            Action::DispatchJob { spec, .. } => {
                assert_eq!(spec.profile.as_deref(), Some("@coder"));
            }
            other => panic!("expected dispatch, got {other:?}"),
        }
        match &rewritten.steps[0].action {
            Action::DispatchJob { spec, .. } => {
                assert_eq!(spec.profile.as_deref(), Some("@coder"));
            }
            other => panic!("expected dispatch, got {other:?}"),
        }
    }

    /// bd-8c5h: every `profile: "@role"` a workflow reaches, through its own
    /// steps AND its graph nodes, at any control-flow depth.
    fn dispatch_profiles(wf: &Workflow) -> Vec<String> {
        fn walk(action: &Action, out: &mut Vec<String>) {
            match action {
                Action::DispatchJob { spec, .. } => {
                    if let Some(p) = spec.profile.as_deref() {
                        out.push(p.trim_start_matches('@').to_string());
                    }
                }
                // bd-8c5h: a fan-out or a branch arm is still a dispatch site.
                // Walking only the top level would miss a folded role nested in
                // one — which is the half of the document nobody checked.
                Action::Parallel { each, .. } | Action::Foreach { each, .. } => {
                    walk(&each.action, out)
                }
                Action::Retry { step, .. } => walk(&step.action, out),
                Action::Loop { body, .. } => {
                    for s in body {
                        walk(&s.action, out);
                    }
                }
                Action::Branch {
                    then, else_steps, ..
                } => {
                    for s in then.iter().chain(else_steps.iter()) {
                        walk(&s.action, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        for s in &wf.steps {
            walk(&s.action, &mut out);
        }
        if let Some(g) = &wf.graph {
            for node in g.nodes.values() {
                walk(&node.action, &mut out);
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// bd-8c5h: THE remaining gap. `apply_role_aliases` rewrites the document
    /// it is handed, so a fold only lands when the PARENT dispatches the role
    /// itself. `goal` dispatches nothing (every node is a `call_workflow`) and
    /// the role it needs is dispatched by the CHILD — so the rewrite is a
    /// no-op and the fold the packer just announced never happens.
    ///
    /// The fold therefore has to travel with the RESOLVER, which is the only
    /// thing that ever sees a child. This pins that against the REAL builtins:
    /// if `goal` ever starts dispatching a role itself, or `ce-brainstorm`
    /// stops dispatching the folded one, the preconditions fail loudly instead
    /// of the test quietly asserting nothing.
    #[test]
    fn the_fold_reaches_the_child_that_dispatches_the_role() {
        let catalog = crate::builtin_workflows::builtin_workflows();
        let goal = catalog.get("goal").expect("goal builtin");
        let child = catalog.get("ce-brainstorm").expect("ce-brainstorm builtin");

        // Precondition 1: the parent dispatches roles, but NOT the folded one.
        // That is the whole gap — a rewrite of the parent can only ever reach
        // what the parent dispatches, so a role only the child dispatches is
        // unreachable no matter how many times the parent is rewritten.
        let parent_roles = dispatch_profiles(goal);
        assert!(
            !parent_roles.is_empty(),
            "goal dispatches nothing itself — re-check whether the child gap still exists"
        );
        assert!(
            !parent_roles.contains(&"oracle".to_string()),
            "goal now dispatches @oracle itself, so the child gap is gone: {parent_roles:?}"
        );
        // Precondition 2: the child is the one that dispatches the folded role.
        assert!(
            dispatch_profiles(child).contains(&"oracle".to_string()),
            "ce-brainstorm no longer dispatches @oracle: {:?}",
            dispatch_profiles(child)
        );

        let aliases: BTreeMap<String, String> =
            BTreeMap::from([("oracle".to_string(), "coder".to_string())]);

        // The bug, as an assertion: the existing rewrite cannot touch a role
        // the parent never dispatches, so rewriting the parent changes nothing
        // and @oracle survives in the child untouched.
        let rewritten_parent = apply_role_aliases(goal, &aliases);
        let parent_after = dispatch_profiles(&rewritten_parent);
        assert_eq!(
            parent_after, parent_roles,
            "the parent rewrite changed a role the parent never dispatches — \
             that would mean the gap closed some other way: {parent_roles:?} vs {parent_after:?}"
        );

        // The fix: the fold rides the resolver, so the child is rewritten
        // before the engine ever sees it.
        let raw = std::sync::Arc::new(child.clone());
        let base: toron_workflow::WorkflowResolver = std::sync::Arc::new(move |name: &str| {
            (name == "ce-brainstorm").then(|| (*raw).clone())
        });
        let resolver = aliasing_resolver(base, aliases);

        let folded = resolver("ce-brainstorm").expect("the child still resolves");
        let after = dispatch_profiles(&folded);
        assert!(
            !after.contains(&"oracle".to_string()),
            "the fold never reached the child document: {after:?}"
        );
        assert!(
            after.contains(&"coder".to_string()),
            "the host role is missing from the rewritten child: {after:?}"
        );
        // A fold rewrites ONE role, not the document: an unfurled role in the
        // same child must survive.
        assert!(
            after.contains(&"reviewer".to_string()),
            "an unfurled role was rewritten too: {after:?}"
        );
    }

    /// The wrapper must be a no-op when the packer folded nothing, so every
    /// crew that fits its budget is byte-identical to the pre-bd-8c5h path.
    #[test]
    fn aliasing_resolver_with_no_fold_is_the_identity() {
        let raw = std::sync::Arc::new(
            parse_workflow(
                "name: solo\ntrigger: { type: webhook }\nsteps:\n  - name: a\n    action: dispatch_job\n    assignee: x\n    spec: { profile: \"@oracle\" }\n",
            )
            .expect("parses"),
        );
        let base: toron_workflow::WorkflowResolver =
            std::sync::Arc::new(move |_: &str| Some((*raw).clone()));
        let resolver = aliasing_resolver(base, BTreeMap::new());
        let got = resolver("anything").expect("resolves");
        assert_eq!(dispatch_profiles(&got), vec!["oracle".to_string()]);
    }

    /// Wrapping must not invent a document for a child the base could not
    /// resolve: the engine's own "no such sub-workflow" error is the honest
    /// one, and a synthesized empty child would run and report success.
    #[test]
    fn aliasing_resolver_preserves_an_unresolvable_child() {
        let base: toron_workflow::WorkflowResolver = std::sync::Arc::new(|_: &str| None);
        let resolver = aliasing_resolver(
            base,
            BTreeMap::from([("oracle".to_string(), "coder".to_string())]),
        );
        assert!(
            resolver("nope").is_none(),
            "wrapping invented a document for an unresolvable child"
        );
    }

    #[test]
    fn pack_inferred_default_caps_at_max() {
        let mut members = BTreeMap::new();
        for r in [
            "coder",
            "reviewer",
            "planner",
            "scout",
            "housekeeper",
            "oracle",
        ] {
            members.insert(r.to_string(), 1);
        }
        let inferred = CrewPlan { members };
        let packed = pack_inferred(&inferred, &CrewFlag::Default, 5).expect("pack");
        assert_eq!(packed.total(), 5);
        assert_eq!(packed.unique, 6);
        assert_eq!(packed.aliases.len(), 1);
    }

    // ---- bd-0s0y: crew demand / self-collision ----

    fn wf_from(yaml: &str) -> Workflow {
        toron_workflow::schema::parse_workflow(yaml).expect("yaml parses")
    }

    /// bd-0s0y: the three builtins the shape fence flagged must now PACK — each
    /// role gets the copies its widest wave needs, with no aliasing. This is
    /// half 2c: before it, `pack_inferred` passed only the role KEYS, so every
    /// count was reset to 1 and each of these runs was refused at dispatch.
    #[test]
    fn the_multi_copy_worklist_now_packs_with_real_copies() {
        let catalog = crate::builtin_workflows::builtin_workflows();
        for (name, expected) in [
            ("debate", [("oracle", 1u32), ("reviewer", 2u32)].as_slice()),
            ("phase-review", [("reviewer", 2u32)].as_slice()),
            (
                "weekly-review",
                [("oracle", 1u32), ("scout", 3u32)].as_slice(),
            ),
        ] {
            let shape = crew_demand(&catalog[name]).crew_shape;
            let got: Vec<(String, u32)> = shape.iter().map(|(k, v)| (k.clone(), *v)).collect();
            let want: Vec<(String, u32)> =
                expected.iter().map(|(k, v)| (k.to_string(), *v)).collect();
            assert_eq!(got, want, "{name} shape");

            let needed: u32 = shape.values().sum();
            assert!(needed <= DEFAULT_CREW_MAX, "{name} must fit the cap");
            let packed = pack_crew_shape(&shape, needed, DEFAULT_CREW_MAX)
                .unwrap_or_else(|e| panic!("{name} must pack: {e}"));
            assert!(
                packed.aliases.is_empty(),
                "{name} must not alias: a multi-copy role folded onto a host is the collision \
                 this change exists to prevent, got {:?}",
                packed.aliases
            );
            for (role, copies) in &shape {
                assert_eq!(
                    packed.members.get(role).copied(),
                    Some(*copies),
                    "{name} must seat {copies} pane(s) for @{role}, got {:?}",
                    packed.members
                );
            }
        }
    }

    /// bd-0s0y: a role that genuinely needs more panes than the cap allows
    /// fails LOUD, naming the role. It must never be folded onto a host — that
    /// is the collision — and it must never be silently truncated to one pane,
    /// which is the original defect.
    #[test]
    fn a_shape_over_the_cap_fails_loud_and_never_folds_a_multi_copy_role() {
        // 3 oracles + 3 planners = 6 > cap 5. Every role is multi-copy, so
        // there is nothing safe to fold.
        let shape: BTreeMap<String, u32> = [("oracle", 3u32), ("planner", 3u32)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let err = pack_crew_shape(&shape, 6, DEFAULT_CREW_MAX)
            .expect_err("6 panes cannot fit a cap of 5");
        assert!(
            err.contains("6 panes") || err.contains("crew.max=5"),
            "the error must state the shortfall: {err}"
        );

        // With foldable roles it packs by folding ONLY single-copy ones: 6
        // panes of demand, budget/cap 5, and the surplus comes out of `scout`
        // (and then `reviewer`) — never out of the 3-way oracle.
        let shape: BTreeMap<String, u32> = [
            ("oracle", 3u32),
            ("planner", 1u32),
            ("scout", 1u32),
            ("reviewer", 1u32),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let packed = pack_crew_shape(&shape, 5, DEFAULT_CREW_MAX)
            .expect("6 panes of demand, 2 foldable, budget 5");
        assert_eq!(
            packed.members.get("oracle").copied(),
            Some(3),
            "a multi-copy role is never the fold: {members:?}",
            members = packed.members
        );
        assert_eq!(packed.members.get("planner").copied(), Some(1));
        assert_eq!(packed.members.values().sum::<u32>(), 5);
        assert_eq!(
            packed.aliases.len(),
            1,
            "exactly one single-copy role folded: {:?}",
            packed.aliases
        );
    }

    /// The regression itself, in miniature. This is the shape that killed
    /// every crew run: one node, one role, three concurrent copies, and a crew
    /// that holds one pane per role.
    #[test]
    fn a_parallel_fanout_over_one_role_is_a_collision() {
        let wf = wf_from(
            r#"
name: fanout-probe
steps:
  - name: diverge
    action: parallel
    over: "3"
    each:
      action: dispatch_job
      assignee: x
      spec: { profile: "@oracle", task: "diverge" }
"#,
        );
        let d = crew_demand(&wf);
        assert_eq!(d.required_panes, 1, "one role, so one pane is the cost");
        assert_eq!(
            d.collisions,
            vec![("oracle".to_string(), 3)],
            "three concurrent copies of one role is exactly the refusal: {d:?}"
        );
    }

    /// Sequential reuse of a role is FREE — the fence releases on permit drop
    /// — so a chain that reuses a role must NOT be flagged. This is what makes
    /// the lint usable rather than noise: flagging this would push authors to
    /// invent panes they do not need.
    #[test]
    fn sequential_reuse_of_a_role_costs_nothing() {
        let wf = wf_from(
            r#"
name: chain-probe
steps:
  - name: reduce
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", task: "a" }
  - name: plan
    action: dispatch_job
    assignee: x
    spec: { profile: "@planner", task: "b" }
  - name: build
    action: dispatch_job
    assignee: x
    spec: { profile: "@coder", task: "c" }
"#,
        );
        let d = crew_demand(&wf);
        assert!(
            d.collisions.is_empty(),
            "sequential reuse is not a collision"
        );
    }

    /// Concurrent nodes on DISTINCT roles are what a fan-out should look like:
    /// three panes, no collision.
    #[test]
    fn concurrent_nodes_on_distinct_roles_cost_one_pane_each() {
        let wf = wf_from(
            r#"
name: spread-probe
graph:
  nodes:
    a:
      action: dispatch_job
      assignee: x
      spec: { profile: "@oracle", task: "diverge" }
    b:
      action: dispatch_job
      assignee: x
      spec: { profile: "@planner", task: "structure" }
    c:
      action: dispatch_job
      assignee: x
      spec: { profile: "@reviewer", task: "risk" }
    reduce:
      action: dispatch_job
      assignee: x
      spec: { profile: "@planner", stage: "ce-brainstorm", task: "reduce" }
      needs: [a, b, c]
"#,
        );
        let d = crew_demand(&wf);
        assert!(d.collisions.is_empty(), "{d:?}");
        assert_eq!(
            d.required_panes, 3,
            "the three divergers share a wave; reduce is a later wave and reuses the planner"
        );
    }

    /// THE fence. Every builtin must PACK on a cap-sized crew: the shape its
    /// widest wave needs has to fit, and no role that needs copies may end up
    /// aliased onto another agent — that is the busy-fence refusal.
    ///
    /// `ce-brainstorm` is why this walks the catalog and not one file: the
    /// fatal fan-out lived in a CHILD of `goal`, so checking `goal`'s own steps
    /// passed while the run still died on step one. And "packs" rather than
    /// "fits", because a shape that fits on paper but cannot be packed is still
    /// a dead run.
    ///
    /// bd-07m2: this used to pack at `budget = crew_total.min(cap)`, which makes
    /// `n == total`, so the fold loop never ran, `aliases` stayed empty and
    /// BOTH collision assertions were dead — the test could not fail on the
    /// aliasing defect its own text claimed to catch. It now also packs at a
    /// budget one pane SHORT of the shape, which is what forces the fold path,
    /// so the aliasing branch is reachable and is actually exercised.
    #[test]
    fn every_builtin_packs_on_a_cap_sized_crew() {
        let catalog = crate::builtin_workflows::builtin_workflows();
        let mut broken: Vec<String> = Vec::new();
        for (name, wf) in &catalog {
            let d = crew_demand(wf);
            if d.crew_total == 0 {
                continue;
            }
            // Two budgets on purpose: the natural one (shape fits, no fold) and
            // a fold-forcing one (one pane short, so single-copy roles fold and
            // a multi-copy role must survive intact).
            let budgets = [
                d.crew_total.min(DEFAULT_CREW_MAX),
                d.crew_total.saturating_sub(1).max(1),
            ];
            for (i, budget) in budgets.into_iter().enumerate() {
                match pack_crew_shape(&d.crew_shape, budget, DEFAULT_CREW_MAX) {
                    Ok(p) => {
                        for (role, copies) in &d.collisions {
                            let seated = p.members.get(role).copied().unwrap_or(0);
                            let aliased = p.aliases.contains_key(role);
                            if aliased || seated < *copies as u32 {
                                broken.push(format!(
                                    "{name} @{role} (budget {budget}): needs {copies} concurrent \
                                     pane(s) but packed {seated} (aliased: {aliased}) — copies \
                                     2..{copies} will be refused by the busy-owner fence"
                                ));
                            }
                        }
                    }
                    // At the NATURAL budget an Err means the shape is over the
                    // cap with nothing left to fold — a real defect. At the
                    // SHORT budget folding is the point, so an Err there is
                    // expected and must not be reported.
                    Err(e) if i == 0 => broken.push(format!("{name}: {e}")),
                    Err(_) => {}
                }
            }
        }
        broken.sort();
        broken.dedup();
        assert!(
            broken.is_empty(),
            "{} builtin workflow(s) cannot be crewed:\n  {}",
            broken.len(),
            broken.join("\n  ")
        );
    }

    /// bd-q2gw: a TEMPLATED `over` must resolve to the width the engine really
    /// runs. `over: "{{inputs.reviewers}}"` with `reviewers` defaulted to "2"
    /// genuinely runs two concurrent copies — the engine binds the default and
    /// resolves the template — but parsing the raw string as a number yields 1.
    /// That seated one pane for a 2-wide fan-out and the busy-owner fence
    /// refused copy 1, killing `interrogate`, `prototype` and `arena` while this
    /// very catalog fence reported them clean.
    #[test]
    fn a_templated_fanout_resolves_to_its_declared_default() {
        let wf = wf_from(
            r#"
name: templated-probe
inputs:
  - name: reviewers
    default: "2"
steps:
  - name: fan
    action: parallel
    over: "{{inputs.reviewers}}"
    each:
      action: dispatch_job
      assignee: x
      spec: { profile: "@reviewer", task: "review" }
"#,
        );
        let d = crew_demand(&wf);
        assert_eq!(
            d.collisions,
            vec![("reviewer".to_string(), 2)],
            "a templated width must not degrade to 1: {d:?}"
        );
        assert_eq!(d.crew_shape.get("reviewer").copied(), Some(2));
    }

    /// bd-q2gw: the three builtins that were invisible. Named explicitly, so a
    /// future edit that re-templates one of them fails here rather than at
    /// dispatch time with a fence error that names the wrong cause.
    #[test]
    fn the_templated_fanout_builtins_are_seated_with_copies() {
        let catalog = crate::builtin_workflows::builtin_workflows();
        for (name, role, copies) in [
            ("interrogate", "reviewer", 2u32),
            ("prototype", "coder", 2),
            ("arena", "coder", 2),
        ] {
            let shape = crew_demand(&catalog[name]).crew_shape;
            assert_eq!(
                shape.get(role).copied(),
                Some(copies),
                "{name}: @{role} must be seated with {copies} copies, got {shape:?}"
            );
            let packed =
                pack_crew_shape(&shape, shape.values().sum(), DEFAULT_CREW_MAX).expect("packs");
            assert_eq!(packed.members.get(role).copied(), Some(copies));
        }
    }

    /// bd-q2gw: an UNRESOLVABLE width must stay 1, never be guessed upward.
    /// Over-seating wastes panes against a hard cap of 5, so a width we cannot
    /// read has to fail toward the existing behaviour, not invent panes.
    #[test]
    fn an_unresolvable_fanout_width_stays_one() {
        let none: BTreeMap<String, String> = BTreeMap::new();
        assert_eq!(resolve_over("{{inputs.missing}}", &none), 1);
        assert_eq!(
            resolve_over(
                "{{inputs.missing}}",
                &BTreeMap::from([("missing".to_string(), "not-a-number".to_string())])
            ),
            1
        );
        assert_eq!(resolve_over("2", &none), 2);
        assert_eq!(resolve_over("0", &none), 1, "zero is not a fan-out");
        assert_eq!(
            resolve_over(
                "{{inputs.n}}",
                &BTreeMap::from([("n".to_string(), "3".to_string())])
            ),
            3
        );
    }

    /// `goal` specifically: five panes, zero collisions, and full three-way
    /// divergence. This is the analysis the request asked for, pinned so a
    /// future edit cannot quietly re-break it.
    #[test]
    fn goal_fits_five_panes_with_three_way_divergence() {
        let catalog = crate::builtin_workflows::builtin_workflows();

        // The chain itself: 3 dispatch sites on 3 distinct roles, sequential.
        let goal = catalog.get("goal").expect("goal builtin");
        assert_eq!(
            crew_demand(goal).required_panes,
            1,
            "goal's own steps are sequential"
        );

        // The child that actually costs panes, and it must be clean.
        let cb = catalog.get("ce-brainstorm").expect("ce-brainstorm builtin");
        let d = crew_demand(cb);
        assert!(
            d.collisions.is_empty(),
            "ce-brainstorm self-collides: {d:?}"
        );
        assert_eq!(
            d.required_panes, 3,
            "three independent divergers, three panes"
        );

        // Divergence is preserved, not traded away: three distinct lenses.
        let lenses: Vec<String> = ["diverge_oracle", "diverge_structure", "diverge_risk"]
            .into_iter()
            .filter(|n| cb.graph.as_ref().is_some_and(|g| g.nodes.contains_key(*n)))
            .map(str::to_string)
            .collect();
        assert_eq!(
            lenses.len(),
            3,
            "the fan-out is still 3-way, just spread across roles: {lenses:?}"
        );

        // The union of roles across goal's whole call graph is the crew it
        // needs, and it is 5 — the cap, not over it.
        let mut roles: std::collections::BTreeSet<String> = Default::default();
        for wf_name in ["goal", "ce-brainstorm", "phase-plan", "beads"] {
            let wf = catalog.get(wf_name).expect("workflow in goal's call graph");
            for action in wf
                .graph
                .iter()
                .flat_map(|g| g.nodes.values().map(|n| &n.action))
                .chain(wf.steps.iter().map(|s| &s.action))
            {
                let mut uses = Vec::new();
                role_uses(action, &BTreeMap::new(), &mut uses);
                roles.extend(uses);
            }
        }
        assert_eq!(
            roles.len(),
            5,
            "goal's call graph needs exactly one pane per role, and that is 5: {roles:?}"
        );
    }
}

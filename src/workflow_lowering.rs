//! Lower catalog workflows to elm-effect-xstate machines and check the
//! D5 topology gates (ADR-0021 D5.1-D5.3, bd-lower-workflows-verify-9mdqc).
//!
//! First slice: reachable, can-finish, bounded cycles. The witnessed-exit
//! check rides a later slice (needs bd-witnessed-exits-hd035). Pure logic:
//! no I/O, no clock, no ambient state.

use std::collections::{BTreeMap, BTreeSet};

use toron_core::kernel::{Machine, StateNode, Topology};
use toron_workflow::schema::{Action, Graph, Step, StepOnError, Workflow};

/// Terminal states plus the synthetic entry.
const DONE: &str = "Done";
const FAILED: &str = "Failed";
const START: &str = "Start";

/// A recorded back edge: body end to body start, with the bound
/// that justifies it. `max: None` means no statically finite bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackEdge {
    pub from: String,
    pub to: String,
    pub max: Option<u64>,
    pub kind: BackEdgeKind,
}

/// The control construct that owns a back edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackEdgeKind {
    Loop,
    Retry,
}

/// A lowered workflow: the machine plus the back-edge list the bounded
/// check reads (never guards).
pub struct Lowered {
    pub machine: Machine<String, (), (), ()>,
    pub back_edges: Vec<BackEdge>,
}

/// Lowering failures: only workflows with no states at all.
#[derive(Debug, PartialEq, Eq)]
pub enum LowerError {
    NoStates,
}

/// Lower a workflow to a state machine: one state per step or graph node.
/// Linear steps chain to the next step; graph nodes go to their dependents
/// through `needs:`. Branch arms fan out to arm starts and rejoin at the
/// container exit (engine-faithful: PlanEntry::ArmEnd continues after the
/// branch). Loop/retry add a back edge from the body end to the body start,
/// plus an exit edge. Foreach/parallel is one compound state over its
/// `each`. Sinks reach Done; `fail_run` steps also reach Failed.
pub fn lower(wf: &Workflow) -> Result<Lowered, LowerError> {
    let inputs: Vec<(String, Option<String>)> = wf
        .inputs
        .iter()
        .map(|i| {
            let def = i.default.as_ref().and_then(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Number(n) => Some(n.to_string()),
                _ => None,
            });
            (i.name.clone(), def)
        })
        .collect();
    let mut b = LowerBuilder::new(inputs);
    b.kinds.insert(START.to_string(), NodeKind::Atomic);
    match (&wf.steps, &wf.graph) {
        (steps, None) if !steps.is_empty() => {
            let labels = b.lower_steps(steps, &[DONE.to_string()], "");
            if let Some(first) = labels.first() {
                b.edge(START, first);
            }
        }
        (_, Some(graph)) if !graph.nodes.is_empty() => {
            let entries = b.lower_graph(graph);
            for e in &entries {
                b.edge(START, e);
            }
        }
        (steps, _) if !steps.is_empty() => {
            let labels = b.lower_steps(steps, &[DONE.to_string()], "");
            if let Some(first) = labels.first() {
                b.edge(START, first);
            }
        }
        _ => return Err(LowerError::NoStates),
    }
    b.build()
}

/// Structural kind of a lowered state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeKind {
    Atomic,
    Compound { initial: String },
    Final,
}

/// Mutable lowering scratch space: states, edges, back edges, and the
/// input defaults loop `max` templates resolve through.
struct LowerBuilder {
    kinds: BTreeMap<String, NodeKind>,
    edges: BTreeMap<String, Vec<String>>,
    parents: BTreeMap<String, String>,
    back_edges: Vec<BackEdge>,
    used: BTreeSet<String>,
    inputs: Vec<(String, Option<String>)>,
}

impl LowerBuilder {
    fn new(inputs: Vec<(String, Option<String>)>) -> Self {
        Self {
            kinds: BTreeMap::new(),
            edges: BTreeMap::new(),
            parents: BTreeMap::new(),
            back_edges: Vec::new(),
            used: [START, DONE, FAILED]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            inputs,
        }
    }

    /// Fresh deterministic label; reserved names always win.
    fn take(&mut self, base: &str) -> String {
        if self.used.insert(base.to_string()) {
            return base.to_string();
        }
        let mut n = 2u32;
        loop {
            let cand = format!("{base}#{n}");
            if self.used.insert(cand.clone()) {
                return cand;
            }
            n += 1;
        }
    }

    /// Add an edge, declaring endpoints Atomic unless already known.
    /// Done/Failed resolve to Final at build.
    fn edge(&mut self, from: &str, to: &str) {
        self.kinds
            .entry(from.to_string())
            .or_insert(NodeKind::Atomic);
        self.used.insert(from.to_string());
        self.kinds.entry(to.to_string()).or_insert(NodeKind::Atomic);
        self.used.insert(to.to_string());
        let entry = self.edges.entry(from.to_string()).or_default();
        if !entry.iter().any(|t| t == to) {
            entry.push(to.to_string());
        }
    }

    /// Resolve a loop `max` to a finite number: a plain integer, or a pure
    /// `{{inputs.X}}` ref through its declared default (the same precedent
    /// the workflow lint uses for profiles). Anything else is unbounded.
    fn resolve_max(&self, max: &str) -> Option<u64> {
        if let Ok(n) = max.trim().parse::<u64>() {
            return Some(n);
        }
        let t = max.trim();
        let inner = t.strip_prefix("{{")?.strip_suffix("}}")?.trim();
        let root = inner
            .strip_prefix("inputs.")?
            .split('.')
            .next()
            .unwrap_or("");
        if root.is_empty()
            || root.contains(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
        {
            return None;
        }
        for (name, def) in &self.inputs {
            if name == root {
                if let Some(d) = def {
                    if let Ok(n) = d.trim().parse::<u64>() {
                        return Some(n);
                    }
                }
                return None;
            }
        }
        None
    }

    /// Lower a linear step list; returns the labels in order.
    fn lower_steps(&mut self, steps: &[Step], exit: &[String], prefix: &str) -> Vec<String> {
        let mut labels = Vec::new();
        for (i, s) in steps.iter().enumerate() {
            let base = s.name.clone().unwrap_or_else(|| format!("step_{}", i + 1));
            let full = if prefix.is_empty() {
                base
            } else {
                format!("{prefix}/{base}")
            };
            labels.push(self.take(&full));
        }
        for (i, s) in steps.iter().enumerate() {
            let me = labels[i].clone();
            let next: Vec<String> = if i + 1 < labels.len() {
                vec![labels[i + 1].clone()]
            } else {
                exit.to_vec()
            };
            self.lower_action(&s.action, &me, &next, &s.on_error);
        }
        labels
    }

    /// Lower a graph: nodes wired through transitive `needs:` ancestors,
    /// branch arms rejoining node dependents. Returns the entry labels.
    fn lower_graph(&mut self, graph: &Graph) -> Vec<String> {
        let nodes = &graph.nodes;
        let mut renames = BTreeMap::new();
        for name in nodes.keys() {
            renames.insert(name.clone(), self.take(name));
        }
        let mut entries: Vec<String> = nodes
            .iter()
            .filter(|(_, n)| n.needs.is_empty())
            .map(|(name, _)| renames[name].clone())
            .collect();
        if entries.is_empty() {
            if let Some(first) = nodes.keys().next() {
                entries.push(renames[first].clone());
            }
        }
        for (name, node) in nodes {
            let me = renames[name].clone();
            let mut deps: Vec<String> = nodes
                .iter()
                .filter(|(_, n)| n.needs.iter().any(|d| d == name))
                .map(|(m, _)| renames[m].clone())
                .collect();
            if deps.is_empty() {
                deps.push(DONE.to_string());
            }
            self.lower_action(&node.action, &me, &deps, &node.on_error);
        }
        entries
    }

    /// Lower one action at `here`, fanning out control flow into
    /// path-prefixed sub-states. `exit` is the continuation.
    fn lower_action(
        &mut self,
        action: &Action,
        here: &str,
        exit: &[String],
        on_error: &StepOnError,
    ) {
        if matches!(on_error, StepOnError::FailRun) {
            self.edge(here, FAILED);
        }
        match action {
            Action::Loop { max, body, .. } => {
                for e in exit {
                    self.edge(here, e);
                }
                if body.is_empty() {
                    return;
                }
                let kids = self.arm_labels(here, None, body);
                self.edge(here, &kids[0]);
                for (j, b) in body.iter().enumerate() {
                    if j + 1 < kids.len() {
                        let next = vec![kids[j + 1].clone()];
                        self.lower_action(&b.action, &kids[j], &next, &b.on_error);
                    } else {
                        self.lower_action(&b.action, &kids[j], exit, &b.on_error);
                        let last = kids[j].clone();
                        let first = kids[0].clone();
                        let max_n = self.resolve_max(max);
                        self.back_edges.push(BackEdge {
                            from: last.clone(),
                            to: first.clone(),
                            max: max_n,
                            kind: BackEdgeKind::Loop,
                        });
                        self.edge(&last, &first);
                    }
                }
            }
            Action::Retry {
                step, max_attempts, ..
            } => {
                for e in exit {
                    self.edge(here, e);
                }
                let inner = step.name.clone().unwrap_or_else(|| "run".to_string());
                let r = self.take(&format!("{here}/{inner}"));
                self.edge(here, &r);
                self.back_edges.push(BackEdge {
                    from: r.clone(),
                    to: r.clone(),
                    max: Some(*max_attempts as u64),
                    kind: BackEdgeKind::Retry,
                });
                self.edge(&r, &r);
                self.lower_action(&step.action, &r, exit, &step.on_error);
            }
            Action::Branch {
                then, else_steps, ..
            } => {
                for e in exit {
                    self.edge(here, e);
                }
                self.lower_arm(then, here, Some("then"), exit);
                self.lower_arm(else_steps, here, Some("else"), exit);
            }
            Action::Foreach { each, .. } | Action::Parallel { each, .. } => {
                let inner = each.name.clone().unwrap_or_else(|| "each".to_string());
                let c = self.take(&format!("{here}/{inner}"));
                self.kinds
                    .insert(here.to_string(), NodeKind::Compound { initial: c.clone() });
                self.used.insert(here.to_string());
                self.parents.insert(c.clone(), here.to_string());
                self.lower_action(&each.action, &c, exit, &each.on_error);
            }
            _ => {
                for e in exit {
                    self.edge(here, e);
                }
            }
        }
    }

    /// Labels for one arm body under `parent[/arm]`.
    fn arm_labels(&mut self, parent: &str, arm: Option<&str>, body: &[Step]) -> Vec<String> {
        body.iter()
            .enumerate()
            .map(|(j, s)| {
                let base = s.name.clone().unwrap_or_else(|| format!("step_{}", j + 1));
                let full = match arm {
                    Some(a) => format!("{parent}/{a}/{base}"),
                    None => format!("{parent}/{base}"),
                };
                self.take(&full)
            })
            .collect()
    }

    /// Lower one branch arm: chain internally, then rejoin the container
    /// exit (engine-faithful: PlanEntry::ArmEnd continues with the step
    /// after the branch).
    fn lower_arm(&mut self, arm: &[Step], here: &str, which: Option<&str>, exit: &[String]) {
        if arm.is_empty() {
            return;
        }
        let kids = self.arm_labels(here, which, arm);
        self.edge(here, &kids[0]);
        for (j, b) in arm.iter().enumerate() {
            if j + 1 < kids.len() {
                let next = vec![kids[j + 1].clone()];
                self.lower_action(&b.action, &kids[j], &next, &b.on_error);
            } else {
                self.lower_action(&b.action, &kids[j], exit, &b.on_error);
            }
        }
    }

    /// Assemble the machine: Done/Failed resolve to Final when referenced.
    fn build(mut self) -> Result<Lowered, LowerError> {
        for name in [DONE, FAILED] {
            let referenced = self.edges.values().any(|ts| ts.iter().any(|t| t == name));
            if referenced {
                self.kinds.insert(name.to_string(), NodeKind::Final);
            } else {
                self.kinds.remove(name);
            }
        }
        if self.kinds.is_empty() {
            return Err(LowerError::NoStates);
        }
        let mut b: toron_core::kernel::MachineBuilder<String, (), (), ()> =
            Machine::builder(START.to_string(), ());
        let ids: Vec<String> = self.kinds.keys().cloned().collect();
        for id in &ids {
            let targets: Vec<String> = self.edges.get(id).cloned().unwrap_or_default();
            let parent = self.parents.get(id).cloned();
            b = match self.kinds[id] {
                NodeKind::Final => b.final_state(id.clone(), |n| n),
                NodeKind::Compound { ref initial } => {
                    let initial = initial.clone();
                    b.compound(id.clone(), initial, |n| with_targets(n, &targets))
                }
                NodeKind::Atomic => b.state(id.clone(), |n| {
                    let n = match parent {
                        Some(p) => n.with_parent(p),
                        None => n,
                    };
                    with_targets(n, &targets)
                }),
            };
        }
        Ok(Lowered {
            machine: b.build(),
            back_edges: self.back_edges,
        })
    }
}

/// Attach unconditional edges to a state node (topology counts every guard
/// as enabled, so one unconditional transition per target).
fn with_targets(
    node: StateNode<String, (), (), ()>,
    targets: &[String],
) -> StateNode<String, (), (), ()> {
    let mut node = node;
    for t in targets {
        let tr = node.transition(t.clone()).on(|_: &()| true);
        node = node.on(tr);
    }
    node
}

pub fn check_reachable(lowered: &Lowered) -> Vec<String> {
    lowered.machine.topology().unreachable()
}

/// Reachable leaves that cannot reach a top-level final state, by name.
///
/// No YAML form produced by [`lower`] can strand a state today: branch arms
/// rejoin the container exit (engine-faithful) and loops carry exits, so a
/// hand-built machine is the only way to exercise this check.
pub fn check_can_finish(lowered: &Lowered) -> Vec<String> {
    lowered.machine.topology().cannot_finish()
}

/// Back edges without a finite max are errors; a finite max above 32 is a
/// long-hold warning. Unrecorded cycles are errors. Returns (errors, warnings).
pub fn check_bounded(lowered: &Lowered) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut recorded = BTreeSet::new();
    for b in &lowered.back_edges {
        recorded.insert((b.from.clone(), b.to.clone()));
        if b.max.is_none() {
            errors.push(format!(
                "unbounded repetition at {} (no finite max)",
                b.from
            ));
        }
    }
    let topo = lowered.machine.topology();
    let mut visited = BTreeSet::new();
    let mut on_stack = BTreeSet::new();
    dfs_cycle(
        &topo.initial,
        &topo,
        &recorded,
        &mut visited,
        &mut on_stack,
        &mut errors,
    );
    for b in &lowered.back_edges {
        if let Some(n) = b.max {
            if n > 32 {
                let what = match b.kind {
                    BackEdgeKind::Loop => format!("loop max={n}"),
                    BackEdgeKind::Retry => format!("retry max_attempts={n}"),
                };
                warnings.push(format!("{}: {} (long hold)", b.from, what));
            }
        }
    }
    (errors, warnings)
}

/// Depth-first search for back edges the lowering did not record: every
/// directed cycle contains one, and an unrecorded one has no bound.
fn dfs_cycle(
    u: &str,
    topo: &Topology<String>,
    recorded: &BTreeSet<(String, String)>,
    visited: &mut BTreeSet<String>,
    on_stack: &mut BTreeSet<String>,
    issues: &mut Vec<String>,
) {
    visited.insert(u.to_string());
    on_stack.insert(u.to_string());
    if let Some(node) = topo.nodes.get(u) {
        for v in &node.targets {
            if !visited.contains(v) {
                dfs_cycle(v, topo, recorded, visited, on_stack, issues);
            } else if on_stack.contains(v) && !recorded.contains(&(u.to_string(), v.clone())) {
                issues.push(format!("unbounded cycle {u} -> {v}"));
            }
        }
    }
    on_stack.remove(u);
}

#[cfg(test)]
mod tests {
    use super::*;
    use toron_workflow::schema::parse_workflow;

    fn lowered(yaml: &str) -> Lowered {
        let wf = parse_workflow(yaml).expect("fixture parses");
        lower(&wf).expect("fixture lowers")
    }

    /// A graph node no path reaches fails exactly the reachable check.
    #[test]
    fn workflow_lowering_unreachable_node_named() {
        let l = lowered(
            r#"
name: dead
trigger:
  type: webhook
inputs: []
graph:
  nodes:
    a:
      action: dispatch_job
      assignee: x
      spec: { profile: "@coder" }
    b:
      action: dispatch_job
      assignee: x
      spec: { profile: "@coder" }
      needs: [a]
    c:
      action: dispatch_job
      assignee: x
      spec: { profile: "@coder" }
      needs: [zzz]
"#,
        );
        assert_eq!(check_reachable(&l), vec!["c".to_string()]);
        assert!(check_can_finish(&l).is_empty());
        let (errs, warns) = check_bounded(&l);
        assert!(errs.is_empty());
        assert!(warns.is_empty());
    }

    /// Branch arms rejoin the container exit (engine-faithful), so a
    /// steps-form branch reaches Done down every arm.
    #[test]
    fn workflow_lowering_branch_arms_rejoin() {
        let l = lowered(
            r#"
name: rejoined
trigger:
  type: webhook
inputs: []
steps:
  - name: fork
    action: branch
    if: 'trigger_x == "yes"'
    then:
      - name: good
        action: dispatch_job
        assignee: x
        spec: { profile: "@coder" }
    else:
      - name: bad
        action: dispatch_job
        assignee: x
        spec: { profile: "@coder" }
"#,
        );
        assert!(check_reachable(&l).is_empty());
        assert!(check_can_finish(&l).is_empty());
        let (errs, warns) = check_bounded(&l);
        assert!(errs.is_empty());
        assert!(warns.is_empty());
    }

    /// No YAML form strands a state today; a hand-built machine exercises
    /// the cannot-finish check directly.
    #[test]
    fn workflow_lowering_can_finish_names_stranded_state() {
        let machine: Machine<String, (), (), ()> = Machine::builder("a".to_string(), ())
            .state("a".to_string(), |n| n)
            .build();
        let l = Lowered {
            machine,
            back_edges: vec![],
        };
        assert!(check_reachable(&l).is_empty());
        assert_eq!(check_can_finish(&l), vec!["a".to_string()]);
        let (errs, warns) = check_bounded(&l);
        assert!(errs.is_empty());
        assert!(warns.is_empty());
    }

    /// A back edge with no finite max fails exactly the bounded check.
    #[test]
    fn workflow_lowering_unbounded_back_edge_named() {
        let l = lowered(
            r#"
name: unbounded
trigger:
  type: webhook
inputs: []
steps:
  - name: work
    action: loop
    max: "{{inputs.missing}}"
    until: "true"
    body:
      - name: polish
        action: dispatch_job
        assignee: x
        spec: { profile: "@coder" }
"#,
        );
        assert!(check_reachable(&l).is_empty());
        assert!(check_can_finish(&l).is_empty());
        let (errs, warns) = check_bounded(&l);
        assert_eq!(errs.len(), 1, "one unbounded back edge: {errs:?}");
        assert!(errs[0].contains("work/polish"), "{errs:?}");
        assert!(errs[0].contains("no finite max"), "{errs:?}");
        assert!(warns.is_empty());
    }

    /// A finite max above 32 passes with exactly one long-hold warning.
    #[test]
    fn workflow_lowering_retry_max_240_warns_once() {
        let l = lowered(
            r#"
name: patient
trigger:
  type: webhook
inputs: []
steps:
  - name: work
    action: retry
    max_attempts: 240
    step:
      action: dispatch_job
      assignee: x
      spec: { profile: "@coder" }
"#,
        );
        assert!(check_reachable(&l).is_empty());
        assert!(check_can_finish(&l).is_empty());
        let (errs, warns) = check_bounded(&l);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(warns.len(), 1, "one long-hold warning: {warns:?}");
        assert!(warns[0].contains("work/run"), "{warns:?}");
        assert!(warns[0].contains("retry max_attempts=240"), "{warns:?}");
        assert!(warns[0].contains("long hold"), "{warns:?}");
    }

    /// A loop with a finite max passes all three checks.
    #[test]
    fn workflow_lowering_finite_loop_passes() {
        let l = lowered(
            r#"
name: tidy
trigger:
  type: webhook
inputs: []
steps:
  - name: one
    action: dispatch_job
    assignee: x
    spec: { profile: "@coder" }
  - name: more
    action: loop
    max: 3
    until: 'steps_fence_ok == true'
    body:
      - name: polish
        action: dispatch_job
        assignee: x
        spec: { profile: "@coder" }
      - name: fence
        action: verify
        command: 'true'
"#,
        );
        assert!(check_reachable(&l).is_empty());
        assert!(check_can_finish(&l).is_empty());
        let (errs, warns) = check_bounded(&l);
        assert!(errs.is_empty());
        assert!(warns.is_empty());
    }

    /// Every compiled-in catalog workflow lowers and passes all three
    /// checks. A shipped failure stops the lane: report it, do not weaken
    /// the check.
    #[test]
    fn workflow_lowering_every_catalog_passes() {
        for (name, wf) in crate::builtin_workflows::builtin_workflows() {
            let lowered = lower(&wf).unwrap_or_else(|e| panic!("{name} lowers: {e:?}"));
            assert!(
                check_reachable(&lowered).is_empty(),
                "{name} reachable: {:?}",
                check_reachable(&lowered)
            );
            assert!(
                check_can_finish(&lowered).is_empty(),
                "{name} can finish: {:?}",
                check_can_finish(&lowered)
            );
            let (errs, warns) = check_bounded(&lowered);
            assert!(
                errs.is_empty(),
                "{name} bounded errors: {errs:?} warnings: {warns:?}"
            );
        }
    }
}

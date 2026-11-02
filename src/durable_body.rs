// governed-by: ADR-0043
//! One workflow body on the in-house durable engine, for flywheel's call sites.
//!
//! The toron MCP host has its own driver because it also publishes approvals
//! and terminals on a bus. Flywheel runs bodies in-process: cron ticks, crew
//! compositions, phase chains, coalition passes. They share THIS driver, so a
//! shape reaches the same verdict on every flywheel door.
//!
//! Rules (same as the host):
//! - model first: the driver persists through `DurableRun`/`DurableGraphRun`,
//!   then acts on the returned effects;
//! - one leaf policy (`run_linear_step` / `run_graph_node_step`);
//! - a spawn goes through the injected launcher and settles the park, bounded
//!   by the model's deadline;
//! - a `call_workflow` child is driven recursively with a deterministic child
//!   run id, so a restart re-attaches instead of opening a second child;
//! - an approval park has no resolution loop here, so it fails the body loudly
//!   instead of hanging.

use std::sync::Arc;

use serde_json::Value;
use toron_core::index::durable_engine::ToronEngineStore;
use toron_core::index::durable_workflow::SyncWorkflowState;
use toron_core::index::workflow_effects::WorkflowEffectStore;
use toron_core::journal::{JournalDb, ToronJournal};
use toron_core::kernel::OurEngine;
use toron_workflow::engine::durable::{GraphEffect, RunEffect, needs_graph_model};
use toron_workflow::engine::durable_host::{DurableGraphRun, DurableRun, HostError};
use toron_workflow::engine::leaf::LeafSeams;
use toron_workflow::engine::linear_step::{
    GraphNodeStep, LinearStep, run_graph_node_step, run_linear_step,
};
use toron_workflow::engine::spawn::{SpawnLauncher, SpawnSpec};
use toron_workflow::schema::Workflow;

type EngineStore = ToronEngineStore;
type Journal = ToronJournal;
type State = SyncWorkflowState;
type LinearRun = DurableRun<EngineStore, Journal, State>;
type GraphRun = DurableGraphRun<EngineStore, Journal, State>;

/// The seams one body needs.
pub struct BodySeams {
    /// Leaf execution (runner + verify sink + journal mode).
    pub leaf: LeafSeams,
    /// Starts a spawn child. `None` = the body refuses a spawn shape.
    pub launcher: Option<SpawnLauncher>,
    /// Resolves a `call_workflow` callee by name. `None` = refuse the shape.
    pub resolve_child: Option<WorkflowLookup>,
}

/// How one body ended.
pub enum BodyOutcome {
    /// The terminal output value.
    Completed(Value),
    /// A typed failure; `step` names the node when the model knows it.
    Failed {
        /// Failure text.
        error: String,
        /// Failed step/node, when known.
        step: Option<String>,
    },
}

/// Workflow name -> definition (a child-run resolver).
type WorkflowLookup = Arc<dyn Fn(&str) -> Option<Workflow> + Send + Sync>;

impl BodyOutcome {
    /// `Ok(output)` on success, `Err((error, step))` otherwise.
    ///
    /// # Errors
    ///
    /// Returns the failure text and failed step for a failed body.
    #[allow(dead_code)]
    pub fn into_result(self) -> Result<Value, (String, Option<String>)> {
        match self {
            Self::Completed(output) => Ok(output),
            Self::Failed { error, step } => Err((error, step)),
        }
    }
}

/// The production verify sink: run the command through `sh -c`.
///
/// Both engines shipped `VerifySink::default()` (no sink), so a `verify`
/// step has always answered `{ok:false, "verify sink missing"}`. Flywheel's
/// bodies are the first to install one, which is what makes a shell health
/// gate (docker-upgrade, loop `cycle`) mean what it says.
pub fn real_verify_sink() -> toron_workflow::engine::VerifySink {
    use toron_workflow::engine::{VerifyRequest, VerifyResult};
    let sink = toron_workflow::engine::VerifySink::default();
    let f = std::sync::Arc::new(|req: &VerifyRequest| -> anyhow::Result<VerifyResult> {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&req.command)
            .output()?;
        Ok(VerifyResult {
            ok: out.status.success(),
            exit: out.status.code().unwrap_or(-1),
            stdout_path: None,
            stderr_path: None,
            error: None,
        })
    });
    *sink
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(f);
    sink
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn handles(
    store: &WorkflowEffectStore,
    handle: &tokio::runtime::Handle,
) -> (EngineStore, Journal, State) {
    let pool = store.pool().clone();
    (
        ToronEngineStore::spawn(pool.clone(), handle),
        ToronJournal::spawn(JournalDb::new(pool.clone(), "body", 64), handle),
        SyncWorkflowState::spawn(pool, handle),
    )
}

async fn verb<T, F>(run_id: &str, what: &str, f: F) -> Option<T>
where
    F: FnOnce() -> Result<T, HostError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Some(v),
        Ok(Err(e)) => {
            eprintln!("durable body {run_id}: {what} failed: {e}");
            None
        }
        Err(e) => {
            eprintln!("durable body {run_id}: {what} task failed: {e}");
            None
        }
    }
}

/// Drive one workflow body to a terminal value.
///
/// `run_id` is the durable key: calling twice with the same id resumes the
/// persisted model instead of starting over.
pub async fn drive(
    store: &WorkflowEffectStore,
    run_id: &str,
    workflow: &Workflow,
    trigger: Value,
    seams: &BodySeams,
) -> BodyOutcome {
    let handle = tokio::runtime::Handle::current();
    let graph = needs_graph_model(workflow);
    if graph {
        drive_graph(store, &handle, run_id, workflow, trigger, seams).await
    } else {
        drive_linear(store, &handle, run_id, workflow, trigger, seams).await
    }
}

async fn drive_linear(
    store: &WorkflowEffectStore,
    handle: &tokio::runtime::Handle,
    run_id: &str,
    workflow: &Workflow,
    trigger: Value,
    seams: &BodySeams,
) -> BodyOutcome {
    let (engine_store, journal, state) = handles(store, handle);
    let wf = workflow.clone();
    let rid = run_id.to_string();
    let trig = trigger.clone();
    let opened = tokio::task::spawn_blocking(move || {
        let engine = OurEngine::with_journal(engine_store, journal);
        DurableRun::open(engine, state, rid, wf, trig, unix_ms())
    })
    .await;
    let (mut run, mut effects) = match opened {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return failed(format!("durable open: {e}"), None),
        Err(e) => return failed(format!("durable open task: {e}"), None),
    };
    if effects.is_empty() {
        return terminal_linear(&mut run);
    }
    let mut outputs: serde_json::Map<String, Value> =
        run.model().outputs.clone().into_iter().collect();
    loop {
        if effects.is_empty() {
            return failed("durable drive ran out of effects while non-terminal", None);
        }
        let mut follow: Vec<RunEffect> = Vec::new();
        for effect in std::mem::take(&mut effects) {
            match effect {
                RunEffect::ExecuteStep {
                    index,
                    step,
                    iteration,
                } => {
                    let value = run_linear_step(
                        LinearStep {
                            run_id,
                            workflow: &workflow.name,
                            workflow_digest: &digest(workflow),
                            step: &step,
                            index,
                            trigger: &trigger,
                            outputs: &mut outputs,
                            iteration,
                        },
                        &seams.leaf,
                    )
                    .await;
                    let then = move |mut run: LinearRun| {
                        run.step_done(value, unix_ms()).map(|eff| (run, eff))
                    };
                    let Some((back, mut eff)) = verb(run_id, "step feed", move || then(run)).await
                    else {
                        return failed("step feed failed", None);
                    };
                    run = back;
                    outputs = run.model().outputs.clone().into_iter().collect();
                    follow.append(&mut eff);
                }
                RunEffect::StartSpawn { node, spec } => {
                    let Some(launcher) = seams.launcher.clone() else {
                        return failed(format!("spawn {node} has no launcher on this door"), None);
                    };
                    let deadline = run.model().deadline_ms();
                    let outcome = await_spawn(&launcher, &spec, deadline).await;
                    let then = move |mut run: LinearRun| match outcome {
                        SpawnLaunch::Settled(value) => {
                            run.spawn_settled(&node, value, unix_ms()).map(|e| (run, e))
                        }
                        SpawnLaunch::Failed(error) => {
                            run.spawn_failed(&node, error, unix_ms()).map(|e| (run, e))
                        }
                        SpawnLaunch::TimedOut => run.woke(unix_ms()).map(|e| (run, e)),
                    };
                    let Some((back, mut eff)) = verb(run_id, "spawn feed", move || then(run)).await
                    else {
                        return failed("spawn feed failed", None);
                    };
                    run = back;
                    outputs = run.model().outputs.clone().into_iter().collect();
                    follow.append(&mut eff);
                }
                RunEffect::SpawnDetached { spec, .. } => {
                    if let Some(launcher) = seams.launcher.clone()
                        && let Err(e) = launcher(spec).await
                    {
                        eprintln!("durable body {run_id}: detached spawn failed: {e:#}");
                    }
                }
                RunEffect::ParkUntil { until_ms, .. } => {
                    let now = unix_ms();
                    if until_ms > now {
                        tokio::time::sleep(std::time::Duration::from_millis(until_ms - now)).await;
                    }
                    let then = move |mut run: LinearRun| run.woke(unix_ms()).map(|eff| (run, eff));
                    let Some((back, mut eff)) = verb(run_id, "wake", move || then(run)).await
                    else {
                        return failed("wake failed", None);
                    };
                    run = back;
                    follow.append(&mut eff);
                }
                RunEffect::AwaitApproval { step, .. } => {
                    return failed(
                        format!("approval node {step} has no resolution loop on this door"),
                        Some(step),
                    );
                }
                RunEffect::Complete { output } => return BodyOutcome::Completed(output),
                RunEffect::Fail { step, output } => return failure_from(&step, &output),
                RunEffect::Cancelled => return failed("run cancelled", None),
                RunEffect::Unsupported { reason } => {
                    return failed(format!("unsupported after open: {reason}"), None);
                }
            }
        }
        effects = follow;
    }
}

async fn drive_graph(
    store: &WorkflowEffectStore,
    handle: &tokio::runtime::Handle,
    run_id: &str,
    workflow: &Workflow,
    trigger: Value,
    seams: &BodySeams,
) -> BodyOutcome {
    let (engine_store, journal, state) = handles(store, handle);
    let wf = workflow.clone();
    let rid = run_id.to_string();
    let trig = trigger.clone();
    let opened = tokio::task::spawn_blocking(move || {
        let engine = OurEngine::with_journal(engine_store, journal);
        DurableGraphRun::open(engine, state, rid, wf, trig, unix_ms())
    })
    .await;
    let (mut run, mut effects) = match opened {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return failed(format!("durable graph open: {e}"), None),
        Err(e) => return failed(format!("durable graph open task: {e}"), None),
    };
    if effects.is_empty() {
        return terminal_graph(&mut run);
    }
    loop {
        if effects.is_empty() {
            return failed(
                "durable graph drive ran out of effects while non-terminal",
                None,
            );
        }
        let mut follow: Vec<GraphEffect> = Vec::new();
        for effect in std::mem::take(&mut effects) {
            match effect {
                GraphEffect::ExecuteNode {
                    node,
                    wave: _,
                    attempt,
                    inputs,
                    item,
                } => {
                    let Some(node_def) = run
                        .model()
                        .workflow
                        .graph
                        .as_ref()
                        .and_then(|g| g.nodes.get(&node))
                        .cloned()
                    else {
                        return failed(format!("unknown graph node {node}"), Some(node));
                    };
                    let snapshot: serde_json::Map<String, Value> = inputs.into_iter().collect();
                    let value = run_graph_node_step(
                        GraphNodeStep {
                            run_id,
                            workflow: &workflow.name,
                            workflow_digest: &digest(workflow),
                            node_name: &node,
                            node: &node_def,
                            attempt,
                            trigger: &trigger,
                            inputs: &snapshot,
                            item: item.as_ref(),
                        },
                        &seams.leaf,
                    )
                    .await;
                    let then = move |mut run: GraphRun| {
                        run.node_done(&node, value, unix_ms()).map(|eff| (run, eff))
                    };
                    let Some((back, mut eff)) = verb(run_id, "node feed", move || then(run)).await
                    else {
                        return failed("node feed failed", None);
                    };
                    run = back;
                    follow.append(&mut eff);
                }
                GraphEffect::ParkUntil { until_ms, .. } => {
                    let now = unix_ms();
                    if until_ms > now {
                        tokio::time::sleep(std::time::Duration::from_millis(until_ms - now)).await;
                    }
                    let then = move |mut run: GraphRun| run.woke(unix_ms()).map(|eff| (run, eff));
                    let Some((back, mut eff)) = verb(run_id, "wake", move || then(run)).await
                    else {
                        return failed("wake failed", None);
                    };
                    run = back;
                    follow.append(&mut eff);
                }
                GraphEffect::AwaitApproval { node, .. } => {
                    return failed(
                        format!("approval node {node} has no resolution loop on this door"),
                        Some(node),
                    );
                }
                GraphEffect::ChildRun {
                    node,
                    child_run_id,
                    workflow: callee,
                    trigger: child_trigger,
                } => {
                    let Some(resolve) = seams.resolve_child.clone() else {
                        return failed(
                            format!("child run {callee} has no resolver on this door"),
                            Some(node),
                        );
                    };
                    let Some(child_wf) = resolve(&callee) else {
                        let bundle = serde_json::json!({
                            "status": "failed",
                            "error": format!("call_workflow: unknown workflow {callee:?}"),
                            "failed_step": node,
                        });
                        let n = node.clone();
                        let then = move |mut run: GraphRun| {
                            run.child_failed(&n, bundle, unix_ms()).map(|e| (run, e))
                        };
                        let Some((back, mut eff)) =
                            verb(run_id, "child fail", move || then(run)).await
                        else {
                            return failed("child fail feed failed", None);
                        };
                        run = back;
                        follow.append(&mut eff);
                        continue;
                    };
                    let child =
                        Box::pin(drive(store, &child_run_id, &child_wf, child_trigger, seams))
                            .await;
                    let n = node.clone();
                    let then = move |mut run: GraphRun| match child {
                        BodyOutcome::Completed(output) => {
                            run.child_succeeded(&n, output, unix_ms()).map(|e| (run, e))
                        }
                        BodyOutcome::Failed { error, .. } => {
                            let bundle = serde_json::json!({
                                "status": "failed",
                                "error": error,
                                "failed_step": n,
                            });
                            run.child_failed(&n, bundle, unix_ms()).map(|e| (run, e))
                        }
                    };
                    let Some((back, mut eff)) = verb(run_id, "child feed", move || then(run)).await
                    else {
                        return failed("child feed failed", None);
                    };
                    run = back;
                    follow.append(&mut eff);
                }
                // The join marker follows the parent's own child-settled verb;
                // nothing to execute.
                GraphEffect::ChildJoin { .. } => {}
                GraphEffect::Complete { output } => return BodyOutcome::Completed(output),
                GraphEffect::Fail { fail } => {
                    return BodyOutcome::Failed {
                        error: fail.error,
                        step: Some(fail.node),
                    };
                }
                GraphEffect::Cancelled => return failed("run cancelled", None),
                GraphEffect::Unsupported { reason } => {
                    return failed(format!("unsupported after open: {reason}"), None);
                }
            }
        }
        effects = follow;
    }
}

/// The model's own deadline for an awaited spawn.
trait Deadline {
    fn deadline_ms(&self) -> Option<u64>;
}

impl Deadline for toron_workflow::engine::durable::RunModel {
    fn deadline_ms(&self) -> Option<u64> {
        match &self.phase {
            toron_workflow::engine::durable::RunPhase::AwaitingSpawn { deadline_ms, .. } => {
                *deadline_ms
            }
            _ => None,
        }
    }
}

enum SpawnLaunch {
    Settled(Value),
    Failed(String),
    TimedOut,
}

async fn await_spawn(
    launcher: &SpawnLauncher,
    spec: &SpawnSpec,
    deadline_ms: Option<u64>,
) -> SpawnLaunch {
    let Some(deadline) = deadline_ms else {
        return match launcher(spec.clone()).await {
            Ok(value) => SpawnLaunch::Settled(value),
            Err(e) => SpawnLaunch::Failed(format!("{e:#}")),
        };
    };
    let now = unix_ms();
    if deadline <= now {
        return SpawnLaunch::TimedOut;
    }
    match tokio::time::timeout(
        std::time::Duration::from_millis(deadline - now),
        launcher(spec.clone()),
    )
    .await
    {
        Ok(Ok(value)) => SpawnLaunch::Settled(value),
        Ok(Err(e)) => SpawnLaunch::Failed(format!("{e:#}")),
        Err(_) => SpawnLaunch::TimedOut,
    }
}

/// Re-attach to a child that is already terminal (restart reuse).
fn terminal_linear(run: &mut LinearRun) -> BodyOutcome {
    use toron_workflow::engine::durable::RunPhase;
    match &run.model().phase {
        RunPhase::Succeeded => BodyOutcome::Completed(serde_json::json!({
            "status": "succeeded",
            "steps": run.model().outputs,
        })),
        RunPhase::Failed { error } => failed(error.clone(), None),
        RunPhase::Cancelled => failed("run cancelled", None),
        _ => failed("resume produced no effects while non-terminal", None),
    }
}

/// Re-attach to a graph child that is already terminal (restart reuse).
fn terminal_graph(run: &mut GraphRun) -> BodyOutcome {
    use toron_workflow::engine::durable::GraphPhase;
    match &run.model().phase {
        GraphPhase::Succeeded => BodyOutcome::Completed(serde_json::Value::Object(
            run.model().outputs.clone().into_iter().collect(),
        )),
        GraphPhase::Failed { error } => failed(error.clone(), None),
        GraphPhase::Cancelled => failed("run cancelled", None),
        _ => failed("resume produced no effects while non-terminal", None),
    }
}

fn failure_from(step: &str, output: &Value) -> BodyOutcome {
    BodyOutcome::Failed {
        error: output
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("step failed")
            .to_string(),
        step: Some(step.to_string()),
    }
}

fn failed(error: impl Into<String>, step: Option<String>) -> BodyOutcome {
    BodyOutcome::Failed {
        error: error.into(),
        step,
    }
}

fn digest(workflow: &Workflow) -> String {
    toron_workflow::identity::workflow_digest(workflow)
}

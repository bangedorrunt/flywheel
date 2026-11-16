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
//!   instead of hanging;
//! - an `await: result` dispatch watches the job through the injected
//!   watcher and settles exactly like toron's host (claim/result/error
//!   feed their transitions, timeouts fail typed); a body with no watcher
//!   fails the WatchJob loudly instead of hanging.

use crate::phase_contracts::PhaseContractReport;
use std::sync::Arc;

use serde_json::Value;
use toron_core::index::durable_engine::ToronEngineStore;
use toron_core::index::durable_workflow::SyncWorkflowState;
use toron_core::index::workflow_effects::WorkflowEffectStore;
use toron_core::journal::{JournalDb, ToronJournal};
use toron_core::kernel::OurEngine;
use toron_workflow::engine::ProfileResolver;
use toron_workflow::engine::durable::{
    GraphEffect, JobWatchOutcome, JobWatchSpec, JobWatcher, RunEffect, await_job_watch,
    needs_graph_model,
};
use toron_workflow::engine::durable_host::{ChildSeams, DurableGraphRun, DurableRun, HostError};
use toron_workflow::engine::leaf::LeafSeams;
use toron_workflow::engine::linear_step::{
    GraphNodeStep, LinearStep, run_graph_node_step, run_linear_step,
};
use toron_workflow::engine::spawn::{SpawnLauncher, SpawnSeams, SpawnSpec};
use toron_workflow::schema::Workflow;

type EngineStore = ToronEngineStore;
type Journal = ToronJournal;
type State = SyncWorkflowState;
type LinearRun = DurableRun<EngineStore, Journal, State>;
type GraphRun = DurableGraphRun<EngineStore, Journal, State>;

/// Check one settled step output against its stage's declared witness:
/// None settles unchanged (the stage declares no witness); Some carries
/// the pass/fail report that gates success.
pub type WitnessCheck =
    Arc<dyn Fn(&str, &serde_json::Value) -> Option<PhaseContractReport> + Send + Sync>;

/// The seams one body needs.
pub struct BodySeams {
    /// Leaf execution (runner + verify sink + journal mode).
    pub leaf: LeafSeams,
    /// Watches a dispatched job's 43002/43004/43006. `None` = the body has
    /// no bus, so `await: result` fails loud and typed instead of hanging.
    pub job_watcher: Option<JobWatcher>,
    /// Gates settled step outputs against the stage's declared witness.
    /// `None` = no gate; steps settle as the fold lands them.
    pub witness: Option<WitnessCheck>,
    /// Wall clock in production, frozen in tests. Every model timestamp
    /// in this driver reads it, so deadlines stay deterministic.
    pub clock: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Starts a spawn child. `None` = the body refuses a spawn shape.
    pub launcher: Option<SpawnLauncher>,
    /// Profile name -> resolved profile for a `spawn` step. `None` leaves the
    /// step's old unknown-profile value in place (never a run failure).
    pub spawn_resolver: Option<ProfileResolver>,
    /// Resolves a `call_workflow` callee by name. `None` = refuse the shape.
    pub resolve_child: Option<WorkflowLookup>,
}

/// How one body ended.
#[derive(Debug)]
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
        // bd-jhz8x.2: a verify step's data must be consumable downstream
        // (`foreach over: "{{steps.gate.json.ready}}"`). `None` when the
        // command printed no JSON — never an empty object.
        let json = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .ok()
            .filter(|v| !v.is_null());
        Ok(VerifyResult {
            ok: out.status.success(),
            exit: out.status.code().unwrap_or(-1),
            stdout_path: None,
            stderr_path: None,
            error: None,
            json,
        })
    });
    *sink.0.lock().expect("verify sink lock poisoned") = Some(f);
    sink
}

/// The stage the settled step dispatches under, when the step carries one.
/// Spawn steps carry none, so the gate correctly skips them.
fn settle_stage(run: &LinearRun) -> Option<String> {
    let model = run.model();
    let (_, step) = model.plan.step_at(model.step_index)?;
    match &step.action {
        toron_workflow::schema::Action::DispatchJob { spec, .. } => spec.stage.clone(),
        _ => None,
    }
}

/// Run the stage witness gate on one settled step output. None settles
/// unchanged: no seam, no stage on the step, or no declared witness. A
/// failing report returns the typed refusal that replaces success.
fn witness_refusal(
    seam: Option<&WitnessCheck>,
    node: &str,
    stage: Option<&str>,
    value: &Value,
) -> Option<String> {
    let check = seam?;
    let stage = stage?;
    let report = check(stage, value)?;
    if report.ok {
        return None;
    }
    Some(format!(
        "step {node} (stage {stage}) failed witness `{}`: {}",
        report.phase, report.evidence
    ))
}

pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn handles(
    store: &WorkflowEffectStore,
    handle: &tokio::runtime::Handle,
    journal_scope: &str,
) -> (EngineStore, Journal, State) {
    let pool = store.pool().clone();
    (
        ToronEngineStore::spawn(pool.clone(), handle),
        ToronJournal::spawn(JournalDb::new(pool.clone(), journal_scope, 64), handle),
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
    drive_in_scope(store, run_id, workflow, trigger, seams, "body").await
}

/// [`drive`], in the journal scope that STARTED the run.
///
/// The engine key is one run: a host that starts it (cron's `ElmCron`) and a
/// body that drives it must share the journal scope, or the body settles a
/// second row and the starter's row stays `in_flight` forever.
pub async fn drive_in_scope(
    store: &WorkflowEffectStore,
    run_id: &str,
    workflow: &Workflow,
    trigger: Value,
    seams: &BodySeams,
    journal_scope: &str,
) -> BodyOutcome {
    let handle = tokio::runtime::Handle::current();
    let graph = needs_graph_model(workflow);
    if graph {
        drive_graph(
            store,
            &handle,
            run_id,
            workflow,
            trigger,
            seams,
            journal_scope,
        )
        .await
    } else {
        drive_linear(
            store,
            &handle,
            run_id,
            workflow,
            trigger,
            seams,
            journal_scope,
        )
        .await
    }
}

async fn drive_linear(
    store: &WorkflowEffectStore,
    handle: &tokio::runtime::Handle,
    run_id: &str,
    workflow: &Workflow,
    trigger: Value,
    seams: &BodySeams,
    journal_scope: &str,
) -> BodyOutcome {
    let (engine_store, journal, state) = handles(store, handle, journal_scope);
    let clock = seams.clock.clone();
    let opened_at = clock();
    let wf = workflow.clone();
    let rid = run_id.to_string();
    let trig = trigger.clone();
    let spawn_seams = SpawnSeams {
        resolver: seams.spawn_resolver.clone(),
        launcher: seams.launcher.clone(),
    };
    let opened = tokio::task::spawn_blocking(move || {
        let engine = OurEngine::with_journal(engine_store, journal);
        DurableRun::open_with_seams(engine, state, rid, wf, trig, opened_at, &spawn_seams)
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
                    let c = clock.clone();
                    let then =
                        move |mut run: LinearRun| run.step_done(value, c()).map(|eff| (run, eff));
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
                    let outcome = await_spawn(&launcher, &spec, deadline, clock()).await;
                    let stage = settle_stage(&run);
                    let outcome = match outcome {
                        SpawnLaunch::Settled(value) => {
                            match witness_refusal(
                                seams.witness.as_ref(),
                                &node,
                                stage.as_deref(),
                                &value,
                            ) {
                                Some(error) => SpawnLaunch::Failed(error),
                                None => SpawnLaunch::Settled(value),
                            }
                        }
                        other => other,
                    };
                    let c = clock.clone();
                    let then = move |mut run: LinearRun| match outcome {
                        SpawnLaunch::Settled(value) => {
                            run.spawn_settled(&node, value, c()).map(|e| (run, e))
                        }
                        SpawnLaunch::Failed(error) => {
                            run.spawn_failed(&node, error, c()).map(|e| (run, e))
                        }
                        SpawnLaunch::TimedOut => run.woke(c()).map(|e| (run, e)),
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
                RunEffect::WatchJob { node, job_id } => {
                    let Some(watcher) = seams.job_watcher.clone() else {
                        return failed(
                            format!(
                                "dispatch_job await: result needs a toron host; \
                                 flywheel in-process bodies cannot watch job {job_id} \
                                 for step {node}"
                            ),
                            Some(node),
                        );
                    };
                    let (deadline, claimed) = match run.model().phase {
                        toron_workflow::engine::durable::RunPhase::AwaitingJob {
                            claim_deadline_ms,
                            result_deadline_ms,
                            claimed,
                            ..
                        } => {
                            if claimed {
                                (result_deadline_ms, true)
                            } else {
                                (claim_deadline_ms, false)
                            }
                        }
                        _ => (None, false),
                    };
                    let outcome = await_job_watch(
                        &watcher,
                        JobWatchSpec {
                            job_id: job_id.clone(),
                            deadline_ms: deadline,
                            claimed,
                        },
                        deadline,
                        clock(),
                    )
                    .await;
                    let stage = settle_stage(&run);
                    let outcome = match outcome {
                        JobWatchOutcome::Settled(value) => {
                            match witness_refusal(
                                seams.witness.as_ref(),
                                &node,
                                stage.as_deref(),
                                &value,
                            ) {
                                Some(error) => JobWatchOutcome::Failed(error),
                                None => JobWatchOutcome::Settled(value),
                            }
                        }
                        other => other,
                    };
                    let c = clock.clone();
                    let then = move |mut run: LinearRun| match outcome {
                        JobWatchOutcome::Claimed => {
                            run.job_claimed(&node, c()).map(|eff| (run, eff))
                        }
                        JobWatchOutcome::Settled(value) => {
                            run.job_settled(&node, value, c()).map(|eff| (run, eff))
                        }
                        JobWatchOutcome::Failed(error) => {
                            run.job_failed(&node, error, c()).map(|eff| (run, eff))
                        }
                        JobWatchOutcome::TimedOut => {
                            let now = c().max(deadline.unwrap_or(0));
                            run.woke(now).map(|eff| (run, eff))
                        }
                    };
                    let Some((back, mut eff)) = verb(run_id, "job watch", move || then(run)).await
                    else {
                        return failed("job watch failed", None);
                    };
                    run = back;
                    outputs = run.model().outputs.clone().into_iter().collect();
                    follow.append(&mut eff);
                }
                RunEffect::ParkUntil { until_ms, .. } => {
                    // A spawn emits StartSpawn and its deadline park together.
                    // The launcher has already settled that park, so honoring
                    // the stale deadline would sleep until a wake that is no
                    // longer the run's phase.
                    if !matches!(
                        &run.model().phase,
                        toron_workflow::engine::durable::RunPhase::Parked { .. }
                    ) {
                        continue;
                    }
                    let now = clock();
                    if until_ms > now {
                        tokio::time::sleep(std::time::Duration::from_millis(until_ms - now)).await;
                    }
                    let c = clock.clone();
                    let then = move |mut run: LinearRun| run.woke(c()).map(|eff| (run, eff));
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
    journal_scope: &str,
) -> BodyOutcome {
    let (engine_store, journal, state) = handles(store, handle, journal_scope);
    let clock = seams.clock.clone();
    let opened_at = clock();
    let wf = workflow.clone();
    let rid = run_id.to_string();
    let trig = trigger.clone();
    let children = ChildSeams {
        resolver: seams.resolve_child.clone(),
    };
    let opened = tokio::task::spawn_blocking(move || {
        let engine = OurEngine::with_journal(engine_store, journal);
        DurableGraphRun::open_with_children(engine, state, rid, wf, trig, opened_at, &children)
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
                    let c = clock.clone();
                    let then = move |mut run: GraphRun| {
                        run.node_done(&node, value, c()).map(|eff| (run, eff))
                    };
                    let Some((back, mut eff)) = verb(run_id, "node feed", move || then(run)).await
                    else {
                        return failed("node feed failed", None);
                    };
                    run = back;
                    follow.append(&mut eff);
                }
                GraphEffect::ParkUntil { until_ms, .. } => {
                    let now = clock();
                    if until_ms > now {
                        tokio::time::sleep(std::time::Duration::from_millis(until_ms - now)).await;
                    }
                    let c = clock.clone();
                    let then = move |mut run: GraphRun| run.woke(c()).map(|eff| (run, eff));
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
                        let c = clock.clone();
                        let then = move |mut run: GraphRun| {
                            run.child_failed(&n, bundle, c()).map(|e| (run, e))
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
                    let c = clock.clone();
                    let then = move |mut run: GraphRun| match child {
                        BodyOutcome::Completed(output) => {
                            run.child_succeeded(&n, output, c()).map(|e| (run, e))
                        }
                        BodyOutcome::Failed { error, .. } => {
                            let bundle = serde_json::json!({
                                "status": "failed",
                                "error": error,
                                "failed_step": n,
                            });
                            run.child_failed(&n, bundle, c()).map(|e| (run, e))
                        }
                    };
                    let Some((back, mut eff)) = verb(run_id, "child feed", move || then(run)).await
                    else {
                        return failed("child feed failed", None);
                    };
                    run = back;
                    follow.append(&mut eff);
                }
                // Informational only: feeding the `ChildRun` result back
                // (`child_succeeded` / `child_failed`) already settled the
                // node in the fold. A restart re-emits `ChildRun` for a child
                // that never settled, so there is nothing left to do here.
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
    now_ms: u64,
) -> SpawnLaunch {
    let Some(deadline) = deadline_ms else {
        return match launcher(spec.clone()).await {
            Ok(value) => SpawnLaunch::Settled(value),
            Err(e) => SpawnLaunch::Failed(format!("{e:#}")),
        };
    };
    if deadline <= now_ms {
        return SpawnLaunch::TimedOut;
    }
    match tokio::time::timeout(
        std::time::Duration::from_millis(deadline - now_ms),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// bd-36cbt: a timed-out in-flight run moves InFlight -> Unknown ->
    /// ReconcilePending -> Settled through expire_lease + reconcile over
    /// ToronEngineStore + ToronJournal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn expire_lease_and_reconcile_settle_timed_out_run() {
        use toron_core::index::durable_engine::ToronEngineStore;
        use toron_core::index::schema;
        use toron_core::journal::{JournalDb, ToronJournal};
        use toron_core::kernel::{DurableEngine, EffectState, OurEngine, RunPhase, StartOutcome};

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let db = std::env::temp_dir().join(format!(
            "fw-durable-lease-{}-{}.sqlite",
            std::process::id(),
            nanos
        ));
        let pool = schema::connect(db.to_str().unwrap())
            .await
            .expect("connect");
        schema::migrate(&pool).await.expect("migrate");
        let handle = tokio::runtime::Handle::current();
        let mut engine = OurEngine::with_journal(
            ToronEngineStore::spawn(pool.clone(), &handle),
            ToronJournal::spawn(JournalDb::new(pool, "body-test", 64), &handle),
        );
        let key = "bd-36cbt-lease-expiry";
        match engine.start(key, 0).expect("start") {
            StartOutcome::Started { .. } => {}
            StartOutcome::Already { status } => {
                panic!("fresh key already started: {status:?}")
            }
        }
        assert_eq!(
            engine.query(key).expect("query running").phase,
            RunPhase::Running,
            "fresh start is Running (InFlight)"
        );
        engine.park_until(key, 1_000, 60, 0, None).expect("park");
        let expired = engine
            .expire_lease(key, 0)
            .expect("expire")
            .expect("timed-out lease must transition to Unknown");
        assert_eq!(
            expired.state,
            EffectState::Unknown,
            "expire settles the journal to Unknown"
        );
        assert_eq!(
            engine.query(key).expect("query unknown").phase,
            RunPhase::Unknown,
            "storage projects Unknown after expire"
        );
        let settled = engine
            .reconcile(key, expired.generation, EffectState::Settled)
            .expect("reconcile")
            .expect("reconcile must return a row after expire");
        assert_eq!(settled.state, EffectState::Settled, "reconciled to Settled");
        assert_eq!(
            engine.query(key).expect("query settled").phase,
            RunPhase::Settled,
            "run settled after reconcile"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// bd-flywheel-host-run-ofcw witness: a spawn step launches through the
    /// injected launcher exactly once, settles its park, and a second drive
    /// with the same run id re-attaches to the persisted model instead of
    /// launching a second child.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_step_launches_once_through_the_launcher_seam() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use toron_core::index::schema;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let db = std::env::temp_dir().join(format!(
            "fw-durable-spawn-{}-{nanos}.sqlite",
            std::process::id()
        ));
        let pool = schema::connect(db.to_str().unwrap())
            .await
            .expect("connect");
        schema::migrate(&pool).await.expect("migrate");
        let store = toron_core::index::workflow_effects::WorkflowEffectStore::new(pool);

        let wf = toron_workflow::schema::parse_workflow(
            "name: spawn-body\nsteps:\n  - name: launch\n    action: spawn\n    profile: \"@housekeeper\"\n    workflow: child\n    await: true\n    no_claim_timeout: 600\n",
        )
        .expect("parse spawn workflow");

        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let launcher: SpawnLauncher = std::sync::Arc::new(move |spec| {
            seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                assert!(spec.await_, "the spawn parks");
                assert_eq!(spec.workflow_ref.as_deref(), Some("child"));
                Ok(serde_json::json!({ "ok": true, "spawn_key": spec.spawn_key }))
            })
        });
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: None,
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: Some(launcher),
            job_watcher: None,
            witness: None,
            clock: Arc::new(unix_ms),
            spawn_resolver: Some(std::sync::Arc::new(|profile: &str| {
                (profile == "@housekeeper").then(|| {
                    serde_json::json!({
                        "npub": "",
                        "spec": { "harness": "pi" }
                    })
                })
            })),
            resolve_child: None,
        };

        let first = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(&store, "spawn-body-1", &wf, serde_json::json!({}), &seams),
        )
        .await
        .expect("a settled spawn must not sleep out its deadline park");
        assert!(
            matches!(first, BodyOutcome::Completed(_)),
            "a spawn body completes after the launcher settles it"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one launch");

        let second = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(&store, "spawn-body-1", &wf, serde_json::json!({}), &seams),
        )
        .await
        .expect("re-drive must re-attach without sleeping out a stale park");
        assert!(
            matches!(second, BodyOutcome::Completed(_)),
            "re-drive re-attaches to the terminal model"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a settled spawn is never relaunched"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// Without a watcher the WatchJob arm fails loud and typed instead of
    /// parking past a wake that cannot come.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_body_watch_job_fails_typed() {
        use toron_core::index::schema;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let db = std::env::temp_dir().join(format!(
            "fw-durable-watch-{nanos}-{}.sqlite",
            std::process::id()
        ));
        let pool = schema::connect(db.to_str().unwrap())
            .await
            .expect("connect");
        schema::migrate(&pool).await.expect("migrate");
        let store = toron_core::index::workflow_effects::WorkflowEffectStore::new(pool);

        let wf = toron_workflow::schema::parse_workflow(
            "name: watch-body\nsteps:\n  - name: j\n    action: dispatch_job\n    assignee: x\n    spec: { task: hi }\n    await: result\n    result_timeout: 60\n",
        )
        .expect("parse dispatch await");

        // The leaf reports the 43001 id, so the fold emits the WatchJob.
        let runner: toron_workflow::engine::AsyncEffectRunner =
            std::sync::Arc::new(|_inv: &toron_workflow::engine::ActionInvocation| {
                Box::pin(async move {
                    Ok(serde_json::json!({
                        "ok": true,
                        "job_id": "job-1",
                        "published": true
                    }))
                })
                    as std::pin::Pin<
                        Box<
                            dyn std::future::Future<Output = anyhow::Result<serde_json::Value>>
                                + Send,
                        >,
                    >
            });
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: Some(runner),
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: None,
            witness: None,
            clock: Arc::new(unix_ms),
            spawn_resolver: None,
            resolve_child: None,
        };

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(&store, "watch-body-1", &wf, serde_json::json!({}), &seams),
        )
        .await
        .expect("a watch refusal must fail fast, never hang");
        let BodyOutcome::Failed { error, step } = outcome else {
            panic!("an in-process body cannot watch jobs: {outcome:?}");
        };
        assert!(
            error.contains("cannot watch"),
            "typed refusal names the missing watcher: {error}"
        );
        assert_eq!(step.as_deref(), Some("j"));
        let _ = std::fs::remove_file(&db);
    }

    /// Scripted watcher outcomes. An empty queue returns TimedOut, the same
    /// contract the real watcher upholds.
    fn queue_watcher(
        outcomes: Vec<JobWatchOutcome>,
    ) -> (
        JobWatcher,
        Arc<std::sync::Mutex<std::collections::VecDeque<JobWatchOutcome>>>,
    ) {
        let queue = Arc::new(std::sync::Mutex::new(
            outcomes
                .into_iter()
                .collect::<std::collections::VecDeque<_>>(),
        ));
        let q = queue.clone();
        let watcher: JobWatcher = Arc::new(move |_spec: JobWatchSpec| {
            let q = q.clone();
            Box::pin(async move {
                q.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pop_front()
                    .unwrap_or(JobWatchOutcome::TimedOut)
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = JobWatchOutcome> + Send>>
        });
        (watcher, queue)
    }

    fn frozen_clock() -> Arc<dyn Fn() -> u64 + Send + Sync> {
        Arc::new(|| 1_000_000u64)
    }

    /// Witness seam over the real contracts: only stage `s` declares one
    /// (the `resolve` contract); anything else settles unchecked.
    fn resolve_witness() -> WitnessCheck {
        Arc::new(|stage: &str, output: &Value| match stage {
            "s" => Some(crate::phase_contracts::check_witness("resolve", output)),
            _ => None,
        })
    }

    fn receipt_runner() -> toron_workflow::engine::AsyncEffectRunner {
        Arc::new(|_inv: &toron_workflow::engine::ActionInvocation| {
            Box::pin(async move {
                Ok(serde_json::json!({
                    "ok": true,
                    "job_id": "job-1",
                    "published": true
                }))
            })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = anyhow::Result<serde_json::Value>> + Send>,
                >
        })
    }

    async fn watch_test_store(
        tag: &str,
    ) -> (
        toron_core::index::workflow_effects::WorkflowEffectStore,
        std::path::PathBuf,
    ) {
        use toron_core::index::schema;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let db = std::env::temp_dir().join(format!(
            "fw-durable-watch-{tag}-{nanos}-{}.sqlite",
            std::process::id()
        ));
        let pool = schema::connect(db.to_str().unwrap())
            .await
            .expect("connect");
        schema::migrate(&pool).await.expect("migrate");
        (
            toron_core::index::workflow_effects::WorkflowEffectStore::new(pool),
            db,
        )
    }

    /// Claim then settle: the step output equals the result and the run
    /// continues to the next step.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_body_await_result_settles_from_watcher() {
        let (store, db) = watch_test_store("settle").await;
        let wf = toron_workflow::schema::parse_workflow(
            "name: watch-settle\nsteps:\n  - name: j\n    action: dispatch_job\n    assignee: x\n    spec: { task: hi }\n    await: result\n    result_timeout: 60\n  - name: k\n    action: send_dm\n    to: x\n    body: hi\n",
        )
        .expect("parse dispatch await");
        let (watcher, _queue) = queue_watcher(vec![
            JobWatchOutcome::Claimed,
            JobWatchOutcome::Settled(serde_json::json!({
                "ok": true,
                "job": "job-1",
                "witness": {"contract": "signed"},
                "body": "did it",
            })),
        ]);
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: Some(receipt_runner()),
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: Some(watcher),
            witness: None,
            clock: frozen_clock(),
            spawn_resolver: None,
            resolve_child: None,
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(&store, "watch-settle-1", &wf, serde_json::json!({}), &seams),
        )
        .await
        .expect("a settling watch must complete fast");
        let BodyOutcome::Completed(output) = outcome else {
            panic!("a settled job completes the run: {outcome:?}");
        };
        assert_eq!(
            output["steps"]["j"]["body"],
            serde_json::json!("did it"),
            "step output equals the watched result: {output}"
        );
        assert!(
            output["steps"]["j"]["witness"].is_object(),
            "witness lifted into the step output: {output}"
        );
        assert!(
            output["steps"].get("k").is_some(),
            "the run continues to the next step: {output}"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// A watched 43006 lands through `on_error`: with FailRun the run fails
    /// naming the step.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_body_await_result_error_goes_to_on_error() {
        let (store, db) = watch_test_store("error").await;
        let wf = toron_workflow::schema::parse_workflow(
            "name: watch-error\nsteps:\n  - name: j\n    action: dispatch_job\n    assignee: x\n    spec: { task: hi }\n    await: result\n    result_timeout: 60\n    on_error: fail_run\n",
        )
        .expect("parse dispatch await");
        let (watcher, _queue) =
            queue_watcher(vec![JobWatchOutcome::Failed("agent blew up".to_string())]);
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: Some(receipt_runner()),
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: Some(watcher),
            witness: None,
            clock: frozen_clock(),
            spawn_resolver: None,
            resolve_child: None,
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(&store, "watch-error-1", &wf, serde_json::json!({}), &seams),
        )
        .await
        .expect("a failing watch must fail fast");
        let BodyOutcome::Failed { error, step } = outcome else {
            panic!("a job error must fail the run: {outcome:?}");
        };
        assert!(
            error.contains("agent blew up"),
            "typed failure carries the job message: {error}"
        );
        assert_eq!(step.as_deref(), Some("j"));
        let _ = std::fs::remove_file(&db);
    }

    /// A watch that resolves nothing fails typed at the claim deadline. The
    /// fake only times out on the frozen clock's deadline (1_001_000); any
    /// other deadline settles, so ignoring the seam clock turns this into a
    /// success the test rejects. The 2s guard proves milliseconds, not sleep.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_body_await_result_timeout_is_typed() {
        let (store, db) = watch_test_store("timeout").await;
        let wf = toron_workflow::schema::parse_workflow(
            "name: watch-timeout\nsteps:\n  - name: j\n    action: dispatch_job\n    assignee: x\n    spec: { task: hi }\n    await: result\n    no_claim_timeout: 1\n    result_timeout: 60\n    on_error: fail_run\n",
        )
        .expect("parse dispatch await");
        let watcher: JobWatcher = Arc::new(|spec: JobWatchSpec| {
            Box::pin(async move {
                if spec.deadline_ms == Some(1_001_000) {
                    JobWatchOutcome::TimedOut
                } else {
                    JobWatchOutcome::Settled(serde_json::json!({"ok": true, "job": "job-1"}))
                }
            })
                as std::pin::Pin<Box<dyn std::future::Future<Output = JobWatchOutcome> + Send>>
        });
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: Some(receipt_runner()),
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: Some(watcher),
            witness: None,
            clock: frozen_clock(),
            spawn_resolver: None,
            resolve_child: None,
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            drive(
                &store,
                "watch-timeout-1",
                &wf,
                serde_json::json!({}),
                &seams,
            ),
        )
        .await
        .expect("a timing-out watch must fail fast");
        let BodyOutcome::Failed { error, step } = outcome else {
            panic!("a timed-out watch must never succeed: {outcome:?}");
        };
        assert!(
            error.contains('j') && error.contains("no claim"),
            "typed timeout names the step and the bound: {error}"
        );
        assert_eq!(step.as_deref(), Some("j"));
        let _ = std::fs::remove_file(&db);
    }

    /// A satisfied witness settles unchanged: the checked output lands as
    /// the step value and the run continues.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_body_witness_pass_settles() {
        let (store, db) = watch_test_store("witness-pass").await;
        let wf = toron_workflow::schema::parse_workflow(
            "name: witness-pass\nsteps:\n  - name: j\n    action: dispatch_job\n    assignee: x\n    spec: { task: hi, stage: s }\n    await: result\n    result_timeout: 60\n  - name: k\n    action: send_dm\n    to: x\n    body: hi\n",
        )
        .expect("parse dispatch await");
        let (watcher, _queue) = queue_watcher(vec![
            JobWatchOutcome::Claimed,
            JobWatchOutcome::Settled(serde_json::json!({
                "ok": true,
                "job": "job-1",
                "target": "fix the flake",
                "non_goals": ["no rewrite"],
                "blast_radius": "flywheel loop",
                "open_questions": [],
                "body": "did it",
            })),
        ]);
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: Some(receipt_runner()),
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: Some(watcher),
            witness: Some(resolve_witness()),
            clock: frozen_clock(),
            spawn_resolver: None,
            resolve_child: None,
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(&store, "witness-pass-1", &wf, serde_json::json!({}), &seams),
        )
        .await
        .expect("a witnessed settle must complete fast");
        let BodyOutcome::Completed(output) = outcome else {
            panic!("a passing witness settles: {outcome:?}");
        };
        assert_eq!(
            output["steps"]["j"]["body"],
            serde_json::json!("did it"),
            "checked output lands unchanged: {output}"
        );
        assert!(
            output["steps"].get("k").is_some(),
            "the run continues: {output}"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// A violated witness fails typed through `on_error`, naming the step,
    /// the stage, the witness and the report detail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_body_witness_fail_goes_to_on_error() {
        let (store, db) = watch_test_store("witness-fail").await;
        let wf = toron_workflow::schema::parse_workflow(
            "name: witness-fail\nsteps:\n  - name: j\n    action: dispatch_job\n    assignee: x\n    spec: { task: hi, stage: s }\n    await: result\n    result_timeout: 60\n    on_error: fail_run\n",
        )
        .expect("parse dispatch await");
        let (watcher, _queue) = queue_watcher(vec![
            JobWatchOutcome::Claimed,
            JobWatchOutcome::Settled(serde_json::json!({
                "ok": true,
                "job": "job-1",
                "body": "did it",
            })),
        ]);
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: Some(receipt_runner()),
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: Some(watcher),
            witness: Some(resolve_witness()),
            clock: frozen_clock(),
            spawn_resolver: None,
            resolve_child: None,
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(&store, "witness-fail-1", &wf, serde_json::json!({}), &seams),
        )
        .await
        .expect("a failing witness must fail fast");
        let BodyOutcome::Failed { error, step } = outcome else {
            panic!("a violated witness never settles: {outcome:?}");
        };
        for part in ["j", "s", "resolve", "missing"] {
            assert!(
                error.contains(part),
                "refusal names step, stage, witness and detail ({part}): {error}"
            );
        }
        assert_eq!(step.as_deref(), Some("j"));
        let _ = std::fs::remove_file(&db);
    }

    /// No declared witness, or no stage at all, settles exactly as the
    /// fold lands it — the gate only runs where a witness is declared.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_body_witness_absent_settles() {
        let (store, db) = watch_test_store("witness-absent").await;
        let wf = toron_workflow::schema::parse_workflow(
            "name: witness-absent\nsteps:\n  - name: j\n    action: dispatch_job\n    assignee: x\n    spec: { task: hi, stage: plain }\n    await: result\n    result_timeout: 60\n  - name: k\n    action: dispatch_job\n    assignee: x\n    spec: { task: hi }\n    await: result\n    result_timeout: 60\n",
        )
        .expect("parse dispatch await");
        let settled = || {
            JobWatchOutcome::Settled(serde_json::json!({
                "ok": true,
                "job": "job-1",
                "body": "did it",
            }))
        };
        let (watcher, _queue) = queue_watcher(vec![
            JobWatchOutcome::Claimed,
            settled(),
            JobWatchOutcome::Claimed,
            settled(),
        ]);
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: Some(receipt_runner()),
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: Some(watcher),
            witness: Some(resolve_witness()),
            clock: frozen_clock(),
            spawn_resolver: None,
            resolve_child: None,
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(
                &store,
                "witness-absent-1",
                &wf,
                serde_json::json!({}),
                &seams,
            ),
        )
        .await
        .expect("an unwitnessed settle must complete fast");
        let BodyOutcome::Completed(output) = outcome else {
            panic!("no declared witness settles: {outcome:?}");
        };
        assert_eq!(
            output["steps"]["j"]["body"],
            serde_json::json!("did it"),
            "witness-less stage settles ungated: {output}"
        );
        assert_eq!(
            output["steps"]["k"]["body"],
            serde_json::json!("did it"),
            "stageless step settles ungated: {output}"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// A non-JSON output for a witnessed stage fails: a missing report is
    /// never a pass.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_body_witness_missing_report_fails() {
        let (store, db) = watch_test_store("witness-missing").await;
        let wf = toron_workflow::schema::parse_workflow(
            "name: witness-missing\nsteps:\n  - name: j\n    action: dispatch_job\n    assignee: x\n    spec: { task: hi, stage: s }\n    await: result\n    result_timeout: 60\n    on_error: fail_run\n",
        )
        .expect("parse dispatch await");
        let (watcher, _queue) = queue_watcher(vec![
            JobWatchOutcome::Claimed,
            JobWatchOutcome::Settled(serde_json::json!("just a string")),
        ]);
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: Some(receipt_runner()),
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: Some(watcher),
            witness: Some(resolve_witness()),
            clock: frozen_clock(),
            spawn_resolver: None,
            resolve_child: None,
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(
                &store,
                "witness-missing-1",
                &wf,
                serde_json::json!({}),
                &seams,
            ),
        )
        .await
        .expect("a missing report must fail fast");
        let BodyOutcome::Failed { error, step } = outcome else {
            panic!("a missing report never passes: {outcome:?}");
        };
        assert!(
            error.contains("no machine-readable report"),
            "refusal carries the report detail: {error}"
        );
        assert_eq!(step.as_deref(), Some("j"));
        let _ = std::fs::remove_file(&db);
    }

    fn wave_swarm_workflow() -> toron_workflow::schema::Workflow {
        crate::builtin_workflows::builtin_workflows()["wave-swarm"].clone()
    }

    fn wave_swarm_trigger() -> serde_json::Value {
        serde_json::json!({"inputs": {
            "objective": "prove the wave ends on witnesses",
            "wave_label": "wave-1",
            "steer_channel": "adr0006-steer",
            "captain_pin": "",
            "coder_profile": "@coder",
        }})
    }

    fn wave_swarm_seams(watcher: JobWatcher) -> BodySeams {
        BodySeams {
            leaf: LeafSeams {
                async_runner: Some(receipt_runner()),
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: Some(watcher),
            // Compiled-in seed catalog, not the operator's user tier: the
            // test pins the shipped witnesses, not ambient config.
            witness: Some(crate::stages::StageLibrary::compiled_in().witness_checker()),
            clock: frozen_clock(),
            spawn_resolver: None,
            resolve_child: None,
        }
    }

    /// ADR-0021 acceptance #3: a coder that never writes a witness fails
    /// at result_timeout with a typed error naming the first stage step.
    /// The watcher never settles; the frozen clock proves milliseconds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wave_swarm_missing_witness_fails_at_result_timeout() {
        let (store, db) = watch_test_store("wave-timeout").await;
        let wf = wave_swarm_workflow();
        let (watcher, _queue) = queue_watcher(vec![]);
        let seams = wave_swarm_seams(watcher);
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(&store, "wave-timeout-1", &wf, wave_swarm_trigger(), &seams),
        )
        .await
        .expect("an unsettled wave must fail fast");
        let BodyOutcome::Failed { error, step } = outcome else {
            panic!("a witness-less wave never succeeds: {outcome:?}");
        };
        assert!(
            error.contains("gate") && error.contains("no claim"),
            "typed timeout names the first stage step: {error}"
        );
        assert_eq!(step.as_deref(), Some("gate"));
        let _ = std::fs::remove_file(&db);
    }

    /// A held gate settles a failing witness, and the run fails naming
    /// the gate step — held is a typed failure, never silent success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wave_swarm_failing_witness_goes_to_on_error() {
        let (store, db) = watch_test_store("wave-held").await;
        let wf = wave_swarm_workflow();
        let (watcher, _queue) = queue_watcher(vec![
            JobWatchOutcome::Claimed,
            JobWatchOutcome::Settled(serde_json::json!({
                "ok": true,
                "job": "job-1",
                "witness": {"held": true, "blockers": ["wave-0 still open"]},
                "body": "[gate] held",
            })),
        ]);
        let seams = wave_swarm_seams(watcher);
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(&store, "wave-held-1", &wf, wave_swarm_trigger(), &seams),
        )
        .await
        .expect("a held gate must fail fast");
        let BodyOutcome::Failed { error, step } = outcome else {
            panic!("a held gate never succeeds: {outcome:?}");
        };
        for part in ["gate", "wave-gate", "plan"] {
            assert!(
                error.contains(part),
                "refusal names step, stage and witness ({part}): {error}"
            );
        }
        assert_eq!(step.as_deref(), Some("gate"));
        let _ = std::fs::remove_file(&db);
    }

    /// A broken stage catalog fails closed: every staged step is refused
    /// with the load error instead of settling ungated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_body_witness_catalog_error_fails_closed() {
        let (store, db) = watch_test_store("witness-catalog").await;
        let wf = toron_workflow::schema::parse_workflow(
            "name: witness-catalog\nsteps:\n  - name: j\n    action: dispatch_job\n    assignee: x\n    spec: { task: hi, stage: s }\n    await: result\n    result_timeout: 60\n    on_error: fail_run\n",
        )
        .expect("parse dispatch await");
        let (watcher, _queue) = queue_watcher(vec![
            JobWatchOutcome::Claimed,
            JobWatchOutcome::Settled(serde_json::json!({
                "ok": true,
                "job": "job-1",
                "body": "did it",
            })),
        ]);
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: Some(receipt_runner()),
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: Some(watcher),
            witness: Some(crate::stages::StageLibrary::witness_checker_unavailable(
                "bad user stages.toml".to_string(),
            )),
            clock: frozen_clock(),
            spawn_resolver: None,
            resolve_child: None,
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(
                &store,
                "witness-catalog-1",
                &wf,
                serde_json::json!({}),
                &seams,
            ),
        )
        .await
        .expect("a catalog failure must fail fast");
        let BodyOutcome::Failed { error, step } = outcome else {
            panic!("a broken catalog never settles staged steps: {outcome:?}");
        };
        for part in ["j", "s", "unavailable", "bad user stages.toml"] {
            assert!(
                error.contains(part),
                "refusal names step, stage and the load error ({part}): {error}"
            );
        }
        assert_eq!(step.as_deref(), Some("j"));
        let _ = std::fs::remove_file(&db);
    }

    /// Spawn steps carry no stage, so the gated StartSpawn arm consults
    /// the gate and settles: the fail-everything spy is never called.
    /// (A failing spawn witness needs `stage` on the spawn path, which
    /// neither `Action::Spawn` nor `SpawnSpec` carries today.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn durable_body_witness_spawn_unstaged_settles() {
        let (store, db) = watch_test_store("witness-spawn").await;
        let wf = toron_workflow::schema::parse_workflow(
            "name: witness-spawn\nsteps:\n  - name: j\n    action: spawn\n    profile: \"@housekeeper\"\n    workflow: child\n    await: true\n    no_claim_timeout: 600\n",
        )
        .expect("parse spawn await");
        let launcher: toron_workflow::engine::spawn::SpawnLauncher = std::sync::Arc::new(|spec| {
            Box::pin(
                async move { Ok(serde_json::json!({ "ok": true, "spawn_key": spec.spawn_key })) },
            )
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = anyhow::Result<serde_json::Value>> + Send>,
                >
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let witness: WitnessCheck = Arc::new(move |_stage: &str, _output: &Value| {
            seen.fetch_add(1, Ordering::SeqCst);
            Some(crate::phase_contracts::PhaseContractReport {
                phase: "witness",
                ok: false,
                evidence: "spy must never fire for a stageless step".to_string(),
            })
        });
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: None,
                sync_runner: None,
                verify_sink: toron_workflow::engine::VerifySink::default(),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: Some(launcher),
            job_watcher: None,
            witness: Some(witness),
            clock: frozen_clock(),
            spawn_resolver: Some(std::sync::Arc::new(|profile: &str| {
                (profile == "@housekeeper").then(|| {
                    serde_json::json!({
                        "npub": "",
                        "spec": { "harness": "pi" }
                    })
                })
            })),
            resolve_child: None,
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(
                &store,
                "witness-spawn-1",
                &wf,
                serde_json::json!({}),
                &seams,
            ),
        )
        .await
        .expect("an unstaged spawn must settle fast");
        let BodyOutcome::Completed(output) = outcome else {
            panic!("a stageless spawn settles: {outcome:?}");
        };
        assert!(
            output["steps"]["j"]["ok"].as_bool().unwrap_or(false),
            "spawn settled: {output}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the gate never fires without a stage"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// bd-5cnw7 witness: a graph body with a static `call_workflow` opens only
    /// when `BodySeams::resolve_child` reaches the graph's child seams, then
    /// drives the child to a terminal value and joins it into the parent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graph_child_call_resolves_through_the_child_seam() {
        use toron_core::index::schema;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let db = std::env::temp_dir().join(format!(
            "fw-durable-child-{}-{nanos}.sqlite",
            std::process::id()
        ));
        let pool = schema::connect(db.to_str().unwrap())
            .await
            .expect("connect");
        schema::migrate(&pool).await.expect("migrate");
        let store = toron_core::index::workflow_effects::WorkflowEffectStore::new(pool);

        let parent = toron_workflow::schema::parse_workflow(
            "name: parent\nsteps:\n  - name: call\n    action: call_workflow\n    workflow: child\n    inputs: {}\n",
        )
        .expect("parse parent");
        let child = toron_workflow::schema::parse_workflow(
            "name: child\nsteps:\n  - name: check\n    action: verify\n    command: \"true\"\n",
        )
        .expect("parse child");

        // A counting fake, not `real_verify_sink`: the witness must prove the
        // child's verify leaf ran exactly once without spawning a shell.
        let verify_calls = Arc::new(AtomicUsize::new(0));
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: None,
                sync_runner: None,
                verify_sink: counting_verify_sink(verify_calls.clone()),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: None,
            witness: None,
            clock: Arc::new(unix_ms),
            spawn_resolver: None,
            resolve_child: Some(Arc::new(move |name: &str| {
                (name == "child").then(|| child.clone())
            })),
        };

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(
                &store,
                "graph-child-1",
                &parent,
                serde_json::json!({}),
                &seams,
            ),
        )
        .await
        .expect("a resolved child graph must not hang");
        let output = match outcome {
            BodyOutcome::Completed(output) => output,
            BodyOutcome::Failed { error, step } => {
                panic!("a resolved child graph must complete: {error} (step {step:?})")
            }
        };
        assert_eq!(
            output["call"]["steps"]["check"]["ok"],
            serde_json::json!(true),
            "the child verify ran and its terminal joined: {output}"
        );
        assert_eq!(
            verify_calls.load(Ordering::SeqCst),
            1,
            "the child's verify leaf runs exactly once"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// Child-call refusal witness: without a child resolver the graph refuses
    /// at open with the host's exact contract text, before any leaf can run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn graph_child_call_without_a_resolver_refuses_at_open() {
        use toron_core::index::schema;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let db = std::env::temp_dir().join(format!(
            "fw-durable-child-none-{}-{nanos}.sqlite",
            std::process::id()
        ));
        let pool = schema::connect(db.to_str().unwrap())
            .await
            .expect("connect");
        schema::migrate(&pool).await.expect("migrate");
        let store = toron_core::index::workflow_effects::WorkflowEffectStore::new(pool);

        let parent = toron_workflow::schema::parse_workflow(
            "name: parent\nsteps:\n  - name: call\n    action: call_workflow\n    workflow: child\n    inputs: {}\n",
        )
        .expect("parse parent");

        let verify_calls = Arc::new(AtomicUsize::new(0));
        let seams = BodySeams {
            leaf: LeafSeams {
                async_runner: None,
                sync_runner: None,
                verify_sink: counting_verify_sink(verify_calls.clone()),
                journal_mode: toron_workflow::engine::EffectJournalMode::Recording,
            },
            launcher: None,
            job_watcher: None,
            witness: None,
            clock: Arc::new(unix_ms),
            spawn_resolver: None,
            resolve_child: None,
        };

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drive(
                &store,
                "graph-child-no-resolver",
                &parent,
                serde_json::json!({}),
                &seams,
            ),
        )
        .await
        .expect("a refused child graph must not hang");
        match outcome {
            BodyOutcome::Failed { error, step } => {
                assert!(
                    error.contains("call_workflow needs a workflow resolver on this host"),
                    "the host's refusal text reaches the caller: {error}"
                );
                assert_eq!(step, None, "refusal happens at open, before a node runs");
            }
            other => panic!("a resolver-less child graph must fail at open: {other:?}"),
        }
        assert_eq!(
            verify_calls.load(Ordering::SeqCst),
            0,
            "a refused open runs no leaf"
        );
        let _ = std::fs::remove_file(&db);
    }

    /// A fake verify sink: counts calls, never shells out.
    fn counting_verify_sink(calls: Arc<AtomicUsize>) -> toron_workflow::engine::VerifySink {
        use toron_workflow::engine::{VerifyRequest, VerifyResult, VerifySink};
        let sink = VerifySink::default();
        let f = Arc::new(move |_: &VerifyRequest| -> anyhow::Result<VerifyResult> {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(VerifyResult {
                ok: true,
                exit: 0,
                stdout_path: None,
                stderr_path: None,
                error: None,
                json: None,
            })
        });
        *sink.0.lock().expect("verify sink lock poisoned") = Some(f);
        sink
    }
}

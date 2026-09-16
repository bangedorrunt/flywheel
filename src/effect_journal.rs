//! Host-owned effect journal for flywheel's engine run path
//! (ADR-0032 Spec 5/6, `bd-engine-run-path-journaling-fwlx`).
//!
//! flywheel embeds the engine, so it owns the journal. Before this module its
//! effectful leaves (`dispatch_job`, `send_message`/`send_dm`) fell through to
//! the legacy `ActionRunner` and wrote **no** `workflow_effects` row — the
//! silent bypass Spec 6 forbids. Injecting an [`EffectRunner`] built here puts
//! every effectful leaf on the fenced path: commit prepared intent → claim the
//! attempt with a fencing token → invoke the provider **once** → settle (or
//! fail, or record `unknown`).
//!
//! The dispatch algorithm itself is not reimplemented: it is toron-mcp's
//! [`EffectDispatcher`] over toron-core's [`WorkflowEffectStore`], so the host
//! and the CLI share one journal implementation and one reconciliation policy
//! ([`toron_workflow::engine::plan_attempt`]).
//!
//! governed-by: ADR-0032

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;

use anyhow::bail;
use toron_core::index::workflow_effects::{EffectState, NewRun, RunStatus, WorkflowEffectStore};
use toron_core::mailbox::SendMessage;
use toron_core::nostr::Tag;
use toron_mcp::workflow_host::{DispatchRequest, EffectDispatcher};
use toron_workflow::engine::{
    ActionInvocation, ActionPartition, ActionRunner, EffectRunner, ProviderErrorClass,
    ProviderOutcome, RunJournal, RunPhase, RunTransition, capability_for_action_type,
    classify_provider_error_msg, derive_effect_key, partition,
};
use toron_workflow::identity::{action_digest_of, redact_value};
use toron_workflow::stamp_workflow_emitted;

/// Drive one async journal call from the sync [`EffectRunner`] without a
/// nested-runtime panic (the toron-mcp house pattern, ADR-0007 F8).
///
/// The runner is a sync closure by engine contract, but the journal is async
/// (SQLite). Blocking the caller's runtime would panic whenever a run is
/// invoked from inside one — which the cron daemon and the durable loop both
/// do. A dedicated OS thread with its own current-thread runtime is the only
/// correct isolation; it is joined immediately, so at most one extra thread
/// exists per effectful leaf.
fn block_on_journal<F, T>(fut: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build effect-journal runtime")?;
        rt.block_on(fut)
    })
    .join()
    .map_err(|_| anyhow::anyhow!("effect-journal thread panicked"))?
}

/// The journal store for this process, or `None` when the toron index cannot
/// be opened at all.
///
/// Preferred source is the mailbox flywheel already built (no second SQLite
/// connection, no second schema — migrations including `0010_workflow_effects`
/// ran in `mail_shim::build_mailbox`).
///
/// Most CLI paths never build a mailbox, though, and those are exactly the ones
/// that run workflows. Opening the SAME index file the mailbox would use
/// (`mailbox_roots`, bd-enov: one index, one schema) keeps the journal engaged
/// without forcing mail construction — and without calling
/// `init_mailbox`, whose shared-runtime `block_on` would panic inside the cron
/// daemon's async context. Migrations are idempotent, and the work runs on the
/// same thread-isolated runtime the dispatch path uses.
pub fn journal_store() -> Option<WorkflowEffectStore> {
    if let Some(pool) =
        crate::mail_shim::try_mailbox().and_then(toron_core::mailbox::Mailbox::index_pool)
    {
        return Some(WorkflowEffectStore::new(pool.clone()));
    }
    let (data_dir, db) = crate::mail_shim::mailbox_roots();
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!(
            "flywheel: cannot create the toron data dir {}: {e} — effectful workflow leaves \
             stay UNJOURNALED this run (ADR-0032 Spec 6)",
            data_dir.display()
        );
        return None;
    }
    let db_path = db.clone();
    let db = db.to_string_lossy().into_owned();
    match block_on_journal(async move {
        let pool = toron_core::index::schema::connect(&db).await?;
        toron_core::index::schema::migrate(&pool).await?;
        Ok(WorkflowEffectStore::new(pool))
    }) {
        Ok(store) => Some(store),
        Err(e) => {
            eprintln!(
                "flywheel: cannot open the effect journal at {}: {e:#} — effectful workflow \
                 leaves stay UNJOURNALED this run (ADR-0032 Spec 6)",
                db_path.display()
            );
            None
        }
    }
}

/// Wrap a leaf runner in the host-owned effect journal (see module docs).
///
/// `inner` stays the provider: this wrapper adds intent/claim/settle around
/// exactly the effect the embedding already performs. It never invents an
/// effect — a sink the inner runner does not perform is still reported by the
/// engine's own bypass marker, not settled here.
pub fn effect_runner(
    inner: ActionRunner,
    store: WorkflowEffectStore,
    plane: Option<MailPlane>,
) -> EffectRunner {
    let dispatcher = EffectDispatcher::new(store);
    Arc::new(move |inv: &ActionInvocation| {
        let ActionPartition::HostSink { sink } = partition(&inv.action) else {
            // Engine-owned control flow is sequenced by the engine. A runner
            // that journaled it would claim an external effect nobody
            // performed, so it stays a pass-through (the engine's boundary
            // rejects a real misdispatch on its own).
            return Ok(leaf_value((inner)(&inv.action)));
        };
        match sink {
            // The leaf runner is the provider: a real pane or a real
            // `__run-headless` subprocess performs the effect.
            SINK_JOB => {
                let action = inv.action.clone();
                let provider = inner.clone();
                Ok(fenced_outcome(dispatch_once(
                    &dispatcher,
                    inv,
                    Box::new(move || provider_outcome((provider)(&action))),
                )))
            }
            // The mailbox is its own provider (flywheel's leaf runners record
            // mailbox actions rather than dispatching them), so without a
            // sender there is no effect to journal.
            SINK_MAILBOX => {
                let Some(plane) = plane.clone() else {
                    return Ok(uncovered_sink(
                        &inner,
                        inv,
                        sink,
                        "no in-process toron mailbox is available for a fenced send \
                         (Spec 5.4 needs `corr == effect_key`, which the remote wire cannot carry)",
                    ));
                };
                let key = effect_key_for(inv);
                let key_out = key.clone();
                let delivery = mail_delivery(inv);
                Ok(fenced_outcome(dispatch_once(
                    &dispatcher,
                    inv,
                    Box::new(move || match delivery {
                        Ok(delivery) => mail_outcome(&plane, &key_out, delivery),
                        Err(e) => ProviderOutcome::ProvenFailed(format!("{e:#}")),
                    }),
                )))
            }
            // `verify` (bd-engine-run-path-journaling-fwlx, the Spec 6 flip):
            // the engine runs the embedding's VerifySink FIRST and hands the
            // already-executed result to this runner via
            // `RunContext.provider_result` — the sink stays the executor
            // (ADR-0024 Spec 2); the journal observes the result, it never
            // re-runs the command. No provider_result means the invocation
            // came from outside the engine's verify path, so there is no
            // observed effect to journal and the row would be a lie.
            SINK_VERIFY => {
                let Some(result) = inv.run.provider_result.clone() else {
                    return Ok(uncovered_sink(
                        &inner,
                        inv,
                        sink,
                        "no engine-observed verify result on this invocation — the VerifySink \
                         never ran, so there is no effect to journal",
                    ));
                };
                // The SINK is verify's provider, so this is the one sink whose
                // "provider" is not the leaf runner: the engine already
                // executed it and handed us the result. The leaf runner is
                // deliberately NOT invoked — for a verify action both live
                // runners (`pane_dispatch`, `headless_dispatch`) return a bare
                // `{action: …}` no-op, so calling it adds no evidence, and
                // calling it wholesale would double-perform the command the
                // moment a runner grew real verify handling.
                Ok(fenced_outcome(dispatch_once(
                    &dispatcher,
                    inv,
                    Box::new(move || provider_outcome(Ok(result))),
                )))
            }
            other => Ok(uncovered_sink(
                &inner,
                inv,
                other,
                "no provider performs this sink",
            )),
        }
    })
}

/// The named sink the leaf runner performs (a real pane / `__run-headless`
/// subprocess) — an external effect worth a journal row.
const SINK_JOB: &str = "job";

/// The named sink this module performs itself, through [`MailSender`].
const SINK_MAILBOX: &str = "mailbox";

/// The verify sink: executed by the engine through the embedding's
/// `VerifySink`, journaled here around the observed result.
const SINK_VERIFY: &str = "verify";

/// Where a workflow mailbox effect actually goes: deliver one assembled
/// [`SendMessage`] and report the wire count. Injected, so the sink's assembly
/// (sender, `corr`, stamps) is provable without a live mailbox.
pub type MailSender = Arc<dyn Fn(SendMessage) -> Result<usize> + Send + Sync>;

/// Who a workflow-originated message is sent AS. Resolved through flywheel's
/// ONE actor ladder and registered best-effort, exactly like every other
/// flywheel send — a workflow must not invent a second identity plane.
pub type MailActor = Arc<dyn Fn(&str) -> Result<String> + Send + Sync>;

/// The mailbox sink's dependencies: who sends, and how it reaches the wire.
#[derive(Clone)]
pub struct MailPlane {
    pub actor: MailActor,
    pub send: MailSender,
}

/// A host sink no provider performs.
///
/// It is NOT journaled: a `settled` row would claim an effect nobody performed.
/// The leaf still runs through its runner and carries the engine's own
/// uncovered-sink marker, so the gap stays visible in the ledger instead of
/// looking journaled — and it is reported once per occurrence, loudly, with the
/// reason.
fn uncovered_sink(
    inner: &ActionRunner,
    inv: &ActionInvocation,
    sink: &'static str,
    why: &str,
) -> Value {
    eprintln!(
        "flywheel: effectful '{sink}' action '{}' wrote NO effect-journal row \
         ({}): {why} — journaling it would settle work nobody did (ADR-0032 Spec 6)",
        inv.action.type_name(),
        toron_workflow::engine::EFFECT_JOURNAL_SINK_UNCOVERED
    );
    let mut value = leaf_value((inner)(&inv.action));
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "effect_journal".into(),
            Value::String(toron_workflow::engine::EFFECT_JOURNAL_SINK_UNCOVERED.to_string()),
        );
    }
    value
}

/// Run one effectful leaf through the fenced journal: commit prepared intent,
/// claim the attempt, invoke the provider EXACTLY once, settle (or fail, or
/// record `unknown`). The journal observes a real effect; it never invents one.
fn dispatch_once(
    dispatcher: &EffectDispatcher,
    inv: &ActionInvocation,
    provider: Box<dyn FnOnce() -> ProviderOutcome + Send>,
) -> Result<toron_mcp::workflow_host::DispatchResult> {
    let req = dispatch_request(inv);
    let dispatcher = dispatcher.clone();
    block_on_journal(async move { dispatcher.dispatch(&req, move |_req| provider()).await })
}

/// The leaf's delivery data, before any identity is attached: what a
/// `send_message` / `send_dm` step actually asks for.
struct MailDelivery {
    project: String,
    to: Vec<String>,
    subject: String,
    body: String,
}

/// Read one mailbox leaf's delivery data. Only a mailbox action reaches here
/// (the partition says so), and a mismatch is a proven failure naming itself.
fn mail_delivery(inv: &ActionInvocation) -> Result<MailDelivery> {
    let (project, to, subject, body) = match &inv.action {
        toron_workflow::schema::Action::SendMessage {
            to,
            subject,
            body,
            project,
        } => (
            project.clone().unwrap_or_default(),
            to.clone(),
            subject.clone(),
            body.clone(),
        ),
        // A DM names one recipient and carries no subject; "workflow dm" is
        // the toron host's own placeholder, reused so both planes read alike.
        toron_workflow::schema::Action::SendDm { to, body, project } => (
            project.clone().unwrap_or_default(),
            vec![to.clone()],
            "workflow dm".to_string(),
            body.clone(),
        ),
        other => bail!(
            "not a mailbox action: {} reached the mailbox sink",
            other.type_name()
        ),
    };
    Ok(MailDelivery {
        project,
        to,
        subject,
        body,
    })
}

/// Assemble the wire message for one delivery (ADR-0007 F6, ADR-0032 Spec 5.4,
/// GS-039 P0-1): the `toron:workflow-emitted` stamp keeps a workflow from
/// re-triggering itself, `corr` IS the effect key so the receipt joins back to
/// the journal row, and `wake: tasked` keeps the delivery from being wake-null
/// theater.
fn mail_message(delivery: &MailDelivery, sender: String, effect_key: &str) -> SendMessage {
    let mut extra_tags = stamp_workflow_emitted(Vec::new());
    for (k, v) in [("corr", effect_key), ("wake", "tasked")] {
        match Tag::parse([k, v]) {
            Ok(tag) => extra_tags.push(tag),
            Err(e) => eprintln!("flywheel: effect journal: {k} tag {v:?} rejected: {e}"),
        }
    }
    SendMessage {
        project_slug: delivery.project.clone(),
        sender_name: sender,
        to: delivery.to.clone(),
        subject: delivery.subject.clone(),
        body_md: delivery.body.clone(),
        extra_tags,
        ..Default::default()
    }
}

/// Resolve the sender, assemble the message, and hand it to the transport. The
/// identity is resolved at SEND time (never cached) so it follows the same
/// ladder — and the same roster — as every other flywheel send.
fn mail_outcome(plane: &MailPlane, effect_key: &str, delivery: MailDelivery) -> ProviderOutcome {
    let sender = match (plane.actor)(&delivery.project) {
        Ok(sender) => sender,
        Err(e) => return classify_failure(format!("{e:#}")),
    };
    let message = mail_message(&delivery, sender, effect_key);
    match (plane.send)(message) {
        Ok(count) => ProviderOutcome::Delivered(serde_json::json!({
            "ok": true,
            "count": count,
            "effect_key": effect_key,
        })),
        Err(e) => classify_failure(format!("{e:#}")),
    }
}

/// The runner-shaped value for one leaf result: the engine's own compat
/// projection (an `Err` becomes `{ok:false,…}`, never a bare error string).
fn leaf_value(result: Result<Value>) -> Value {
    match result {
        Ok(value) => value,
        Err(e) => serde_json::json!({
            "ok": false,
            "error": e.to_string(),
            // GS-049 P1-3: phase-bearing — leaf Err is an execute-stage failure
            "stage": "execute",
        }),
    }
}

/// The two halves of the host-owned journal, built over ONE store (bd-mi9n):
/// the effect runner and the run-lifecycle journal. They belong together — the
/// run row is the index its effect rows hang off, and both must key on the
/// same `run_id`.
pub struct Journals {
    pub effect: EffectRunner,
    pub run: RunJournal,
}

/// [`effect_runner`] + [`run_journal`] against the process journal and mail
/// plane, or `None` when no toron index is open — the engine then keeps its
/// loud `bypassed-no-effect-runner` marker rather than a run that only looks
/// journaled.
pub fn journals(cfg: &crate::config::Config, inner: ActionRunner) -> Option<Journals> {
    // Mail first: when it is in-process it also builds the pool the journal
    // borrows, so the two share one connection instead of opening two.
    let plane = mail_plane(cfg);
    match journal_store() {
        Some(store) => Some(Journals {
            effect: effect_runner(inner, store.clone(), plane),
            run: run_journal(store),
        }),
        None => {
            eprintln!(
                "flywheel: no toron index available — effectful workflow leaves will stay \
                 UNJOURNALED (ADR-0032 Spec 6)"
            );
            None
        }
    }
}

/// The run-lifecycle half of the journal (bd-mi9n, ADR-0032 Spec 5/6).
///
/// The engine calls this for every phase of a run it drives, so the
/// `workflow_runs` row exists without the embedding making its own
/// `create_run`/`cas_run_status` calls — and because the engine supplies the
/// `run_id` here, the run row and its effect rows agree by construction.
///
/// Idempotent by design (a durable run replays, so the same phase arrives more
/// than once): the row is created if absent, then CAS'd to the target only when
/// it differs. A terminal row never moves (toron-core refuses the CAS), so a
/// replayed tail cannot resurrect a finished run.
///
/// Best-effort by the same rule as the effect path: a journal failure is
/// reported loud and never changes the run's outcome — the run itself already
/// happened.
pub fn run_journal(store: WorkflowEffectStore) -> RunJournal {
    Arc::new(move |t: &RunTransition| {
        let target = match t.phase {
            // `workflow_runs` is born `started`; the engine's Started/Running
            // pair advances it to `running` exactly once (the CAS refuses a
            // repeat).
            RunPhase::Started | RunPhase::Running => RunStatus::Running,
            RunPhase::Succeeded => RunStatus::Succeeded,
            RunPhase::Failed => RunStatus::Failed,
        };
        let store = store.clone();
        let run_id = t.run_id.clone();
        let workflow = t.workflow.clone();
        let workflow_digest = t.workflow_digest.clone();
        let label = run_id.clone();
        if let Err(e) = block_on_journal(async move {
            let current = match store.get_run(&run_id).await? {
                Some(row) => row.status,
                None => {
                    store
                        .create_run(NewRun {
                            run_id: run_id.clone(),
                            project: None,
                            workflow,
                            workflow_digest,
                        })
                        .await?;
                    RunStatus::Started
                }
            };
            if current != target {
                let _ = store.cas_run_status(&run_id, current, target).await?;
            }
            Ok(())
        }) {
            eprintln!(
                "flywheel: run lifecycle row for {label} ({:?}) not journaled: {e:#} — the run \
                 itself is unaffected (ADR-0032 Spec 5)",
                t.phase
            );
        }
    })
}

/// The production mail plane for the fenced path, or `None` when this install's
/// mail plane cannot carry one.
///
/// GS-048 residual honesty: expand journal coverage **only** when a real
/// provider exists (job / in-process mailbox / engine-observed verify).
/// Remote mail stays UNJOURNALED — never invent-green a settled row for a
/// wire that cannot carry `corr == effect_key`. Uncovered sinks keep the
/// loud `EFFECT_JOURNAL_SINK_UNCOVERED` marker.
///
/// A remote `[mail.toron] url` is refused deliberately: the daemon's wire
/// surface cannot carry the `corr` tag the journal needs (Spec 5.4, and
/// `mail_shim` refuses that same send), and the journal's index is the LOCAL
/// one — sending here would land a message the daemon's mailbox never reads.
/// The leaves then keep the honest uncovered-sink marker instead.
fn mail_plane(cfg: &crate::config::Config) -> Option<MailPlane> {
    let url = cfg.toron_mail_config().url.clone().unwrap_or_default();
    if !url.trim().is_empty() {
        eprintln!(
            "flywheel: mail plane is remote ({url}) — workflow mailbox leaves stay UNJOURNALED \
             (a fenced send needs the in-process toron mailbox to carry `corr`)"
        );
        return None;
    }
    // In-process plane: make sure the shared mailbox exists (runtime-safe) and
    // deliver through it.
    let _ = crate::mail_shim::Mail::from_config(None);
    let mailbox = crate::mail_shim::try_mailbox()?.clone();
    let send: MailSender = {
        let mailbox = mailbox.clone();
        Arc::new(move |message: SendMessage| {
            let mailbox = mailbox.clone();
            block_on_journal(async move { mailbox.send_message(&message).await.map(|r| r.count) })
        })
    };
    // The sender is the ONE actor ladder (explicit `--as` cannot exist in a
    // workflow, so the env ladder then the config default), read against the
    // project's live roster and registered best-effort — the same identity
    // every other flywheel send binds, so leases and mail agree.
    let actor: MailActor = {
        let mailbox = mailbox.clone();
        Arc::new(move |project: &str| {
            let mailbox = mailbox.clone();
            let project = project.to_string();
            block_on_journal(async move {
                let roster: Vec<String> = mailbox
                    .list_agents(&project, toron_core::mailbox::ListAgentsOptions::default())
                    .await
                    .map(|agents| agents.into_iter().map(|a| a.name).collect::<Vec<String>>())
                    .unwrap_or_default();
                let name = crate::resolve_mail_actor_roster(
                    &crate::project_base(&project),
                    None,
                    &|key| std::env::var(key).ok(),
                    &roster,
                )
                .unwrap_or_else(|| "flywheel".to_string());
                if !roster.iter().any(|n| n == &name) {
                    let profile = toron_core::nostr::identity::AgentProfile {
                        name: name.clone(),
                        program: Some("flywheel".to_string()),
                        model: None,
                        persona: None,
                        task_description: Some("flywheel workflow actor".to_string()),
                    };
                    mailbox
                        .register_agent(&project, &profile)
                        .await
                        .map_err(|e| anyhow::anyhow!("register workflow actor {name}: {e}"))?;
                }
                Ok(name)
            })
        })
    };
    Some(MailPlane { actor, send })
}

/// Stable effect identity for one invocation (ADR-0032 Spec 3): the journal
/// key is the one derivation the engine boundary owns — `(workflow_digest,
/// run_id, node_path, iteration_key, action_digest)` — so a retry of one
/// logical effect reuses the key and a different run or node never collides on
/// a byte-identical payload. A pre-minted `RunContext.effect_key` wins.
pub fn effect_key_for(inv: &ActionInvocation) -> String {
    inv.run.effect_key.clone().unwrap_or_else(|| {
        derive_effect_key(
            &inv.run.workflow_digest,
            &inv.run.run_id,
            &inv.run.node_path,
            inv.run.iteration_key.as_deref(),
            &action_digest_for(inv),
        )
    })
}

/// The redacted action digest for one invocation: `H(canonical redacted
/// render)`. Secrets never enter the journal's identity or its evidence.
fn action_digest_for(inv: &ActionInvocation) -> String {
    let rendered = serde_json::to_value(&inv.action).unwrap_or(Value::Null);
    let (redacted, _) = redact_value(&rendered);
    action_digest_of(&redacted)
}

/// Map one leaf result onto a provider outcome (ADR-0032 I5). One ambiguity
/// rule covers both shapes — a raised error and an in-band `ok:false` — so a
/// timeout is never journaled as a proven failure and a proven rejection is
/// never journaled as `unknown`.
fn provider_outcome(result: Result<Value>) -> ProviderOutcome {
    match result {
        Ok(value) => match in_band_failure(&value) {
            Some(msg) => classify_failure(msg),
            None => ProviderOutcome::Delivered(value),
        },
        Err(e) => classify_failure(format!("{e:#}")),
    }
}

/// The error text of a leaf value that reported its own failure in band
/// (`{"ok": false, "error": "…"}`), or `None` when the value is a success.
fn in_band_failure(value: &Value) -> Option<String> {
    if value.get("ok") != Some(&Value::Bool(false)) {
        return None;
    }
    Some(
        value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("leaf reported ok:false")
            .to_string(),
    )
}

/// Classify one failure message: recognized transport uncertainty is
/// `Ambiguous` (the effect may have happened), everything else is a proven
/// failure. Delegates to the engine's one classifier — this module must not
/// grow a second opinion about what "unknown" means.
fn classify_failure(msg: String) -> ProviderOutcome {
    match classify_provider_error_msg(&msg) {
        ProviderErrorClass::Ambiguous => ProviderOutcome::Ambiguous(msg),
        ProviderErrorClass::Proven => ProviderOutcome::ProvenFailed(msg),
    }
}

/// The fenced dispatch request for one invocation (ADR-0032 Spec 5.4): the
/// row carries the run/node scope and the redacted action as evidence.
fn dispatch_request(inv: &ActionInvocation) -> DispatchRequest {
    let action_type = inv.action.type_name().to_string();
    let project = match &inv.action {
        toron_workflow::schema::Action::SendMessage { project, .. }
        | toron_workflow::schema::Action::SendDm { project, .. } => project.clone(),
        _ => None,
    };
    let rendered = serde_json::to_value(&inv.action).unwrap_or(Value::Null);
    let (redacted, _) = redact_value(&rendered);
    DispatchRequest {
        effect_key: effect_key_for(inv),
        run_id: inv.run.run_id.clone(),
        project,
        workflow_digest: inv.run.workflow_digest.clone(),
        node_path: inv.run.node_path.clone(),
        iteration_key: inv.run.iteration_key.clone(),
        action_type: action_type.clone(),
        action_digest: action_digest_of(&redacted),
        redacted_action: Some(redacted),
        capability: capability_for_action_type(&action_type),
    }
}

/// Project a journaled dispatch result back onto the leaf's `{ok,…}` shape:
/// only `settled` reads as success; every other state names itself, so an
/// unproven effect can never masquerade as a completed step.
fn fenced_outcome(result: Result<toron_mcp::workflow_host::DispatchResult>) -> Value {
    match result {
        Ok(res) if res.state == EffectState::Settled => res.outcome,
        Ok(res) => serde_json::json!({
            "ok": false,
            "effect_key": res.effect_key,
            "effect_state": res.state.as_str(),
            "outcome": res.outcome,
            "stage": "effect",
        }),
        Err(e) => serde_json::json!({
            "ok": false,
            "error": format!("{e:#}"),
            // GS-049 P1-3: fence dispatch Err — effect stage
            "stage": "effect",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;
    use toron_workflow::engine::{ExecutionPolicy, RunContext};
    use toron_workflow::schema::{Action, DispatchSpec};

    /// A migrated, throwaway journal — the same `connect` + `migrate` pair
    /// `mail_shim::build_mailbox` runs against the real index.
    async fn test_store() -> (tempfile::TempDir, WorkflowEffectStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("index.sqlite");
        let pool = toron_core::index::schema::connect(&db.to_string_lossy())
            .await
            .expect("connect");
        toron_core::index::schema::migrate(&pool)
            .await
            .expect("migrate");
        (dir, WorkflowEffectStore::new(pool))
    }

    fn dispatch_job() -> Action {
        Action::DispatchJob {
            assignee: "pi".to_string(),
            spec: DispatchSpec {
                task: Some("work".to_string()),
                ..Default::default()
            },
            channel: None,
        }
    }

    fn send_message() -> Action {
        Action::SendMessage {
            to: vec!["pi".to_string()],
            subject: "handoff".to_string(),
            body: "body".to_string(),
            project: Some("demo".to_string()),
        }
    }

    fn verify() -> Action {
        Action::Verify {
            command: "cargo test --lib".to_string(),
        }
    }

    /// A plane that captures what the sink assembled and reports a wire count.
    /// The actor is fixed here so the assembly (tags, counts) is what is under
    /// test; the real ladder is pinned separately.
    fn recording_plane(seen: Arc<std::sync::Mutex<Vec<SendMessage>>>) -> MailPlane {
        MailPlane {
            actor: Arc::new(|_project: &str| Ok("workflow-demo".to_string())),
            send: Arc::new(move |message: SendMessage| {
                seen.lock().unwrap().push(message);
                Ok(1)
            }),
        }
    }

    fn invocation(action: Action, run_id: &str, node: &str) -> ActionInvocation {
        ActionInvocation {
            action,
            run: RunContext {
                run_id: run_id.to_string(),
                workflow_digest: "wf-digest-1".to_string(),
                node_path: node.to_string(),
                iteration_key: None,
                effect_key: None,
                attempt: 1,
                provider_result: None,
            },
            execution: ExecutionPolicy {
                max_attempts: 1,
                timeout_secs: None,
            },
        }
    }

    /// The key the ENGINE boundary derivation yields for an invocation —
    /// computed here from the public API, independently of how the journal
    /// derives it, so a flywheel-invented key scheme fails this test.
    fn boundary_derived_key(inv: &ActionInvocation) -> String {
        let rendered = serde_json::to_value(&inv.action).expect("action serializes");
        let (redacted, _) = redact_value(&rendered);
        derive_effect_key(
            &inv.run.workflow_digest,
            &inv.run.run_id,
            &inv.run.node_path,
            inv.run.iteration_key.as_deref(),
            &action_digest_of(&redacted),
        )
    }

    /// The journal is keyed by the ONE derivation the engine owns (Spec 3) —
    /// an embedding must not mint a second identity scheme.
    #[tokio::test]
    async fn journal_key_is_the_engine_boundary_derivation() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner = Arc::new(|_a: &Action| Ok(json!({ "ok": true })));
        let fx = effect_runner(inner, store.clone(), None);
        let inv = invocation(dispatch_job(), "run-key", "ship");

        fx(&inv).expect("runner returns a value");

        let key = boundary_derived_key(&inv);
        assert_eq!(key, effect_key_for(&inv), "one key derivation, not two");
        let row = store
            .get_effect(&key)
            .await
            .expect("query")
            .expect("the row lives at the boundary-derived key");
        assert_eq!(row.run_id, "run-key");
        assert_eq!(row.node_path, "ship");
    }

    /// ADR-0032 I3: intent is committed and the attempt claimed BEFORE the
    /// provider is invoked. The provider observes the row from outside the
    /// dispatcher's own call, so a prepare-after-dispatch order fails here.
    #[tokio::test]
    async fn intent_and_claim_are_committed_before_the_provider_runs() {
        let (_dir, store) = test_store().await;
        let inv = invocation(dispatch_job(), "run-intent", "ship");
        let key = effect_key_for(&inv);
        let seen: Arc<std::sync::Mutex<Option<EffectState>>> =
            Arc::new(std::sync::Mutex::new(None));
        let seen_c = seen.clone();
        let probe_store = store.clone();
        let probe_key = key.clone();
        let inner: ActionRunner = Arc::new(move |_a: &Action| {
            let store = probe_store.clone();
            let k = probe_key.clone();
            let row = block_on_journal(async move { store.get_effect(&k).await })
                .expect("probe read")
                .expect("the intent row exists before the provider runs");
            *seen_c.lock().unwrap() = Some(row.state);
            Ok(json!({ "ok": true }))
        });
        let fx = effect_runner(inner, store.clone(), None);

        fx(&inv).expect("runner returns a value");

        assert_eq!(
            *seen.lock().unwrap(),
            Some(EffectState::InFlight),
            "the attempt was claimed before the provider ran (Spec 5.3 step 4 precedes step 6)"
        );
    }

    /// A delivered effect settles, and the step value is the provider's own —
    /// the journal observes, it does not rewrite outputs.
    #[tokio::test]
    async fn settled_effect_journals_the_claim_and_settle_attempts() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner = Arc::new(|_a: &Action| Ok(json!({ "ok": true, "pane": "p1" })));
        let fx = effect_runner(inner, store.clone(), None);
        let inv = invocation(dispatch_job(), "run-settle", "ship");

        let out = fx(&inv).expect("runner returns a value");

        assert_eq!(out, json!({ "ok": true, "pane": "p1" }));
        let key = effect_key_for(&inv);
        let row = store.get_effect(&key).await.unwrap().expect("row");
        assert_eq!(row.state, EffectState::Settled);
        assert_eq!(row.action_type, "dispatch_job");
        assert_eq!(row.workflow_digest, "wf-digest-1");
        let attempts = store.list_attempts(&key).await.unwrap();
        assert!(
            attempts.iter().any(|a| a.state == EffectState::InFlight),
            "the claim is append-only evidence: {attempts:?}"
        );
        assert!(
            attempts.iter().any(|a| a.state == EffectState::Settled),
            "the settle is append-only evidence: {attempts:?}"
        );
    }

    /// I5: a timeout/transport loss is never journaled as a proven failure.
    #[tokio::test]
    async fn ambiguous_provider_error_journals_unknown_not_failed() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner =
            Arc::new(|_a: &Action| Err(anyhow::anyhow!("connection reset by peer")));
        let fx = effect_runner(inner, store.clone(), None);
        let inv = invocation(dispatch_job(), "run-amb", "ship");

        let out = fx(&inv).expect("a fenced runner returns a value, never a bare Err");

        assert_eq!(
            out["ok"],
            json!(false),
            "an unproven effect never reads as success: {out}"
        );
        assert_eq!(out["effect_state"], json!("unknown"), "{out}");
        let row = store
            .get_effect(&effect_key_for(&inv))
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.state, EffectState::Unknown);
        assert!(
            !row.state.is_terminal(),
            "unknown is explicitly nonterminal"
        );
    }

    /// A proven rejection stays a proven failure — the classifier must not
    /// widen `unknown` to every error.
    #[tokio::test]
    async fn proven_provider_error_journals_failed() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner =
            Arc::new(|_a: &Action| Err(anyhow::anyhow!("unknown agent 'nobody'")));
        let fx = effect_runner(inner, store.clone(), None);
        let inv = invocation(dispatch_job(), "run-fail", "ship");

        let out = fx(&inv).expect("runner returns a value");

        assert_eq!(out["effect_state"], json!("failed"), "{out}");
        let row = store
            .get_effect(&effect_key_for(&inv))
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.state, EffectState::Failed);
    }

    /// A terminal effect replays its stored outcome instead of re-dispatching:
    /// the second attempt must not repeat an external effect.
    #[tokio::test]
    async fn terminal_effect_replays_without_redispatching() {
        let (_dir, store) = test_store().await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_c = calls.clone();
        let inner: ActionRunner = Arc::new(move |_a: &Action| {
            calls_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "ok": true, "count": 7 }))
        });
        let fx = effect_runner(inner, store.clone(), None);
        let inv = invocation(dispatch_job(), "run-replay", "ship");

        fx(&inv).expect("first dispatch");
        let second = fx(&inv).expect("replay");

        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a terminal effect is never dispatched twice"
        );
        assert_eq!(
            second,
            json!({ "ok": true, "count": 7 }),
            "the stored outcome replays"
        );
    }

    /// Two runs with a byte-identical payload are two effects, not one — the
    /// run scope is what removes the compat adapter's over-dedupe.
    #[tokio::test]
    async fn runs_scope_effect_keys_so_identical_payloads_do_not_collide() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner = Arc::new(|_a: &Action| Ok(json!({ "ok": true })));
        let fx = effect_runner(inner, store.clone(), None);
        let a = invocation(dispatch_job(), "run-a", "ship");
        let b = invocation(dispatch_job(), "run-b", "ship");

        fx(&a).expect("run a");
        fx(&b).expect("run b");

        assert_ne!(
            effect_key_for(&a),
            effect_key_for(&b),
            "run scope, not payload hash"
        );
        assert!(
            store
                .get_effect(&effect_key_for(&a))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .get_effect(&effect_key_for(&b))
                .await
                .unwrap()
                .is_some()
        );
    }

    /// Engine-owned control flow is never journaled as an external effect: the
    /// engine sequences it, and a runner that claimed it would write a row for
    /// work no provider performed.
    #[tokio::test]
    async fn engine_control_actions_are_not_journaled() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner = Arc::new(|_a: &Action| Ok(json!({ "action": "delay" })));
        let fx = effect_runner(inner, store.clone(), None);
        let inv = invocation(Action::Delay { secs: 0 }, "run-control", "wait");

        let out = fx(&inv).expect("runner returns a value");

        assert_eq!(out, json!({ "action": "delay" }));
        assert!(
            store
                .get_effect(&effect_key_for(&inv))
                .await
                .unwrap()
                .is_none(),
            "no journal row for engine-owned control flow"
        );
    }
    /// A mailbox action whose leaf cannot be journaled (no sender) is marked,
    /// never silently clean — the same uncovered-sink semantics the engine uses
    /// for a sink an injected runner does not cover.
    #[tokio::test]
    async fn mailbox_sink_without_a_sender_is_marked_not_settled() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner = Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let fx = effect_runner(inner, store.clone(), None);
        let inv = invocation(send_message(), "run-mail", "notify");

        let out = fx(&inv).expect("runner returns a value");

        assert_eq!(
            out["effect_journal"],
            json!(toron_workflow::engine::EFFECT_JOURNAL_SINK_UNCOVERED),
            "the unperformed sink is marked, never silently clean: {out}"
        );
        assert_eq!(out["action"], json!("send_message"), "the leaf still ran");
        assert!(
            store
                .get_effect(&effect_key_for(&inv))
                .await
                .unwrap()
                .is_none(),
            "no journal row for an effect nobody performed"
        );
    }

    /// The mailbox sink is journaled like the job sink: intent -> claim ->
    /// settle around a REAL delivery, with the wire contract the stack requires
    /// (workflow-emitted stamp, `corr == effect_key`, a non-null wake).
    #[tokio::test]
    async fn mailbox_sink_journals_intent_claim_settle_around_a_delivery() {
        let (_dir, store) = test_store().await;
        let seen: Arc<std::sync::Mutex<Vec<SendMessage>>> = Arc::new(std::sync::Mutex::new(vec![]));
        let inner: ActionRunner = Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let fx = effect_runner(inner, store.clone(), Some(recording_plane(seen.clone())));
        let inv = invocation(send_message(), "run-mail", "notify");

        let out = fx(&inv).expect("runner returns a value");

        let key = effect_key_for(&inv);
        // Clone out of the guard so no `MutexGuard` is live across the awaits
        // below (clippy::await_holding_lock); the guard drops here.
        let sent = seen.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "exactly one delivery for one logical effect");
        let msg = &sent[0];
        assert_eq!(msg.project_slug, "demo");
        assert_eq!(msg.to, vec!["pi".to_string()]);
        assert_eq!(msg.subject, "handoff");
        assert_eq!(msg.body_md, "body");
        assert_eq!(
            msg.sender_name, "workflow-demo",
            "the resolved actor is the sender, never a synthetic pin"
        );
        let tags: Vec<Vec<String>> = msg
            .extra_tags
            .iter()
            .map(|t| t.as_slice().to_vec())
            .collect();
        assert!(
            tags.iter()
                .any(|t| t.first().map(String::as_str) == Some("corr")
                    && t.get(1).map(String::as_str) == Some(key.as_str())),
            "corr IS the effect key (Spec 5.4): {tags:?}"
        );
        assert!(
            tags.iter()
                .any(|t| t.first().map(String::as_str) == Some("wake")),
            "a delivery is never wake-null theater (GS-039 P0-1): {tags:?}"
        );
        assert!(
            tags.iter()
                .any(|t| t.get(1).map(String::as_str) == Some("toron:workflow-emitted")),
            "the loop-prevention stamp rides the rumor (ADR-0007 F6): {tags:?}"
        );
        assert_eq!(out["count"], json!(1));
        assert_eq!(out["effect_key"], json!(key.as_str()));
        let row = store.get_effect(&key).await.unwrap().expect("row");
        assert_eq!(row.state, EffectState::Settled);
        assert_eq!(row.action_type, "send_message");
        let attempts = store.list_attempts(&key).await.unwrap();
        assert!(
            attempts.iter().any(|a| a.state == EffectState::InFlight),
            "the claim is journaled before the send: {attempts:?}"
        );
        assert!(attempts.iter().any(|a| a.state == EffectState::Settled));
    }

    /// A mailed effect is not re-sent on replay: the second call must return the
    /// stored outcome and deliver nothing.
    #[tokio::test]
    async fn terminal_mailbox_effect_replays_without_resending() {
        let (_dir, store) = test_store().await;
        let seen: Arc<std::sync::Mutex<Vec<SendMessage>>> = Arc::new(std::sync::Mutex::new(vec![]));
        let inner: ActionRunner = Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let fx = effect_runner(inner, store.clone(), Some(recording_plane(seen.clone())));
        let inv = invocation(send_message(), "run-mail-replay", "notify");

        fx(&inv).expect("first");
        let second = fx(&inv).expect("replay");

        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "a terminal effect is never delivered twice"
        );
        assert_eq!(
            second["count"],
            json!(1),
            "the stored outcome replays: {second}"
        );
    }

    /// An ambiguous delivery error is `unknown`, never a proven failure — the
    /// message may have landed.
    #[tokio::test]
    async fn ambiguous_delivery_error_journals_unknown() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner = Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let plane = MailPlane {
            actor: Arc::new(|_p: &str| Ok("workflow-demo".to_string())),
            send: Arc::new(|_m: SendMessage| Err(anyhow::anyhow!("timeout waiting for relay"))),
        };
        let fx = effect_runner(inner, store.clone(), Some(plane));
        let inv = invocation(send_message(), "run-mail-amb", "notify");

        let out = fx(&inv).expect("runner returns a value");

        assert_eq!(out["effect_state"], json!("unknown"), "{out}");
        let row = store
            .get_effect(&effect_key_for(&inv))
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.state, EffectState::Unknown);
    }

    /// The production sender really delivers through the in-process toron
    /// mailbox — the plane `corr` requires.
    #[tokio::test]
    async fn in_process_mailbox_sender_delivers_a_workflow_message() {
        let (_dir, store) = test_store().await;
        crate::mail_shim::Mail::from_config(None)
            .expect("mail config")
            .expect("in-process facade");
        let mailbox = crate::mail_shim::mailbox().clone();
        // The recipient must exist in the project before the mailbox will route
        // to it — the same precondition any workflow send has in production.
        block_on_journal({
            let mailbox = mailbox.clone();
            let profile = toron_core::nostr::identity::AgentProfile {
                name: "pi".to_string(),
                program: Some("opencode".into()),
                model: None,
                persona: None,
                task_description: None,
            };
            async move { mailbox.register_agent("demo", &profile).await }
        })
        .expect("register the recipient");
        drop(mailbox);
        let plane = mail_plane(&crate::config::Config::default())
            .expect("the default config is the in-process plane");
        let inner: ActionRunner = Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let fx = effect_runner(inner, store.clone(), Some(plane));
        let inv = invocation(send_message(), "run-mail-live", "notify");

        let out = fx(&inv).expect("runner returns a value");

        assert_eq!(
            out["ok"],
            json!(true),
            "the in-process plane delivered: {out}"
        );
        let row = store
            .get_effect(&effect_key_for(&inv))
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.state, EffectState::Settled);
        assert!(
            row.transport_corr.is_some(),
            "the row links its transport corr (Spec 5.4)"
        );
    }

    /// bd-engine-run-path-journaling-fwlx (the Spec 6 flip): verify journals
    /// around the engine-observed VerifySink result carried on the invocation.
    #[tokio::test]
    async fn verify_sink_journals_around_the_engine_observed_result() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner = Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let fx = effect_runner(inner, store.clone(), None);
        let mut inv = invocation(verify(), "run-verify", "gate");
        inv.run.provider_result = Some(json!({
            "ok": true,
            "exit": 0,
            "stdout_path": ".flywheel/receipts/verify-1.txt",
        }));

        let out = fx(&inv).expect("runner returns a value");

        assert_eq!(out["ok"], json!(true), "the observed result replays: {out}");
        let key = effect_key_for(&inv);
        let row = store.get_effect(&key).await.unwrap().expect("row");
        assert_eq!(row.state, EffectState::Settled);
        assert_eq!(row.action_type, "verify");
        let attempts = store.list_attempts(&key).await.unwrap();
        assert!(
            attempts.iter().any(|a| a.state == EffectState::Settled),
            "the settle is journaled evidence: {attempts:?}"
        );
    }

    /// A failed verify (the command exited non-zero) journals a PROVEN
    /// failure — the process exit is evidence, not transport ambiguity.
    #[tokio::test]
    async fn failed_verify_journals_failed_not_unknown() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner = Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let fx = effect_runner(inner, store.clone(), None);
        let mut inv = invocation(verify(), "run-verify-red", "gate");
        inv.run.provider_result =
            Some(json!({ "ok": false, "exit": 101, "error": "tests failed" }));

        let out = fx(&inv).expect("runner returns a value");

        assert_eq!(out["effect_state"], json!("failed"), "{out}");
        let row = store
            .get_effect(&effect_key_for(&inv))
            .await
            .unwrap()
            .expect("row");
        assert_eq!(row.state, EffectState::Failed);
    }

    /// A verify invocation with NO observed result (outside the engine's
    /// verify path) is marked uncovered — journaling it would settle a
    /// command nobody ran.
    #[tokio::test]
    async fn verify_without_an_observed_result_is_marked_not_settled() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner = Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let fx = effect_runner(inner, store.clone(), None);
        let inv = invocation(verify(), "run-verify-nores", "gate");

        let out = fx(&inv).expect("runner returns a value");

        assert_eq!(
            out["effect_journal"],
            json!(toron_workflow::engine::EFFECT_JOURNAL_SINK_UNCOVERED),
            "no observed effect, no journal row: {out}"
        );
        assert!(
            store
                .get_effect(&effect_key_for(&inv))
                .await
                .unwrap()
                .is_none(),
            "no journal row for a command nobody ran"
        );
    }

    /// A journaled verify replays its stored outcome without re-running — the
    /// second dispatch must not re-execute the command.
    #[tokio::test]
    async fn terminal_verify_effect_replays_without_rerunning() {
        let (_dir, store) = test_store().await;
        let inner: ActionRunner = Arc::new(|a: &Action| Ok(json!({ "action": a.type_name() })));
        let fx = effect_runner(inner, store.clone(), None);
        let mut inv = invocation(verify(), "run-verify-replay", "gate");
        inv.run.provider_result = Some(json!({ "ok": true, "exit": 0 }));

        fx(&inv).expect("first");
        let second = fx(&inv).expect("replay");

        assert_eq!(
            second["ok"],
            json!(true),
            "the stored outcome replays: {second}"
        );
    }

    /// The pure classifier, pinned: in-band `ok:false` is classified by the
    /// same rule as a raised error, so neither shape can smuggle a failure
    /// into a `delivered` outcome.
    #[test]
    fn in_band_failure_values_are_classified_not_delivered() {
        assert!(matches!(
            provider_outcome(Ok(json!({ "ok": true, "count": 1 }))),
            ProviderOutcome::Delivered(_)
        ));
        assert!(matches!(
            provider_outcome(Ok(json!({ "ok": false, "error": "timeout after 30s" }))),
            ProviderOutcome::Ambiguous(_)
        ));
        assert!(matches!(
            provider_outcome(Ok(
                json!({ "ok": false, "error": "unknown profile @ghost" })
            )),
            ProviderOutcome::ProvenFailed(_)
        ));
        assert!(matches!(
            provider_outcome(Err(anyhow::anyhow!("pane gone"))),
            ProviderOutcome::ProvenFailed(_)
        ));
    }

    // ---- bd-mi9n: run-lifecycle rows through the injected journal ----

    fn transition(run_id: &str, workflow: &str, phase: RunPhase) -> RunTransition {
        RunTransition {
            run_id: run_id.to_string(),
            workflow: workflow.to_string(),
            workflow_digest: "digest-1".to_string(),
            phase,
        }
    }

    /// The engine's own lifecycle feed lands a run row: `Started` → `Running`
    /// → terminal, one row, ending `succeeded`.
    #[tokio::test]
    async fn run_journal_writes_started_running_succeeded() {
        let (_dir, store) = test_store().await;
        let journal = run_journal(store.clone());
        for phase in [RunPhase::Started, RunPhase::Running, RunPhase::Succeeded] {
            journal(&transition("run-life-1", "ship", phase));
        }
        let row = store
            .get_run("run-life-1")
            .await
            .expect("get_run")
            .expect("the engine-driven run has a row");
        assert_eq!(row.status, RunStatus::Succeeded, "terminal row: {row:?}");
        assert_eq!(row.workflow, "ship");
        assert_eq!(row.workflow_digest, "digest-1");
    }

    /// A failed run's row is terminal `failed` — never left `running`.
    #[tokio::test]
    async fn run_journal_writes_a_failed_terminal() {
        let (_dir, store) = test_store().await;
        let journal = run_journal(store.clone());
        for phase in [RunPhase::Started, RunPhase::Running, RunPhase::Failed] {
            journal(&transition("run-life-fail-1", "ship", phase));
        }
        let row = store
            .get_run("run-life-fail-1")
            .await
            .expect("get_run")
            .expect("row");
        assert_eq!(row.status, RunStatus::Failed, "{row:?}");
    }

    /// Replay safety: a durable run re-delivers phases, and a terminal row must
    /// never move back ("one ambiguity rule" would be a lie if the tail could
    /// resurrect a finished run).
    #[tokio::test]
    async fn run_journal_never_moves_a_terminal_row() {
        let (_dir, store) = test_store().await;
        let journal = run_journal(store.clone());
        for phase in [
            RunPhase::Started,
            RunPhase::Running,
            RunPhase::Succeeded,
            // the replay
            RunPhase::Started,
            RunPhase::Running,
        ] {
            journal(&transition("run-life-replay-1", "ship", phase));
        }
        let row = store
            .get_run("run-life-replay-1")
            .await
            .expect("get_run")
            .expect("row");
        assert_eq!(
            row.status,
            RunStatus::Succeeded,
            "a replayed phase must not move a terminal row: {row:?}"
        );
    }

    /// The run row and the effect rows agree on `run_id` — that agreement is
    /// the whole reason the engine (not the host) reports the lifecycle.
    #[tokio::test]
    async fn run_row_and_effect_rows_share_the_run_id() {
        let (_dir, store) = test_store().await;
        let journal = run_journal(store.clone());
        for phase in [RunPhase::Started, RunPhase::Running, RunPhase::Succeeded] {
            journal(&transition("run-life-2", "ship", phase));
        }
        // An effect row for the SAME run, through the same store.
        let inner: ActionRunner = Arc::new(|_a: &Action| Ok(json!({ "ok": true })));
        let fx = effect_runner(inner, store.clone(), None);
        let inv = invocation(dispatch_job(), "run-life-2", "ship");
        fx(&inv).expect("runner returns a value");

        let effect = store
            .get_effect(&effect_key_for(&inv))
            .await
            .expect("query")
            .expect("the effect row exists");
        let run = store
            .get_run("run-life-2")
            .await
            .expect("get_run")
            .expect("the run row exists");
        assert_eq!(
            effect.run_id, run.run_id,
            "the run row is the index its effect rows hang off — one run_id"
        );
        assert_eq!(run.status, RunStatus::Succeeded, "{run:?}");
    }
}

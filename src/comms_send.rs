// governed-by: ADR-0041 (toron), build-plan §5 (Phase 2)
//! `flywheel send --via-inbox` and the loop's `__send` on the comms inbox.
//!
//! One send is:
//!
//! 1. resolve every address to an npub at the CLI edge (`npub`, `pin`, or
//!    `pin@project`); an unknown or ambiguous name is a typed error and
//!    nothing is sent;
//! 2. build one `task@1` envelope per recipient ([`task_envelope`], pure);
//! 3. append it through toron's `mail.append` effect
//!    ([`toron_core::outbox::send_and_recover`]): the intent is journaled
//!    before the append, `Durability::Durable { key = msg_id }`, and an
//!    unknown outcome is reconciled through
//!    `kernel::plan_attempt(.., &outbox::MAIL_APPEND)`, never retried blind;
//! 4. ring the recipient's pane with the constant wake line only
//!    ([`crate::pane_write::PaneWrite::WakeLine`]). No brief byte reaches a
//!    pane; the reader pulls with `toron next`.
//!
//! Idempotency: the msg id for `(sender, task id, recipient)` is recorded in
//! flywheel's intent file before toron's journal sees it, so a retry with
//! the same task id re-sends the SAME msg id ([`plan_send`]), and the log
//! appends it once. Each run mints under an epoch claimed from the log
//! itself (`InboxStore::claim_epoch`, atomic, toron#19), so two runs, in one
//! process or many, never share an epoch and flywheel keeps no lock or epoch
//! file. A claim fences every lower epoch at once, so a run whose append is
//! refused as fenced (and was therefore never delivered) mints again under a
//! fresh claim.
//!
//! A different payload under an existing msg id is `PayloadConflict`
//! (toron#18): a hard error, never a replay.
//!
//! Default on. Opt out with `TORON_COMMS_INBOX=0` or `[comms] inbox_log = false`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
#[cfg(any(test, feature = "backend-superlogical"))]
use toron_core::backend::herdr::AgentInfo;
use toron_core::contracts::task_v1::TaskV1;
use toron_core::delivery::Reject;
use toron_core::envelope::{
    Class, ContextId, ContractId, ENVELOPE_V, Envelope, MsgId, Recipient, TaskId, is_npub_hex,
};
use toron_core::inbox::{InboxError, InboxStore};
use toron_core::outbox::{self, SendState};

use crate::config::Config;
use crate::pane_write::{PaneTyper, PaneWrite, Urgency, Wm};

/// The comms flag. Absent config and an unset env are on.
/// `inbox_log = false` opts out. A set `TORON_COMMS_INBOX` wins:
/// `1|true|on` on, `0|false|off` and any other value off.
pub fn enabled(cfg: &Config) -> bool {
    toron_core::inbox::enabled_with(cfg.comms.as_ref().and_then(|c| c.inbox_log))
}

/// Typed send failures. Every one of them means: nothing was appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    Disabled,
    BadAddress {
        token: String,
    },
    UnknownAddress {
        token: String,
        why: String,
    },
    AmbiguousAddress {
        token: String,
        why: String,
    },
    NoSender,
    /// The log (or the intent record) holds different content under this
    /// producer triple, or a task id was reused with a different brief.
    PayloadConflict {
        msg_key: String,
    },
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(
                f,
                "comms inbox is off: TORON_COMMS_INBOX=0 or [comms] inbox_log = false (default is on; nothing sent)"
            ),
            Self::BadAddress { token } => write!(
                f,
                "bad address {token:?}: use an npub (64 hex), pin, or pin@project (nothing sent)"
            ),
            Self::UnknownAddress { token, why } => {
                write!(f, "unknown address {token}: {why} (nothing sent)")
            }
            Self::AmbiguousAddress { token, why } => write!(
                f,
                "ambiguous address {token}: {why} — address it by npub or pin@project (nothing sent)"
            ),
            Self::NoSender => write!(
                f,
                "no sender: pass --from <pin|npub> or set FLYWHEEL_MAIL_AS / AGENT_NAME (nothing sent)"
            ),
            Self::PayloadConflict { msg_key } => write!(
                f,
                "payload conflict on {msg_key}: the log (or this task id's recorded brief) holds different content under this id; not replayed, not re-sent. Use a new --param task_id"
            ),
        }
    }
}

impl std::error::Error for SendError {}

/// A parsed address. Names are resolved to npubs once, at the edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    Npub(String),
    Pin {
        pin: String,
        project: Option<String>,
    },
}

impl Address {
    /// The pin, for a pane lookup (an npub has no pane name here).
    pub fn pin(&self) -> Option<&str> {
        match self {
            Self::Pin { pin, .. } => Some(pin),
            Self::Npub(_) => None,
        }
    }
}

fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Pure: parse one address token.
pub fn parse_address(token: &str) -> Result<Address, SendError> {
    let t = token.trim();
    if is_npub_hex(t) {
        return Ok(Address::Npub(t.to_string()));
    }
    let bad = || SendError::BadAddress {
        token: token.to_string(),
    };
    match t.split_once('@') {
        Some((pin, project)) if is_name(pin) && is_name(project) => Ok(Address::Pin {
            pin: pin.to_string(),
            project: Some(project.to_string()),
        }),
        Some(_) => Err(bad()),
        None if is_name(t) => Ok(Address::Pin {
            pin: t.to_string(),
            project: None,
        }),
        None => Err(bad()),
    }
}

/// Pure: classify toron's whois failure for a name.
pub fn classify_resolve_error(token: &str, why: &str) -> SendError {
    let w = why.to_ascii_lowercase();
    if w.contains("ambiguous") || w.contains("split") || w.contains("more than one key") {
        SendError::AmbiguousAddress {
            token: token.to_string(),
            why: why.to_string(),
        }
    } else {
        SendError::UnknownAddress {
            token: token.to_string(),
            why: why.to_string(),
        }
    }
}

/// Boundary: resolve one address to an npub through the mailbox roster.
pub async fn resolve(
    mb: &toron_core::mailbox::Mailbox,
    default_project: &str,
    token: &str,
) -> Result<String, SendError> {
    match parse_address(token)? {
        Address::Npub(n) => Ok(n),
        Address::Pin { pin, project } => {
            let p = project.as_deref().unwrap_or(default_project);
            mb.comms_npub(p, &pin)
                .await
                .map_err(|e| classify_resolve_error(token, &format!("{e:#}")))
        }
    }
}

/// Pure: one `task@1` envelope.
pub fn task_envelope(
    sender: &str,
    to: &str,
    msg: MsgId,
    context: &str,
    task_id: &str,
    task: &TaskV1,
) -> Result<Envelope, Reject> {
    let env = Envelope {
        v: ENVELOPE_V,
        msg,
        from: sender.to_string(),
        to: Recipient::Npub(to.to_string()),
        class: Class::Task,
        contract: ContractId::new("task", 1),
        context: ContextId(context.to_string()),
        task: Some(TaskId(task_id.to_string())),
        generation: 0,
        causation: None,
        deadline_ts_us: None,
        lease_ms: None,
        body: serde_json::to_value(task).map_err(|e| Reject::bad_envelope(e.to_string()))?,
    };
    env.validate()?;
    Ok(env)
}

/// Pure: a refusal text from the log that means "different content under
/// this triple" (toron#18). Never retried, never read as a replay.
pub fn is_payload_conflict(why: &str) -> bool {
    why.contains("payload conflict")
}

/// What to do for one recipient.
#[derive(Debug, Clone, PartialEq)]
pub enum SendPlan {
    /// The recorded envelope is already in the log: nothing to send.
    AlreadyAppended { env: Envelope, offset: u64 },
    /// The recorded envelope never landed and its epoch is still current:
    /// send the SAME msg id again (the log appends it once).
    Resend(Envelope),
    /// Nothing recorded, or the record can no longer land (stale epoch, so
    /// it was never delivered): mint a new msg id.
    Mint,
}

/// Pure: plan one recipient from the intent record and the log's answers.
pub fn plan_send(
    recorded: Option<Envelope>,
    recorded_offset: Option<u64>,
    store_epoch: Option<u64>,
) -> SendPlan {
    match (recorded, recorded_offset) {
        (Some(env), Some(offset)) => SendPlan::AlreadyAppended { env, offset },
        (Some(env), None) if store_epoch.is_none_or(|cur| env.msg.epoch >= cur) => {
            SendPlan::Resend(env)
        }
        _ => SendPlan::Mint,
    }
}

/// Pure: the intent-file key for `(task id, recipient)`.
pub fn intent_key(task_id: &str, recipient: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(task_id.as_bytes());
    h.update(b"\n");
    h.update(recipient.as_bytes());
    hex::encode(&h.finalize()[..12])
}

fn comms_dir(state_dir: &Path, sender: &str) -> PathBuf {
    state_dir.join("comms").join("outbox").join(sender)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// One recipient's result.
#[derive(Debug, Clone, Serialize)]
pub struct SentRow {
    pub to: String,
    pub npub: String,
    pub msg_id: String,
    /// `appended`, `already-appended`, `unknown`, or `refused: ...`.
    pub state: String,
    pub offset: Option<u64>,
    /// `rung`, `no-pane`, `skipped: ...`, or `failed: ...`.
    pub wake: String,
}

/// Rounds a send may take when a concurrent sender's epoch claim fences it
/// (the refused append never landed, so minting again cannot duplicate).
const MAX_ROUNDS: usize = 24;

/// One async mutex per sender npub, process-wide. Claim-to-settle is the
/// window in which a concurrent run of the SAME sender fences us (a claim
/// fences every lower epoch at once), so runs of one sender in one process
/// take turns instead of fencing each other. Across processes there is no
/// lock (and no lock file): contenders recover through the jittered
/// retry rounds.
fn sender_gate(sender: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static GATES: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut g = GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    g.entry(sender.to_string()).or_default().clone()
}

/// Pure: the backoff before retry round `round` (>= 1) given a random
/// draw: `1 + draw % (25 * round + 1)` ms. Every contender draws its own, so
/// they stop moving in step (a fixed `25 * round` kept them in lockstep and
/// let the rounds run out).
pub fn backoff_ms(round: usize, draw: u64) -> u64 {
    1 + draw % (25 * round as u64 + 1)
}

/// A fresh random draw per call (std's per-process random hasher state, mixed
/// with the clock and a counter; no extra dependency).
fn random_draw() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    h.finish()
}

fn run_id() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("flywheel:{}:{t}", std::process::id())
}

/// Boundary: re-append a recorded envelope that the log already holds. The
/// log answers `replay` for identical content and `PayloadConflict` for
/// different content; a fenced epoch cannot be re-appended, so it is
/// accepted on the lookup alone.
async fn verify_recorded(store: &InboxStore, env: &Envelope) -> Result<()> {
    match store.append_envelope(env).await {
        Err(InboxError::PayloadConflict { msg_key }) => {
            Err(SendError::PayloadConflict { msg_key }.into())
        }
        _ => Ok(()),
    }
}

/// Boundary: append one task to every recipient, exactly once per
/// `(sender, task id, recipient)`.
#[allow(clippy::too_many_arguments)]
pub async fn append_task(
    store: &InboxStore,
    state_dir: &Path,
    sender: &str,
    recipients: &[(String, String)],
    context: &str,
    task_id: &str,
    task: &TaskV1,
) -> Result<Vec<SentRow>> {
    let dir = comms_dir(state_dir, sender);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let run = run_id();
    let gate = sender_gate(sender);
    let body = serde_json::to_value(task)?;

    // 1. Reconcile whatever a crashed earlier run journaled (toron's
    //    mail.append recovery): present -> settled, absent -> appended with
    //    the SAME triple.
    //
    //    The recovery takes the sender's turn (bd-3s2q). The outbox CLAIMS a
    //    journal epoch, and a claim fences every lower epoch at once, so a
    //    recovery outside the turn fenced a live peer's in-flight append:
    //    "stale epoch for producer <npub>#journal: sent 12, current 13".
    //    One sender, one turn: the claim and the appends it fences are the
    //    same critical section, so a run's own epoch is never fenced by a
    //    peer that is still writing under it.
    {
        let _turn = gate.lock().await;
        outbox::send_and_recover(store.clone(), sender.to_string(), Vec::new())
            .await
            .map_err(|e| anyhow!("recover sender intents: {e}"))?;
    }

    // (msg key, state, offset) once a recipient is settled; `last_why` keeps
    // the latest refusal for a recipient still pending.
    type Done = (String, String, Option<u64>);
    let mut done: Vec<Option<Done>> = vec![None; recipients.len()];
    let mut last_why: Vec<Option<(String, String)>> = vec![None; recipients.len()];
    let paths: Vec<PathBuf> = recipients
        .iter()
        .map(|(_, npub)| dir.join(format!("{}.json", intent_key(task_id, npub))))
        .collect();

    for round in 0..MAX_ROUNDS {
        if done.iter().all(Option::is_some) {
            break;
        }
        if round > 0 {
            // Fenced by a concurrent sender of this npub (another process,
            // or the toron journal-epoch race): back off with jitter, then
            // re-plan.
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms(
                round,
                random_draw(),
            )))
            .await;
        }
        // Claim-to-settle is one critical section per sender in this process.
        let _turn = gate.lock().await;
        // 2. Plan each pending recipient from its intent record and the log.
        let store_epoch = store.producer_epoch(sender).await?;
        let mut resend: Vec<(usize, Envelope)> = Vec::new();
        let mut mint: Vec<usize> = Vec::new();
        for i in 0..recipients.len() {
            if done[i].is_some() {
                continue;
            }
            let recorded: Option<Envelope> = std::fs::read_to_string(&paths[i])
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok());
            if let Some(env) = &recorded
                && (env.body != body || env.context.0 != context)
            {
                // The same task id with a different brief is not a retry.
                return Err(SendError::PayloadConflict {
                    msg_key: env.msg.key(),
                }
                .into());
            }
            let offset = match &recorded {
                Some(env) => store.lookup(&env.msg).await?,
                None => None,
            };
            match plan_send(recorded, offset, store_epoch) {
                SendPlan::AlreadyAppended { env, offset } => {
                    verify_recorded(store, &env).await?;
                    done[i] = Some((env.msg.key(), "already-appended".into(), Some(offset)));
                }
                SendPlan::Resend(env) => resend.push((i, env)),
                SendPlan::Mint => mint.push(i),
            }
        }

        // 3a. Resends first, under their recorded epochs, lower epoch first
        //     (the log fences an epoch below its current one). Resending
        //     BEFORE claiming keeps the claim from fencing them.
        resend.sort_by_key(|(_, e)| (e.msg.epoch, e.msg.seq));
        // 3b. Mint under an epoch claimed from the log; record each intent
        //     BEFORE the send (the record is what makes a retry re-send the
        //     same msg id). An unused epoch is simply skipped.
        let mut fresh: Vec<(usize, Envelope)> = Vec::new();
        if !mint.is_empty() {
            // Resends settle before the claim fences their epochs.
            settle_batch(store, sender, &resend, &paths, &mut done, &mut last_why).await?;
            let claim = store
                .claim_epoch(sender, &run)
                .await
                .context("claim an epoch")?;
            for (seq, i) in mint.iter().enumerate() {
                let msg = MsgId {
                    producer: sender.to_string(),
                    epoch: claim.epoch,
                    seq: seq as u64,
                };
                let env = task_envelope(sender, &recipients[*i].1, msg, context, task_id, task)
                    .map_err(|r| anyhow!("task@1 refused: {r}"))?;
                write_atomic(&paths[*i], serde_json::to_string(&env)?.as_bytes())?;
                fresh.push((*i, env));
            }
            settle_batch(store, sender, &fresh, &paths, &mut done, &mut last_why).await?;
        } else {
            settle_batch(store, sender, &resend, &paths, &mut done, &mut last_why).await?;
        }
    }

    Ok(recipients
        .iter()
        .enumerate()
        .map(|(i, (to, npub))| {
            let (msg_id, state, offset) = done[i].clone().unwrap_or_else(|| {
                let (key, why) = last_why[i].clone().unwrap_or_default();
                (
                    key,
                    format!("refused: fenced out after {MAX_ROUNDS} rounds: {why}"),
                    None,
                )
            });
            SentRow {
                to: to.clone(),
                npub: npub.clone(),
                msg_id,
                state,
                offset,
                wake: String::new(),
            }
        })
        .collect())
}

/// Boundary: send one batch through `mail.append` and fold the outcomes
/// into `done`. A refusal that is not a payload conflict means the append
/// never landed (fenced by a newer claim, or a seq gap): the intent is
/// dropped so the next round mints fresh. A payload conflict is a hard
/// error.
async fn settle_batch(
    store: &InboxStore,
    sender: &str,
    batch: &[(usize, Envelope)],
    paths: &[PathBuf],
    done: &mut [Option<(String, String, Option<u64>)>],
    last_why: &mut [Option<(String, String)>],
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let report = outbox::send_and_recover(
        store.clone(),
        sender.to_string(),
        batch.iter().map(|(_, e)| e.clone()).collect(),
    )
    .await
    .map_err(|e| anyhow!("mail.append: {e}"))?;
    for (i, env) in batch {
        let key = env.msg.key();
        match report.sent.get(&key) {
            Some(SendState::Refused(why)) if is_payload_conflict(why) => {
                return Err(SendError::PayloadConflict { msg_key: key }.into());
            }
            Some(SendState::Refused(why)) => {
                let _ = std::fs::remove_file(&paths[*i]);
                last_why[*i] = Some((key, why.clone()));
            }
            Some(SendState::Settled(o)) => {
                done[*i] = Some((key, "appended".into(), Some(*o)));
            }
            // In flight / unknown / not reported: the append may not have
            // run (the journal refused the intent under a racing sender).
            // Ask the log, never guess: present -> settled; absent -> the
            // intent record stays and the next round re-sends the SAME msg
            // id (the log dedupes by triple) or mints if it was fenced.
            Some(_) | None => match store.lookup(&env.msg).await? {
                Some(o) => done[*i] = Some((key, "appended".into(), Some(o))),
                None => last_why[*i] = Some((key, "append not confirmed".into())),
            },
        }
    }
    Ok(())
}

/// What the sender does for one recipient pane. The harness record decides
/// whether a ring is possible at all (bd-harness-caps-6r1x): typing into a
/// harness with no injection door is a wake nobody receives, so the send row
/// says so instead of reporting a ring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SendWake {
    Ring,
    /// Nothing was typed. The note lands in the send result row and names
    /// the fallback.
    Skip(String),
}

/// The one policy the sender asks. Pure; `hook_installed` is the caller's
/// reading of the operator hook file.
pub(crate) fn send_wake(kind: &str, hook_installed: bool) -> SendWake {
    match crate::harness_caps::wake_route(kind, hook_installed) {
        crate::harness_caps::WakeRoute::Inject { .. } => SendWake::Ring,
        crate::harness_caps::WakeRoute::NoDoor(no_door) => {
            SendWake::Skip(crate::harness_caps::no_door_line(&no_door))
        }
    }
}

/// Boundary: ring one recipient with the constant wake line for its head.
/// A failed ring never re-sends the envelope; the next ring for the same
/// head is the same line.
pub async fn ring(
    store: &InboxStore,
    panes: &dyn PaneTyper,
    target: &str,
    npub: &str,
) -> Result<Wm> {
    let wm = Wm(store.head(npub).await?);
    crate::pane_write::write(
        panes,
        target,
        &PaneWrite::WakeLine {
            wm,
            urgency: Urgency::Normal,
        },
    )?;
    Ok(wm)
}

/// The in-process comms store over the shared durable root.
pub fn store() -> Result<InboxStore> {
    let mb = crate::mail::comms_mailbox()?;
    let pool = mb
        .index_pool()
        .cloned()
        .ok_or_else(|| anyhow!("comms inbox needs the index pool"))?;
    Ok(InboxStore::new(pool))
}

/// The sender pin: `--as`, else `AGENT_NAME`. The same two rungs as the guard.
pub fn sender_token(
    from: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String, SendError> {
    crate::authority::select_agent_identity(from, env).ok_or(SendError::NoSender)
}

/// One full send from sync code (CLI and the `__send` leaf).
pub struct Request<'a> {
    pub project: &'a str,
    pub from: Option<&'a str>,
    pub to: &'a str,
    pub task_id: &'a str,
    pub task: TaskV1,
}

/// Resolve, append once, ring. Returns the rows (one per recipient).
pub fn send(cfg: &Config, req: Request<'_>) -> Result<Vec<SentRow>> {
    if !enabled(cfg) {
        return Err(SendError::Disabled.into());
    }
    let tokens: Vec<String> = req
        .to
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if tokens.is_empty() {
        return Err(SendError::BadAddress {
            token: req.to.to_string(),
        }
        .into());
    }
    // Parse everything before any I/O: a bad token sends nothing.
    let parsed: Vec<Address> = tokens
        .iter()
        .map(|t| parse_address(t))
        .collect::<Result<_, _>>()?;
    let sender_tok = sender_token(req.from, &|k| std::env::var(k).ok())?;
    let store = store()?;
    let mb = crate::mail::mailbox();
    let project = req.project.to_string();
    let (sender, recipients) = crate::mail::block_on(async {
        let sender = resolve(mb, &project, &sender_tok).await?;
        let mut out = Vec::new();
        for t in &tokens {
            out.push((t.clone(), resolve(mb, &project, t).await?));
        }
        Ok::<_, SendError>((sender, out))
    })?;
    let state_dir = crate::config::state_dir();
    let mut rows = crate::mail::block_on(append_task(
        &store,
        &state_dir,
        &sender,
        &recipients,
        &project,
        req.task_id,
        &req.task,
    ))?;
    // Wake: the constant line only, to the recipient's pane. Off herdr the
    // typer and the target resolution follow the selected backend (same
    // constant line, same no-host-rings rule); herdr keeps the path below.
    // (Feature-gated: without the rex backend this selection cannot ring
    // anywhere else, so the herdr path below stays the behavior.)
    #[cfg(feature = "backend-superlogical")]
    if crate::backend::resolve_backend_name(cfg) != "herdr" {
        for (row, addr) in rows.iter_mut().zip(parsed.iter()) {
            row.wake = match addr.pin() {
                None => "no-pane (npub address; Phase 3 rings by binding)".into(),
                Some(pin) => wake_via_backend(cfg, &store, &project, pin, &row.npub),
            };
        }
        return Ok(rows);
    }
    let mut herdr = crate::herdr::Client::connect(Some(&cfg.socket_path()));
    for (row, addr) in rows.iter_mut().zip(parsed.iter()) {
        row.wake = match (&mut herdr, addr.pin()) {
            (_, None) => "no-pane (npub address; Phase 3 rings by binding)".into(),
            (Err(e), _) => format!("skipped: herdr unavailable ({e})"),
            (Ok(c), Some(pin)) => match wake_target(c, &project, pin) {
                Err(e) => format!("no-pane ({e:#})"),
                // The doorbell's inbox host rings level-triggered with the ring
                // budget and the deaf escalation. A sender ring on top of it
                // would be a second, unbudgeted writer for the same mail.
                Ok(target) => {
                    let ws = crate::find_workspace_info(c, &project).ok();
                    if doorbell_owner(&project, ws.as_ref()).is_some() {
                        "left to the doorbell host (budgeted, level-triggered)".to_string()
                    } else {
                        let kind = crate::state::load_spawn_registry(&project)
                            .into_iter()
                            .find(|e| e.mail_identity.as_deref() == Some(pin))
                            .and_then(|e| e.harness)
                            .unwrap_or_default();
                        match send_wake(&kind, crate::harness_caps::installed_from_env()) {
                            SendWake::Skip(note) => note,
                            SendWake::Ring => {
                                let panes = crate::pane_write::HerdrPanes(c);
                                match crate::mail::block_on(ring(
                                    &store, &panes, &target, &row.npub,
                                )) {
                                    Ok(wm) => format!("rung wm {:x}", wm.0),
                                    Err(e) => format!("failed: {e:#}"),
                                }
                            }
                        }
                    }
                }
            },
        };
    }
    Ok(rows)
}

/// A live doorbell owner for this project's workspace (embedded or daemon
/// host), by its lease: the writer that rings with a budget. `None` = nobody
/// is hosting, so the sender's own ring is the only wake there is.
pub fn doorbell_owner(
    project: &str,
    workspace: Option<&crate::herdr::WorkspaceInfo>,
) -> Option<u32> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    owner_of_workspace(&crate::config::state_dir(), project, workspace, now)
}

/// The sender's lookup for a herdr workspace: by the key its doorbell host
/// was spawned under ([`host_key`]), never by the herdr workspace id.
fn owner_of_workspace(
    state_dir: &std::path::Path,
    project: &str,
    workspace: Option<&crate::herdr::WorkspaceInfo>,
    now: u64,
) -> Option<u32> {
    doorbell_owner_in(state_dir, project, workspace.map(host_key), now)
}

/// A doorbell host runs as `__doorbell --workspace <label>`: its pid file,
/// owner lease and inbox lease ([`crate::doorbell::inbox_owner_key`]) all
/// carry the herdr workspace LABEL (prod: `owner-inbox-flywheel-flywheel.json`).
fn host_key(ws: &crate::herdr::WorkspaceInfo) -> &str {
    &ws.label
}

fn doorbell_owner_in(
    state_dir: &std::path::Path,
    project: &str,
    workspace: Option<&str>,
    now: u64,
) -> Option<u32> {
    // A lease counts only if its holder RINGS: a daemon host under the comms
    // flag holds the workspace lease but types nothing (kind `no-text`), so
    // deferring to it left the workspace silent. The inbox host's own lease
    // (`inbox-<ws>`) is the ringer there (follow-up a).
    let inbox = workspace.map(crate::doorbell::inbox_owner_key);
    doorbell_rt::lease::live_leases(state_dir, now)
        .into_iter()
        .find(|(ws, l)| {
            (ws == project || Some(ws.as_str()) == workspace) && doorbell_rt::lease::is_ringer(l)
                || Some(ws) == inbox.as_ref()
        })
        .map(|(_, l)| l.pid)
}

/// Pure: the wake target pane for a pin from a backend listing. Mirrors
/// `wake_target`'s exactly-one-or-typed-error contract: a pin names its
/// block label (set by spawn/rename) or its agent name, never a substring.
// Shared with the default send path's rex arm (`resolve_send_targets_dyn`),
// which needs the same rule, not a copy.
#[cfg(feature = "backend-superlogical")]
pub(crate) fn backend_wake_target(agents: &[AgentInfo], pin: &str) -> Result<String, String> {
    let mut hits = agents
        .iter()
        .filter(|a| a.name.as_deref() == Some(pin) || a.display_agent.as_deref() == Some(pin));
    match (hits.next(), hits.next()) {
        (Some(a), None) => Ok(a.pane_id.clone()),
        (None, _) => Err(format!("{pin} has no pane")),
        (Some(_), Some(_)) => Err(format!("{pin} matches multiple panes; not ringing")),
    }
}

/// Sender-side wake through the selected backend: resolve the pin to its
/// block, gate on the harness door, and ring the constant line through the
/// rex typer. String in, string out, matching the herdr arm's report
/// shapes (`rung wm …`, skip notes, `no-pane …`, `failed: …`).
#[cfg(feature = "backend-superlogical")]
fn wake_via_backend(
    cfg: &Config,
    store: &InboxStore,
    project: &str,
    pin: &str,
    npub: &str,
) -> String {
    let backend = match crate::backend::multiplexer_backend(cfg) {
        Ok(b) => b,
        Err(e) => return format!("skipped: backend unavailable ({e})"),
    };
    let target = match backend
        .agent_list()
        .map_err(|e| format!("no-pane ({e:#})"))
        .and_then(|agents| backend_wake_target(&agents, pin))
    {
        Ok(t) => t,
        Err(e) => return e,
    };
    let kind = crate::state::load_spawn_registry(project)
        .into_iter()
        .find(|e| e.mail_identity.as_deref() == Some(pin))
        .and_then(|e| e.harness)
        .unwrap_or_default();
    match send_wake(&kind, crate::harness_caps::installed_from_env()) {
        SendWake::Skip(note) => note,
        SendWake::Ring => {
            let panes = crate::pane_write::RexPanes(&*backend);
            match crate::mail::block_on(ring(store, &panes, &target, npub)) {
                Ok(wm) => format!("rung wm {:x}", wm.0),
                Err(e) => format!("failed: {e:#}"),
            }
        }
    }
}

fn wake_target(c: &mut crate::herdr::Client, project: &str, pin: &str) -> Result<String> {
    let ws = crate::find_workspace(c, project)?;
    let agents = crate::resolve_agents(c, &ws, project, pin)?;
    match agents.as_slice() {
        [a] => Ok(crate::agent_target(a).to_string()),
        [] => Err(anyhow!("{pin} has no pane in {project}")),
        many => Err(anyhow!("{pin} matches {} panes; not ringing", many.len())),
    }
}

/// The rows as the CLI's JSON.
pub fn rows_json(rows: &[SentRow]) -> Value {
    json!({ "via": "inbox", "sent": rows })
}

/// Pure: the `task@1` body for a free-text send (title = first line).
pub fn task_from_text(text: &str, bead: Option<&str>, scope_paths: Vec<String>) -> TaskV1 {
    let title: String = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("task")
        .chars()
        .take(120)
        .collect();
    TaskV1 {
        title,
        brief: text.to_string(),
        scope_paths,
        acceptance: Vec::new(),
        bead: bead.map(str::to_string),
    }
}

/// Pure: the deterministic task id of one loop send (so a re-executed leaf
/// re-sends the same msg id). The text digest keeps two different prompts
/// of one iteration distinct.
pub fn loop_task_id(invocation: &str, iteration: u64, name: &str, text: &str) -> String {
    use sha2::Digest;
    let d = hex::encode(&sha2::Sha256::digest(text.as_bytes())[..4]);
    format!("loop-{invocation}-{iteration}-{name}-{d}")
}

/// The loop's `__send` on the inbox (build-plan §5): the same `mail.append`
/// effect as `send --via-inbox`, no subprocess typing, no resend. Returns
/// the leaf's `SendOutcome` JSON: `landed` is the envelope fact (appended
/// once), `deduped` when a re-executed leaf found its envelope already in
/// the log.
pub fn loop_send(
    cfg: &Config,
    project: &str,
    name: &str,
    text: &str,
    task_id: &str,
) -> Result<Value> {
    // The loop names its target by herdr agent name; the inbox is keyed by
    // the pane's mail pin (spawn registry), so map name -> pin first.
    let registry = crate::state::load_spawn_registry(project);
    let pin = crate::registry_entry_for_token(&registry, project, name)
        .and_then(|e| e.mail_identity.clone())
        .unwrap_or_else(|| name.to_string());
    let rows = send(
        cfg,
        Request {
            project,
            from: None,
            to: &pin,
            task_id,
            task: task_from_text(text, None, Vec::new()),
        },
    )?;
    let row = rows.first().ok_or_else(|| anyhow!("no recipient row"))?;
    let landed = row.state == "appended" || row.state == "already-appended";
    Ok(json!({
        "ok": landed,
        "name": name,
        "landed": landed,
        "status": format!("inbox:{}", row.state),
        "reason": format!("msg {} offset {:?}; wake {}", row.msg_id, row.offset, row.wake),
        "deduped": row.state == "already-appended",
    }))
}

#[cfg(test)]
mod tests {
    #[test]
    fn backoff_is_jittered_bounded_and_never_zero() {
        for round in 1..=MAX_ROUNDS {
            let mut seen = std::collections::BTreeSet::new();
            for d in 0..2000u64 {
                let ms = backoff_ms(round, d);
                assert!((1..=25 * round as u64 + 1).contains(&ms), "{ms} @ {round}");
                seen.insert(ms);
            }
            // Contenders must be able to land on different delays.
            assert!(seen.len() > 1);
        }
        assert_ne!(random_draw(), random_draw());
    }

    #[tokio::test]
    async fn one_sender_one_gate_distinct_senders_distinct_gates() {
        assert!(std::sync::Arc::ptr_eq(
            &sender_gate("npub-a"),
            &sender_gate("npub-a")
        ));
        assert!(!std::sync::Arc::ptr_eq(
            &sender_gate("npub-a"),
            &sender_gate("npub-b")
        ));
        let g = sender_gate("npub-a");
        let _held = g.lock().await;
        assert!(
            sender_gate("npub-a").try_lock().is_err(),
            "same sender takes turns"
        );
        assert!(
            sender_gate("npub-b").try_lock().is_ok(),
            "other senders do not wait"
        );
    }

    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn task() -> TaskV1 {
        task_from_text("Fix the doorbell\nsecret brief body", Some("bd-x"), vec![])
    }

    fn msg(epoch: u64, seq: u64) -> MsgId {
        MsgId {
            producer: A.into(),
            epoch,
            seq,
        }
    }

    #[test]
    fn addresses_parse_to_npub_pin_or_pin_at_project() {
        assert_eq!(parse_address(A), Ok(Address::Npub(A.into())));
        assert_eq!(
            parse_address("AmberFixture@comms-p2-fixture"),
            Ok(Address::Pin {
                pin: "AmberFixture".into(),
                project: Some("comms-p2-fixture".into())
            })
        );
        assert_eq!(
            parse_address("AmberFixture"),
            Ok(Address::Pin {
                pin: "AmberFixture".into(),
                project: None
            })
        );
        for bad in ["", "a b", "x@", "@p", "a@b@c", "pin;rm"] {
            assert!(
                matches!(parse_address(bad), Err(SendError::BadAddress { .. })),
                "{bad:?}"
            );
        }
        assert!(matches!(
            classify_resolve_error("Amber", "pin Amber is split: more than one key"),
            SendError::AmbiguousAddress { .. }
        ));
        assert!(matches!(
            classify_resolve_error("Nobody", "unknown agent name: Nobody"),
            SendError::UnknownAddress { .. }
        ));
    }

    #[test]
    fn task_envelope_is_npub_addressed_and_valid() {
        let env = task_envelope(A, B, msg(3, 0), "comms-p2-fixture", "t-1", &task()).unwrap();
        assert_eq!(env.to, Recipient::Npub(B.into()));
        assert_eq!(env.contract, ContractId::new("task", 1));
        assert_eq!(env.msg.key(), format!("{A}:3:0"));
        // A pin name in `to` is refused by the envelope itself.
        assert!(task_envelope(A, "AmberFixture", msg(3, 0), "c", "t", &task()).is_err());
        // An empty brief is not a task.
        let empty = TaskV1 {
            brief: " ".into(),
            ..task()
        };
        assert!(task_envelope(A, B, msg(3, 0), "c", "t", &empty).is_err());
    }

    #[test]
    fn a_retry_resends_the_recorded_msg_id_and_never_mints_twice() {
        let env = task_envelope(A, B, msg(2, 0), "c", "t", &task()).unwrap();
        assert_eq!(
            plan_send(Some(env.clone()), Some(7), Some(2)),
            SendPlan::AlreadyAppended {
                env: env.clone(),
                offset: 7
            }
        );
        assert_eq!(
            plan_send(Some(env.clone()), None, Some(2)),
            SendPlan::Resend(env.clone())
        );
        assert_eq!(
            plan_send(Some(env.clone()), None, None),
            SendPlan::Resend(env.clone())
        );
        // Stale: the log moved past its epoch, so it never landed: mint.
        assert_eq!(plan_send(Some(env), None, Some(3)), SendPlan::Mint);
        assert_eq!(plan_send(None, None, Some(3)), SendPlan::Mint);
        assert_eq!(intent_key("t", B), intent_key("t", B));
        assert_ne!(intent_key("t", B), intent_key("t", A));
    }

    #[test]
    fn mail_append_reconciles_and_never_blind_retries() {
        use toron_core::kernel::{DispatchPlan, EffectState, EffectView, plan_attempt};
        for state in [
            EffectState::InFlight,
            EffectState::Unknown,
            EffectState::Accepted,
        ] {
            let view = EffectView {
                state,
                attempt: 1,
                lease_live: false,
            };
            let plan = plan_attempt(Some(&view), &outbox::MAIL_APPEND);
            assert!(
                !matches!(plan, DispatchPlan::RefuseBlindRetry { .. }),
                "{state:?} -> {plan:?}"
            );
        }
    }

    // ---- the effect against the real store (in-memory sqlite) ----

    async fn mem_store() -> InboxStore {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = dir.join("index.sqlite");
        let pool = toron_core::index::schema::connect(&db.to_string_lossy())
            .await
            .unwrap();
        toron_core::index::schema::migrate(&pool).await.unwrap();
        InboxStore::new(pool)
    }

    async fn rows_for(store: &InboxStore, npub: &str) -> u64 {
        store.head(npub).await.unwrap()
    }

    struct Recorder(std::sync::Mutex<Vec<String>>, bool);
    impl PaneTyper for Recorder {
        fn prompt(&self, target: &str, line: &str) -> Result<()> {
            self.0.lock().unwrap().push(format!("{target} {line}"));
            if self.1 {
                Err(anyhow!("herdr stall"))
            } else {
                Ok(())
            }
        }
        fn keys(&self, _: &str, _: &[&str]) -> Result<()> {
            Ok(())
        }
        fn close(&self, _: &str) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_task_appends_once_and_a_retry_replays_it() {
        let store = mem_store().await;
        let dir = tempfile::tempdir().unwrap();
        let rcpt = vec![("Birch".to_string(), B.to_string())];
        let first = append_task(&store, dir.path(), A, &rcpt, "c", "t-1", &task())
            .await
            .unwrap();
        assert_eq!(first[0].state, "appended");
        let head = rows_for(&store, B).await;
        assert!(head > 0);
        // Retry of the same task (e.g. after a crash): the recorded msg id is
        // found in the log; no second row.
        let again = append_task(&store, dir.path(), A, &rcpt, "c", "t-1", &task())
            .await
            .unwrap();
        assert_eq!(again[0].state, "already-appended");
        assert_eq!(again[0].msg_id, first[0].msg_id);
        assert_eq!(again[0].offset, first[0].offset);
        assert_eq!(rows_for(&store, B).await, head, "exactly one envelope");
        // A different task is a different message, under a fresh epoch.
        let other = append_task(&store, dir.path(), A, &rcpt, "c", "t-2", &task())
            .await
            .unwrap();
        assert_eq!(other[0].state, "appended");
        assert_ne!(other[0].msg_id, first[0].msg_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_cursor_the_host_reads_is_the_one_toron_next_commits() {
        let store = mem_store().await;
        let dir = tempfile::tempdir().unwrap();
        let rcpt = vec![("Birch".to_string(), B.to_string())];
        append_task(&store, dir.path(), A, &rcpt, "c", "t-1", &task())
            .await
            .unwrap();
        let (head, cursor) = crate::inbox_ports::head_and_cursor(&store, B).await;
        assert!(
            head > 0 && cursor == 0,
            "unread mail: head {head} > cursor 0"
        );
        // The agent runs `toron next`: the cursor commits to the head.
        let got = store.next(B, None, false).await.unwrap();
        assert_eq!(got.items.len(), 1);
        let (head2, cursor2) = crate::inbox_ports::head_and_cursor(&store, B).await;
        assert_eq!(
            (head2, cursor2),
            (head, head),
            "the ack is seen: cursor == head"
        );
        // The wrong key (the log's name) never sees it: the bug this pins.
        assert_eq!(
            store
                .cursor(B, toron_core::inbox::INBOX_STREAM)
                .await
                .unwrap(),
            0
        );
    }

    #[test]
    fn a_live_doorbell_lease_means_the_sender_does_not_ring() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_000_000;
        assert_eq!(doorbell_owner_in(dir.path(), "proj", None, now), None);
        let me = std::process::id();
        let lease = doorbell_rt::lease::acquire_lease(dir.path(), "proj", now).unwrap();
        assert_eq!(lease.pid, me);
        assert_eq!(doorbell_owner_in(dir.path(), "proj", None, now), Some(me));
        // Another project's owner is not this project's writer.
        assert_eq!(doorbell_owner_in(dir.path(), "other", None, now), None);
        // The workspace name (a label that differs from the project) counts.
        assert_eq!(
            doorbell_owner_in(dir.path(), "x", Some("proj"), now),
            Some(me)
        );
        // A wedged heartbeat is no owner: the sender falls back to ringing.
        assert_eq!(
            doorbell_owner_in(dir.path(), "proj", None, now + 1000),
            None
        );
    }

    /// Follow-up (a), the regression: a LIVE lease held by a daemon host that
    /// types nothing (comms flag) must not silence the sender; the inbox
    /// host's own lease is the ringer there.
    #[test]
    fn a_live_lease_held_by_a_no_text_daemon_does_not_silence_the_sender() {
        use doorbell_rt::lease::{KIND_NO_TEXT, acquire_lease_kind, inbox_lease_key};
        let dir = tempfile::tempdir().unwrap();
        let now = doorbell_rt::lease::unix_now();
        // The daemon (a live process: this one) holds the workspace, typing nothing.
        let held = acquire_lease_kind(dir.path(), "proj", now, Some(KIND_NO_TEXT)).unwrap();
        assert_eq!(held.pid, std::process::id(), "the holder is alive");
        assert_eq!(
            doorbell_owner_in(dir.path(), "proj", Some("proj"), now),
            None,
            "sender rings itself"
        );
        // The inbox host comes up beside it: now there IS a ringer to defer to.
        acquire_lease_kind(dir.path(), &inbox_lease_key("proj"), now, None).unwrap();
        assert_eq!(
            doorbell_owner_in(dir.path(), "proj", Some("proj"), now),
            Some(std::process::id())
        );
        // A different workspace's inbox lease is not this one's.
        assert_eq!(
            doorbell_owner_in(dir.path(), "other", Some("other"), now),
            None
        );
    }

    fn herdr_ws(id: &str, label: &str) -> crate::herdr::WorkspaceInfo {
        serde_json::from_value(json!({
            "workspace_id": id, "number": 1, "label": label, "focused": false,
            "pane_count": 1, "tab_count": 1, "active_tab_id": "t1", "agent_status": "idle"
        }))
        .expect("a herdr workspace row")
    }

    /// rv23 S1/S2, the regression: the inbox host is spawned under the herdr
    /// workspace LABEL and leases `inbox-<label>`; the sender resolved the
    /// workspace ID (`w1`) and looked up `inbox-w1`, found nobody, and rang
    /// the pane itself on every send (two wakes per send, plus one to an
    /// agent with no pane). The label differs from the id AND the project
    /// here, so no coincidence can make the lookup pass.
    #[test]
    fn the_sender_finds_the_inbox_host_by_the_workspace_label_not_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let now = doorbell_rt::lease::unix_now();
        let ws = herdr_ws("w1", "flywheel-proj");
        assert_eq!(owner_of_workspace(dir.path(), "proj", Some(&ws), now), None);
        // Exactly what `run_inbox_only(cfg, "proj", "flywheel-proj")` leases.
        doorbell_rt::lease::acquire_lease(
            dir.path(),
            &crate::doorbell::inbox_owner_key("flywheel-proj"),
            now,
        )
        .unwrap();
        assert_eq!(
            owner_of_workspace(dir.path(), "proj", Some(&ws), now),
            Some(std::process::id()),
            "the sender must defer to the live inbox host: one wake per send"
        );
        // Another workspace's host is not this one's, even with the same id.
        let other = herdr_ws("w1", "flywheel-other");
        assert_eq!(
            owner_of_workspace(dir.path(), "other", Some(&other), now),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_recorded_intent_that_never_landed_is_sent_with_the_same_msg_id() {
        let store = mem_store().await;
        let dir = tempfile::tempdir().unwrap();
        // Simulate a crash after flywheel recorded the intent, before any
        // append: the record exists, the log does not have it.
        let claim = store.claim_epoch(A, "crashed-run").await.unwrap();
        assert_eq!(claim.epoch, 0);
        let env = task_envelope(A, B, msg(0, 0), "c", "t-9", &task()).unwrap();
        let path = comms_dir(dir.path(), A).join(format!("{}.json", intent_key("t-9", B)));
        write_atomic(&path, serde_json::to_string(&env).unwrap().as_bytes()).unwrap();
        let rcpt = vec![("Birch".to_string(), B.to_string())];
        let rows = append_task(&store, dir.path(), A, &rcpt, "c", "t-9", &task())
            .await
            .unwrap();
        assert_eq!(rows[0].msg_id, env.msg.key(), "same msg id, not a new one");
        assert_eq!(rows[0].state, "appended");
        assert_eq!(store.lookup(&env.msg).await.unwrap(), rows[0].offset);
    }

    /// bd-harness-caps-6r1x + bd-9tkm: the sender asks the harness record
    /// before typing. A row that records no door is skipped with a note; an
    /// unrecorded kind keeps its door, because the sink types into any herdr
    /// pane and legacy registry rows carry no harness field at all.
    #[test]
    fn a_sender_rings_a_herdr_pane_and_skips_a_row_with_no_door() {
        for kind in ["opencode", "oc", "fake", "claude", "codex", "", "widget"] {
            assert_eq!(send_wake(kind, true), SendWake::Ring, "kind {kind:?}");
        }
        let refusing = crate::harness_caps::HarnessCaps {
            inject: crate::harness_caps::Capability::unsupported(
                crate::harness_caps::Fallback::Mail,
            ),
            ..*crate::harness_caps::row("opencode").expect("opencode row")
        };
        let note = match crate::harness_caps::route_for(Some(&refusing), "opencode", false) {
            crate::harness_caps::WakeRoute::NoDoor(no_door) => {
                crate::harness_caps::no_door_line(&no_door)
            }
            other => panic!("a refusing row routed {other:?}"),
        };
        assert!(note.contains("fall back to mail"), "{note}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_wake_never_sends_a_second_envelope_and_rerings_the_same_line() {
        let store = mem_store().await;
        let dir = tempfile::tempdir().unwrap();
        let rcpt = vec![("Birch".to_string(), B.to_string())];
        append_task(&store, dir.path(), A, &rcpt, "c", "t-1", &task())
            .await
            .unwrap();
        let head = rows_for(&store, B).await;
        let stalled = Recorder(Default::default(), true);
        assert!(ring(&store, &stalled, "w1:p2", B).await.is_err());
        let ok = Recorder(Default::default(), false);
        ring(&store, &ok, "w1:p2", B).await.unwrap();
        assert_eq!(rows_for(&store, B).await, head, "a ring never appends");
        let lines: Vec<String> = stalled
            .0
            .lock()
            .unwrap()
            .iter()
            .chain(ok.0.lock().unwrap().iter())
            .cloned()
            .collect();
        assert_eq!(lines[0], lines[1], "the re-ring is the same line");
        assert!(lines[0].contains("⟦toron⟧") && !lines[0].contains("secret brief body"));
        assert!(
            !lines[0].contains("Fix the doorbell"),
            "no payload in the pane"
        );
    }

    fn epoch_of(msg_id: &str) -> u64 {
        msg_id.rsplit(':').nth(1).unwrap().parse().unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_runs_never_share_an_epoch() {
        let store = mem_store().await;
        let rcpt = vec![("Birch".to_string(), B.to_string())];
        let (d1, d2, d3) = (
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let t = task();
        let (r1, r2, r3) = tokio::join!(
            append_task(&store, d1.path(), A, &rcpt, "c", "t-a", &t),
            append_task(&store, d2.path(), A, &rcpt, "c", "t-b", &t),
            append_task(&store, d3.path(), A, &rcpt, "c", "t-c", &t),
        );
        let rows = [r1.unwrap(), r2.unwrap(), r3.unwrap()];
        let mut epochs: Vec<u64> = rows.iter().map(|r| epoch_of(&r[0].msg_id)).collect();
        for r in &rows {
            assert_eq!(r[0].state, "appended", "a fenced run mints again: {r:?}");
        }
        epochs.sort_unstable();
        epochs.dedup();
        assert_eq!(epochs.len(), 3, "three runs, three distinct epochs");
        let log = store
            .read_stream(toron_core::inbox::INBOX_STREAM, None)
            .await
            .unwrap();
        assert_eq!(log.len(), 3, "one row per run");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn append_task_takes_its_senders_turn_before_claiming() {
        let store = mem_store().await;
        let rcpt = vec![("Birch".to_string(), B.to_string())];
        let dir = tempfile::tempdir().unwrap();
        let held = sender_gate(A).lock_owned().await;
        let (st, d, r) = (store.clone(), dir.path().to_path_buf(), rcpt.clone());
        let run =
            tokio::spawn(async move { append_task(&st, &d, A, &r, "c", "t-gated", &task()).await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!run.is_finished(), "must wait for the sender's turn");
        assert_eq!(
            store
                .read_stream(toron_core::inbox::INBOX_STREAM, None)
                .await
                .unwrap()
                .len(),
            0,
            "nothing claimed or appended while another run holds the turn"
        );
        drop(held);
        let rows = run.await.unwrap().unwrap();
        assert_eq!(rows[0].state, "appended");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_concurrent_runs_of_one_sender_all_land_on_distinct_epochs() {
        let store = mem_store().await;
        let rcpt = vec![("Birch".to_string(), B.to_string())];
        let dir = tempfile::tempdir().unwrap();
        let mut set = tokio::task::JoinSet::new();
        for i in 0..12 {
            let (st, d, r) = (
                store.clone(),
                dir.path().join(format!("r{i}")),
                rcpt.clone(),
            );
            set.spawn(async move {
                append_task(&st, &d, A, &r, "c", &format!("t-many-{i}"), &task()).await
            });
        }
        let mut rows = Vec::new();
        while let Some(r) = set.join_next().await {
            rows.push(r.unwrap().unwrap());
        }
        let mut epochs: Vec<u64> = rows.iter().map(|r| epoch_of(&r[0].msg_id)).collect();
        epochs.sort_unstable();
        epochs.dedup();
        assert_eq!(epochs.len(), 12, "twelve runs, twelve distinct epochs");
        assert!(rows.iter().all(|r| r[0].state == "appended"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_single_run_needs_no_lock_or_epoch_file() {
        let store = mem_store().await;
        let dir = tempfile::tempdir().unwrap();
        let rcpt = vec![("Birch".to_string(), B.to_string())];
        let rows = append_task(&store, dir.path(), A, &rcpt, "c", "t-1", &task())
            .await
            .unwrap();
        assert_eq!(rows[0].state, "appended");
        assert_eq!(epoch_of(&rows[0].msg_id), 0, "the first claim is epoch 0");
        let names: Vec<String> = std::fs::read_dir(comms_dir(dir.path(), A))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| n.ends_with(".json")) && names.len() == 1,
            "only the intent record, no lock/epoch file: {names:?}"
        );
        // The claim is what the log now says the epoch is.
        assert_eq!(store.producer_epoch(A).await.unwrap(), Some(0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_recorded_intent_fenced_by_a_newer_claim_mints_again() {
        let store = mem_store().await;
        let dir = tempfile::tempdir().unwrap();
        let mine = store.claim_epoch(A, "this-run").await.unwrap();
        assert_eq!(mine.epoch, 0);
        let env = task_envelope(A, B, msg(0, 0), "c", "t-f", &task()).unwrap();
        let path = comms_dir(dir.path(), A).join(format!("{}.json", intent_key("t-f", B)));
        write_atomic(&path, serde_json::to_string(&env).unwrap().as_bytes()).unwrap();
        // Another run claims a newer epoch: the recorded one can never land.
        let other = store.claim_epoch(A, "other-run").await.unwrap();
        assert_eq!(other.epoch, 1);
        let rcpt = vec![("Birch".to_string(), B.to_string())];
        let rows = append_task(&store, dir.path(), A, &rcpt, "c", "t-f", &task())
            .await
            .unwrap();
        assert_eq!(rows[0].state, "appended");
        assert_ne!(rows[0].msg_id, env.msg.key(), "a fresh msg id");
        assert!(
            epoch_of(&rows[0].msg_id) > other.epoch,
            "under a fresh claim"
        );
        assert_eq!(
            store.lookup(&env.msg).await.unwrap(),
            None,
            "never delivered"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn different_content_under_a_recorded_triple_is_a_hard_error_never_a_replay() {
        let store = mem_store().await;
        let dir = tempfile::tempdir().unwrap();
        // The log already holds DIFFERENT content under (A, 0, 0)...
        let other_task = task_from_text("Another brief entirely", None, vec![]);
        let held = task_envelope(A, B, msg(0, 0), "c", "t-held", &other_task).unwrap();
        store.append_envelope(&held).await.unwrap();
        // ...and flywheel's intent record for t-x points at that same triple.
        let mine = task_envelope(A, B, msg(0, 0), "c", "t-x", &task()).unwrap();
        let path = comms_dir(dir.path(), A).join(format!("{}.json", intent_key("t-x", B)));
        write_atomic(&path, serde_json::to_string(&mine).unwrap().as_bytes()).unwrap();
        let head = rows_for(&store, B).await;
        let rcpt = vec![("Birch".to_string(), B.to_string())];
        let err = append_task(&store, dir.path(), A, &rcpt, "c", "t-x", &task())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<SendError>(),
                Some(SendError::PayloadConflict { .. })
            ),
            "{err:#}"
        );
        assert_eq!(rows_for(&store, B).await, head, "nothing appended");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reused_task_id_with_a_different_brief_is_a_conflict_not_a_silent_resend() {
        let store = mem_store().await;
        let dir = tempfile::tempdir().unwrap();
        let rcpt = vec![("Birch".to_string(), B.to_string())];
        append_task(&store, dir.path(), A, &rcpt, "c", "t-1", &task())
            .await
            .unwrap();
        let changed = task_from_text("A different brief", None, vec![]);
        let err = append_task(&store, dir.path(), A, &rcpt, "c", "t-1", &changed)
            .await
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<SendError>(),
            Some(SendError::PayloadConflict { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn toron_payload_conflict_text_is_what_we_classify() {
        let store = mem_store().await;
        let a = task_envelope(A, B, msg(0, 0), "c", "t-1", &task()).unwrap();
        store.append_envelope(&a).await.unwrap();
        let b = task_envelope(A, B, msg(0, 0), "c", "t-2", &task()).unwrap();
        let typed = store.append_envelope(&b).await.unwrap_err();
        assert!(matches!(typed, InboxError::PayloadConflict { .. }));
        // The kernel-facing entry (what mail.append uses) carries it as text.
        let kernel = store
            .append_json(
                toron_core::inbox::INBOX_STREAM,
                &toron_core::inbox::l16_json(&b),
            )
            .await
            .unwrap_err();
        assert!(is_payload_conflict(&kernel.to_string()), "{kernel}");
        assert!(!is_payload_conflict("stale epoch 0 < 3"));
    }

    fn listed_agent(pane: &str, name: Option<&str>, display: Option<&str>) -> AgentInfo {
        AgentInfo {
            agent: None,
            agent_status: toron_core::backend::AgentStatus::Working,
            cwd: None,
            display_agent: display.map(str::to_string),
            focused: false,
            foreground_cwd: None,
            interactive_ready: false,
            launch_pending: false,
            name: name.map(str::to_string),
            pane_id: pane.to_string(),
            revision: 0,
            state_change_seq: 0,
            completion_seq: None,
            state_labels: serde_json::Map::new(),
            tab_id: String::new(),
            terminal_id: String::new(),
            terminal_title: None,
            workspace_id: String::new(),
        }
    }

    /// The sender wake resolves a pin to exactly one block by name or
    /// display label: missing, case-drifted, substring, and ambiguous
    /// addresses all refuse instead of ringing the wrong pane.
    #[test]
    fn backend_wake_target_matches_name_or_display_exactly_once() {
        let one = vec![listed_agent("block:a", Some("TealOtter"), None)];
        assert_eq!(
            backend_wake_target(&one, "TealOtter").as_deref(),
            Ok("block:a")
        );
        let via_display = vec![listed_agent("block:b", None, Some("TealOtter"))];
        assert_eq!(
            backend_wake_target(&via_display, "TealOtter").as_deref(),
            Ok("block:b")
        );
        assert!(backend_wake_target(&[], "TealOtter").is_err());
        assert!(backend_wake_target(&one, "tealotter").is_err());
        assert!(backend_wake_target(&one, "Teal").is_err());
        let two = vec![
            listed_agent("block:a", Some("TealOtter"), None),
            listed_agent("block:c", None, Some("TealOtter")),
        ];
        assert!(backend_wake_target(&two, "TealOtter").is_err());
    }
}

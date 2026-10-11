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
//! appends it once. A sender process claims a fresh epoch per run from a
//! local lease ([`next_epoch`]) so two runs never reuse a triple.
//!
//! Gated: `TORON_COMMS_INBOX=1` or `[comms] inbox_log = true`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
use toron_core::contracts::task_v1::TaskV1;
use toron_core::delivery::Reject;
use toron_core::envelope::{
    Class, ContextId, ContractId, ENVELOPE_V, Envelope, MsgId, Recipient, TaskId, is_npub_hex,
};
use toron_core::inbox::InboxStore;
use toron_core::outbox::{self, SendState};

use crate::config::Config;
use crate::pane_write::{PaneTyper, PaneWrite, Urgency, Wm};

/// The comms flag: env `TORON_COMMS_INBOX=1` or `[comms] inbox_log = true`.
pub fn enabled(cfg: &Config) -> bool {
    toron_core::inbox::enabled()
        || cfg
            .comms
            .as_ref()
            .and_then(|c| c.inbox_log)
            .unwrap_or(false)
}

/// Typed send failures. Every one of them means: nothing was appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    Disabled,
    BadAddress { token: String },
    UnknownAddress { token: String, why: String },
    AmbiguousAddress { token: String, why: String },
    NoSender,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(
                f,
                "comms inbox is off: set TORON_COMMS_INBOX=1 or [comms] inbox_log = true (nothing sent)"
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
        body: serde_json::to_value(task).map_err(|e| Reject::BadEnvelope(e.to_string()))?,
    };
    env.validate()?;
    Ok(env)
}

/// Pure: the next epoch a sender run claims. Above both the log's producer
/// state and every epoch this host already leased, so a triple is never
/// reused (a reused triple would replay as someone else's message).
pub fn next_epoch(store_epoch: Option<u64>, leased: Option<u64>) -> u64 {
    let a = store_epoch.map_or(0, |e| e + 1);
    let b = leased.map_or(0, |e| e + 1);
    a.max(b)
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

fn state_text(s: &SendState) -> (String, Option<u64>) {
    match s {
        SendState::Settled(o) => ("appended".into(), Some(*o)),
        SendState::InFlight => ("unknown".into(), None),
        SendState::Unknown(n) => (format!("unknown (reconciled {n}x)"), None),
        SendState::Refused(why) => (format!("refused: {why}"), None),
    }
}

/// Boundary: append one task to every recipient, exactly once per
/// `(sender, task id, recipient)`. Holds the sender's local lock for the
/// whole run (one sender process at a time per npub on this host).
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
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("sender.lock"))?;
    lock.lock().context("lock the sender")?;

    // 1. Reconcile whatever a crashed earlier run journaled (toron's
    //    mail.append recovery): present -> settled, absent -> appended with
    //    the SAME triple.
    outbox::send_and_recover(store.clone(), sender.to_string(), Vec::new())
        .await
        .map_err(|e| anyhow!("recover sender intents: {e}"))?;

    // 2. Plan each recipient from its intent record and the log.
    let store_epoch = store.producer_epoch(sender).await?;
    let mut plans = Vec::new();
    for (token, npub) in recipients {
        let path = dir.join(format!("{}.json", intent_key(task_id, npub)));
        let recorded: Option<Envelope> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok());
        let offset = match &recorded {
            Some(env) => store.lookup(&env.msg).await?,
            None => None,
        };
        plans.push((
            token.clone(),
            npub.clone(),
            path,
            plan_send(recorded, offset, store_epoch),
        ));
    }

    // 3. Mint under a freshly leased epoch; record each intent BEFORE the
    //    send (the record is what makes a retry re-send the same msg id).
    let mut sends = Vec::new();
    let mut rows = Vec::new();
    let mut minted = Vec::new();
    if plans.iter().any(|p| matches!(p.3, SendPlan::Mint)) {
        let lease = dir.join("epoch");
        let leased = std::fs::read_to_string(&lease)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok());
        let epoch = next_epoch(store_epoch, leased);
        write_atomic(&lease, epoch.to_string().as_bytes())?;
        let mut seq = 0u64;
        for (_, npub, path, plan) in &plans {
            if matches!(plan, SendPlan::Mint) {
                let msg = MsgId {
                    producer: sender.to_string(),
                    epoch,
                    seq,
                };
                seq += 1;
                let env = task_envelope(sender, npub, msg, context, task_id, task)
                    .map_err(|r| anyhow!("task@1 refused: {r}"))?;
                write_atomic(path, serde_json::to_string(&env)?.as_bytes())?;
                minted.push(env);
            }
        }
    }
    let mut minted_iter = minted.into_iter();
    let mut resend = Vec::new();
    let mut fresh = Vec::new();
    for (token, npub, _, plan) in plans {
        let env = match plan {
            SendPlan::AlreadyAppended { env, offset } => {
                rows.push((
                    token,
                    npub,
                    env.msg.key(),
                    Some(("already-appended".to_string(), Some(offset))),
                ));
                continue;
            }
            SendPlan::Resend(env) => {
                resend.push(env.clone());
                env
            }
            SendPlan::Mint => {
                let env = minted_iter
                    .next()
                    .expect("one minted envelope per Mint plan");
                fresh.push(env.clone());
                env
            }
        };
        rows.push((token, npub, env.msg.key(), None));
    }
    // Older epochs first: the log fences an epoch below its current one.
    sends.extend(resend);
    sends.extend(fresh);

    // 4. mail.append (journal first, then append; reconcile, never blind).
    let report = if sends.is_empty() {
        None
    } else {
        Some(
            outbox::send_and_recover(store.clone(), sender.to_string(), sends)
                .await
                .map_err(|e| anyhow!("mail.append: {e}"))?,
        )
    };
    Ok(rows
        .into_iter()
        .map(|(to, npub, key, done)| {
            let (state, offset) = done.unwrap_or_else(|| {
                report
                    .as_ref()
                    .and_then(|r| r.sent.get(&key))
                    .map_or(("unknown".into(), None), state_text)
            });
            SentRow {
                to,
                npub,
                msg_id: key,
                state,
                offset,
                wake: String::new(),
            }
        })
        .collect())
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
    let mb = crate::mail_shim::comms_mailbox()?;
    let pool = mb
        .index_pool()
        .cloned()
        .ok_or_else(|| anyhow!("comms inbox needs the index pool"))?;
    Ok(InboxStore::new(pool))
}

/// The sender pin/npub: explicit, else `FLYWHEEL_MAIL_AS`, else `AGENT_NAME`.
pub fn sender_token(from: Option<&str>) -> Result<String, SendError> {
    from.map(str::to_string)
        .or_else(|| std::env::var("FLYWHEEL_MAIL_AS").ok())
        .or_else(|| std::env::var("AGENT_NAME").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or(SendError::NoSender)
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
    let sender_tok = sender_token(req.from)?;
    let store = store()?;
    let mb = crate::mail_shim::mailbox();
    let project = req.project.to_string();
    let (sender, recipients) = crate::mail_shim::block_on(async {
        let sender = resolve(mb, &project, &sender_tok).await?;
        let mut out = Vec::new();
        for t in &tokens {
            out.push((t.clone(), resolve(mb, &project, t).await?));
        }
        Ok::<_, SendError>((sender, out))
    })?;
    let state_dir = crate::config::state_dir();
    let mut rows = crate::mail_shim::block_on(append_task(
        &store,
        &state_dir,
        &sender,
        &recipients,
        &project,
        req.task_id,
        &req.task,
    ))?;
    // Wake: the constant line only, to the recipient's pane.
    let mut herdr = crate::herdr_shim::Client::connect(Some(&cfg.socket_path()));
    for (row, addr) in rows.iter_mut().zip(parsed.iter()) {
        row.wake = match (&mut herdr, addr.pin()) {
            (_, None) => "no-pane (npub address; Phase 3 rings by binding)".into(),
            (Err(e), _) => format!("skipped: herdr unavailable ({e})"),
            (Ok(c), Some(pin)) => match wake_target(c, &project, pin) {
                Err(e) => format!("no-pane ({e:#})"),
                Ok(target) => {
                    let panes = crate::pane_write::HerdrPanes(c);
                    match crate::mail_shim::block_on(ring(&store, &panes, &target, &row.npub)) {
                        Ok(wm) => format!("rung wm {:x}", wm.0),
                        Err(e) => format!("failed: {e:#}"),
                    }
                }
            },
        };
    }
    Ok(rows)
}

fn wake_target(c: &mut crate::herdr_shim::Client, project: &str, pin: &str) -> Result<String> {
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
    fn epochs_never_reuse_a_triple() {
        assert_eq!(next_epoch(None, None), 0);
        assert_eq!(next_epoch(Some(4), None), 5);
        assert_eq!(next_epoch(Some(4), Some(9)), 10);
        // A leased epoch whose send never landed still moves the next one up.
        assert_eq!(next_epoch(None, Some(0)), 1);
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
    async fn a_recorded_intent_that_never_landed_is_sent_with_the_same_msg_id() {
        let store = mem_store().await;
        let dir = tempfile::tempdir().unwrap();
        // Simulate a crash after flywheel recorded the intent, before any
        // append: the record exists, the log does not have it.
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
}

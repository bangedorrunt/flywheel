// governed-by: redesign-proposal §6A.12 (flywheel owns the host and interpreter), build-plan §8
//! The supervisor host (Phase 4a): interprets [`SupervisorProgram`]'s
//! commands and feeds it facts. The fold decides; this file does.
//!
//! Shape (same as the inbox host):
//! - [`Core`] holds the model and the context log. [`round`] folds a batch
//!   of messages; each message that changes the model or emits a command is
//!   written to the log *before* its commands run (write-ahead), so a kill
//!   at any point replays to the same model (P8).
//! - Every command is `Durable`: [`execute`] runs it under the effect
//!   journal ([`EffectJournal`]; production = toron's `ToronJournal`):
//!   intent recorded, claimed, settled. A key that already settled is
//!   skipped, so a replay never relaunches or escalates twice. [`recover`]
//!   re-runs what the log says was asked and the journal says never
//!   finished.
//! - [`Ports`] is the effect boundary (herdr, mail, `br`, the captain
//!   page). The fold never touches it.
//!
//! The context log is the supervisor's fact log: one JSON line per accepted
//! message, `{"seq","msg","in_off"}`. Operators and crews add facts through
//! `flywheel supervisor fact|steer` into `<ctx>.in.jsonl`; the host is the
//! only writer of `<ctx>.log.jsonl` and records how far it has read the
//! input file, so a restart neither loses nor repeats an input.
//!
//! A steer is a `steer@1` envelope (`op`, `priority`, optional `delta`,
//! `sup_epoch`). `priority` is `urgent` unless the op is `resume` (the same
//! rule the fold uses). The doorbell reads that field and rings; this host
//! only appends. When `inbox_lead` can be read, `sup_epoch` is the epoch
//! toron stored on `lead@1`. `FLYWHEEL_SUPERVISOR_EPOCH=fold` keeps the
//! fold's counter and writes no lead fact. An escalation is `escalate@1`
//! (`context`, `reason`, `evidence`). `Assign` appends one `task@1` to the
//! crew npub and does not type a pane. A steer the log already appended is not sent again; a
//! restart resolves one that is still pending.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use toron_core::kernel::{Cmd as _, Durability, Program};

use crate::supervisor_program::{
    ChildKind, ChildSpec, ChildState, CrewRow, PASS, Policy, Restart, SteerOp, SupCmd, SupModel,
    SupMsg, SupervisorProgram, Tier, crew_view,
};

/// Journal answer for one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Begin {
    /// Run it; settle with this generation.
    Run(u64),
    /// Already finished: skip.
    Skip,
    /// The journal could not be reached: do not run blind.
    Unavailable(String),
    /// A live claim by another owner (not this host's predecessor) holds the
    /// key. Nothing ran and nothing failed: the holder settles it (rv23 S2).
    Held(String),
}

/// The effect journal a command runs under.
pub trait EffectJournal {
    /// Record the intent (idempotent on `key`) and claim it.
    fn begin(&mut self, key: &str) -> Begin;
    /// Settle the claim.
    fn finish(&mut self, key: &str, generation: u64, ok: bool);
}

/// The effect boundary. Each method is one interpreted command.
pub trait Ports {
    /// Facts this round: registry spawns, cursors, deaf facts, herdr edges.
    fn observe(&mut self, model: &SupModel, now_ms: u64) -> Vec<SupMsg>;
    /// Start incarnation `generation` of `child`. A lead is a nested supervise
    /// process, never a pane write.
    fn relaunch(&mut self, child: &str, generation: u64, kind: ChildKind) -> Result<(), String>;
    /// Stop `child`'s pane.
    fn exit(&mut self, child: &str, generation: u64) -> Result<(), String>;
    /// Append the `steer@1` envelope. Returns the log offset. Does not type
    /// a pane: the doorbell rings from the envelope's `priority`.
    fn append_steer(&mut self, s: &SteerSend) -> Result<u64, String>;
    /// Append one `task@1` for an assignment. Does not type a pane.
    /// Missing npub is an error, not a quiet success.
    fn assign(&mut self, a: &AssignSend) -> Result<u64, String>;
    /// Tell the captain and record the fact.
    fn escalate(&mut self, n: u64, reason: &str, evidence: &str) -> Result<(), String>;
    /// Park a poisoned child.
    fn park(&mut self, child: &str, incarnations: u32) -> Result<(), String>;
    /// Page the captain.
    fn page(&mut self, kind: &str, pin: &str, line: &str) -> Result<(), String>;
    /// Record a rejected input.
    fn reject(&mut self, n: u64, cause: &str, child: &str) -> Result<(), String>;
    /// The directory a captain hold runs `br` in: the project's, where its
    /// `.beads` is (the same dir `br ready` and `br init` use). `None`
    /// inherits the host's cwd.
    fn tracker_dir(&self) -> Option<PathBuf> {
        None
    }
}

/// What `append_steer` needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SteerSend {
    /// Steer seq.
    pub seq: u64,
    /// Target child.
    pub child: String,
    /// Op.
    pub op: SteerOp,
    /// Incarnation.
    pub generation: u64,
    /// Epoch.
    pub sup_epoch: u64,
    /// `op != resume`. Not read when building the envelope: `priority`
    /// follows the op, so a stale flag cannot disagree with the fold.
    pub urgent: bool,
    /// sha256 of the redirect's delta; its bytes are in the side store.
    pub delta_sha: Option<String>,
}

/// What `assign` appends. The journal key stays `assign:<task>:<child>:<gen>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignSend {
    /// Task id. A `bd-` prefix is stored as the task bead.
    pub task: String,
    /// Crew pin.
    pub child: String,
    /// The crew's generation.
    pub generation: u64,
}

/// Refuse a missing npub before any append.
pub(crate) fn require_npub(pin: &str, npub: Option<String>) -> Result<String, String> {
    npub.ok_or_else(|| format!("no npub for {pin}"))
}

// ---- the context log ------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Entry {
    seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    in_off: Option<u64>,
    msg: SupMsg,
}

/// Paths of one context's files.
#[derive(Clone, Debug)]
pub struct CtxPaths {
    /// The authoritative log (host-only writer).
    pub log: PathBuf,
    /// Operator/crew inputs (CLI appends).
    pub inbox: PathBuf,
}

impl CtxPaths {
    /// Files for `project` under `dir`.
    pub fn new(dir: &Path, project: &str) -> Self {
        let key = crate::state::state_key(project);
        Self {
            log: dir.join(format!("{key}.log.jsonl")),
            inbox: dir.join(format!("{key}.in.jsonl")),
        }
    }

    /// A lead's own log. The root log stays [`Self::new`].
    pub fn with_context(dir: &Path, project: &str, context: &str) -> Self {
        let key = format!("{}--{context}", crate::state::state_key(project));
        Self {
            log: dir.join(format!("{key}.log.jsonl")),
            inbox: dir.join(format!("{key}.in.jsonl")),
        }
    }

    /// The default state directory.
    pub fn default_dir() -> PathBuf {
        crate::config::state_dir().join("supervisor")
    }
}

/// Read the log: its messages in order, the next seq and the input offset.
pub fn read_log(p: &CtxPaths) -> Result<(Vec<SupMsg>, u64, u64)> {
    let mut msgs = Vec::new();
    let (mut seq, mut in_off) = (0u64, 0u64);
    let Ok(s) = std::fs::read_to_string(&p.log) else {
        return Ok((msgs, 0, 0));
    };
    for (i, l) in s.lines().enumerate() {
        if l.trim().is_empty() {
            continue;
        }
        let e: Entry = serde_json::from_str(l)
            .with_context(|| format!("{}:{}: bad log line", p.log.display(), i + 1))?;
        seq = e.seq + 1;
        if let Some(o) = e.in_off {
            in_off = o;
        }
        msgs.push(e.msg);
    }
    Ok((msgs, seq, in_off))
}

fn append_log(p: &CtxPaths, e: &Entry) -> Result<()> {
    if let Some(d) = p.log.parent() {
        // 0700/0600, not the ambient umask. A log line can carry a steer sha.
        crate::private_fs::create_private_dir_all(d)?;
    }
    let mut f = crate::private_fs::open_private_append(&p.log)?;
    writeln!(f, "{}", serde_json::to_string(e)?)?;
    f.sync_data()?;
    Ok(())
}

/// Drop token shapes and raw newlines from free-text messages before they
/// are folded. Child ids and epochs stay, so the fold's keys do not move.
fn scrub_msg(m: SupMsg) -> SupMsg {
    use crate::redact::flatten;
    match m {
        SupMsg::Wedged { pin, line } => SupMsg::Wedged {
            pin: flatten(&pin),
            line: flatten(&line),
        },
        SupMsg::Stalled { line } => SupMsg::Stalled {
            line: flatten(&line),
        },
        SupMsg::ChildEscalated {
            child,
            reason,
            now_ms,
            fact_key,
        } => SupMsg::ChildEscalated {
            child,
            reason: flatten(&reason),
            now_ms,
            fact_key,
        },
        SupMsg::CmdFailed { key, why } => SupMsg::CmdFailed {
            key,
            why: flatten(&why),
        },
        SupMsg::CmdHeld { key, holder } => SupMsg::CmdHeld {
            key,
            holder: flatten(&holder),
        },
        other => other,
    }
}

/// Read complete input lines from `from`; returns them and the new offset
/// (a half-written last line is left for the next read).
pub fn read_inputs(p: &CtxPaths, from: u64) -> (Vec<SupMsg>, u64) {
    let Ok(mut f) = std::fs::File::open(&p.inbox) else {
        return (Vec::new(), from);
    };
    let mut buf = String::new();
    if std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(from)).is_err()
        || f.read_to_string(&mut buf).is_err()
    {
        return (Vec::new(), from);
    }
    let mut out = Vec::new();
    let mut used = 0usize;
    for l in buf.split_inclusive('\n') {
        if !l.ends_with('\n') {
            break;
        }
        used += l.len();
        match serde_json::from_str::<SupMsg>(l.trim()) {
            Ok(m) => out.push(m),
            Err(e) => eprintln!("flywheel supervisor: skipping a bad input line: {e}"),
        }
    }
    (out, from + used as u64)
}

/// CLI side: add one input fact.
pub fn append_input(p: &CtxPaths, msg: &SupMsg) -> Result<()> {
    if let Some(d) = p.inbox.parent() {
        crate::private_fs::create_private_dir_all(d)?;
    }
    let mut f = crate::private_fs::open_private_append(&p.inbox)?;
    // One write per line: O_APPEND keeps concurrent writers' lines whole.
    f.write_all(format!("{}\n", serde_json::to_string(msg)?).as_bytes())?;
    Ok(())
}

// ---- the core: model + log ------------------------------------------------

/// Model, log and read position.
pub struct Core {
    /// The fold's model.
    pub model: SupModel,
    paths: CtxPaths,
    seq: u64,
    in_off: u64,
    pending_in_off: Option<u64>,
}

/// What a round did (tests and the host's status line read it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoundReport {
    /// Messages folded.
    pub folded: u64,
    /// Messages written to the log.
    pub logged: u64,
    /// Commands run to success.
    pub ran: u64,
    /// Commands skipped (journal says settled).
    pub skipped: u64,
    /// Commands that failed.
    pub failed: u64,
    /// Commands another live owner's claim held (not failures).
    pub held: u64,
    /// Effects `FLYWHEEL_ALLOW_EXEC=0` refused. One line each.
    pub suppressed: Vec<String>,
}

impl Core {
    /// Open a context: replay the log. A fresh context starts with `Hello`;
    /// every open then claims the next `sup_epoch` (the lease fact), so a
    /// restarted supervisor outranks whatever it replaced.
    pub fn open(paths: CtxPaths, me: &str, tier: Tier, policy: Option<Policy>) -> Result<Self> {
        let (msgs, seq, in_off) = read_log(&paths)?;
        let (model, _) = crate::supervisor_program::replay(&msgs);
        let mut core = Self {
            model,
            paths,
            seq,
            in_off,
            pending_in_off: None,
        };
        if msgs.is_empty() {
            core.append_fold(SupMsg::Hello {
                me: me.into(),
                tier,
                policy,
            })?;
        }
        Ok(core)
    }

    /// Claim the next epoch from the fold (`accepted_epoch + 1`).
    ///
    /// This runs only while the fold is the authority (`FLYWHEEL_SUPERVISOR_EPOCH=fold`,
    /// or `inbox_lead` cannot be read). Startup uses [`claim_for_host`] when
    /// the store is the authority, so the two counters never both assign.
    pub fn claim(&mut self, holder: &str) -> Result<u64> {
        let e = self.model.accepted_epoch + 1;
        self.claim_stored(holder, e)
    }

    /// Record a `sup_epoch` toron already stored. The fold does not add one.
    /// A number at or below the accepted epoch is logged and refused.
    pub fn claim_stored(&mut self, holder: &str, sup_epoch: u64) -> Result<u64> {
        if self.model.me != holder {
            self.append_fold(SupMsg::Rebind { me: holder.into() })?;
        }
        self.append_fold(SupMsg::Claim {
            holder: holder.into(),
            sup_epoch,
        })?;
        if self.model.accepted_epoch == sup_epoch && self.model.leader == holder {
            Ok(sup_epoch)
        } else {
            bail!("fold refused lead epoch {sup_epoch}")
        }
    }

    fn append_fold(&mut self, m: SupMsg) -> Result<Vec<SupCmd>> {
        // Free text is flattened before the fold, so the log, the replay, and
        // the page all see the same line. A token never lands in the jsonl.
        let m = scrub_msg(m);
        let u = SupervisorProgram::update(&self.model, &m);
        if u.model != self.model || !u.commands.is_empty() {
            append_log(
                &self.paths,
                &Entry {
                    seq: self.seq,
                    in_off: self.pending_in_off.take(),
                    msg: m,
                },
            )?;
            self.seq += 1;
        }
        self.model = u.model;
        Ok(u.commands)
    }

    /// The paths this core reads and writes. The host loop reads them; tests
    /// drive the core directly, which is why `paths` is read only there.
    #[cfg(test)]
    pub fn paths(&self) -> &CtxPaths {
        &self.paths
    }
}

// ---- executing commands ---------------------------------------------------

/// `steer@1` body.
///
/// `priority` is `urgent` when the fold would treat the steer as urgent
/// (`op != resume`), otherwise `normal`. `delta` is the 4b.4 object on
/// `redirect` and is omitted for every other op. `ack_by` is omitted: this
/// host does not put a deadline on the envelope. `sup_epoch` is the epoch
/// the caller holds. `0` is valid only while the context has no `lead@1`
/// row; once a row exists toron refuses any other.
pub(crate) fn steer_v1(
    s: &SteerSend,
    delta: Option<&crate::steer_delta::Delta>,
) -> toron_core::contracts::steer_v1::SteerV1 {
    let delta = if s.op == SteerOp::Redirect {
        delta.and_then(|d| serde_json::from_str(&d.json).ok())
    } else {
        None
    };
    toron_core::contracts::steer_v1::SteerV1 {
        op: s.op.tag().to_string(),
        priority: if s.op == SteerOp::Resume {
            "normal"
        } else {
            "urgent"
        }
        .to_string(),
        delta,
        ack_by: None,
        sup_epoch: s.sup_epoch,
    }
}

/// The envelope `append_steer` appends. `task` is the child: the fold has no
/// task id distinct from the child pin. `generation` is the incarnation the
/// command named (a redirect's is already `g+1`).
pub(crate) fn steer_envelope(
    project: &str,
    producer: &str,
    epoch: u64,
    to_npub: &str,
    s: &SteerSend,
    delta: Option<&crate::steer_delta::Delta>,
) -> Result<toron_core::envelope::Envelope, String> {
    use toron_core::envelope::{
        Class, ContextId, ContractId, ENVELOPE_V, Envelope, MsgId, Recipient, TaskId,
    };
    let body = serde_json::to_value(steer_v1(s, delta)).map_err(|e| e.to_string())?;
    let env = Envelope {
        v: ENVELOPE_V,
        msg: MsgId {
            producer: producer.to_string(),
            epoch,
            seq: 0,
        },
        from: producer.to_string(),
        to: Recipient::Npub(to_npub.to_string()),
        class: Class::Control,
        contract: ContractId::new("steer", 1),
        context: ContextId(project.to_string()),
        task: Some(TaskId(s.child.clone())),
        generation: u32::try_from(s.generation).unwrap_or(u32::MAX),
        causation: None,
        deadline_ts_us: None,
        lease_ms: None,
        body,
    };
    env.validate().map_err(|e| format!("{e:?}"))?;
    Ok(env)
}

/// One `task@1` for an assignment. Title is the task id. Brief names the
/// crew. A task id that starts with `bd-` is the bead. No pane text.
pub(crate) fn assign_envelope(
    project: &str,
    producer: &str,
    epoch: u64,
    to_npub: &str,
    task: &str,
    child: &str,
    generation: u64,
) -> Result<toron_core::envelope::Envelope, String> {
    use toron_core::contracts::task_v1::TaskV1;
    let bead = task.starts_with("bd-").then(|| task.to_string());
    let body = TaskV1 {
        title: task.to_string(),
        brief: format!("assigned to {child}"),
        scope_paths: Vec::new(),
        acceptance: Vec::new(),
        bead,
    };
    let env = crate::comms_send::task_envelope(
        producer,
        to_npub,
        toron_core::envelope::MsgId {
            producer: producer.to_string(),
            epoch,
            seq: 0,
        },
        project,
        task,
        &body,
    )
    .map_err(|e| format!("{e:?}"))?;
    let mut env = env;
    env.generation = u32::try_from(generation).unwrap_or(u32::MAX);
    env.validate().map_err(|e| format!("{e:?}"))?;
    Ok(env)
}

/// `escalate@1` body. `reason` must be non-empty. `evidence` is whatever the
/// caller has; an empty slice is an empty list, not a missing field.
/// `context` is prose for the reader (the project), not an envelope id.
pub(crate) fn escalate_v1(
    context: &str,
    reason: &str,
    evidence: &[String],
) -> Result<toron_core::contracts::escalate_v1::EscalateV1, String> {
    if reason.trim().is_empty() {
        return Err("escalate@1 reason must be non-empty".into());
    }
    Ok(toron_core::contracts::escalate_v1::EscalateV1 {
        context: context.to_string(),
        reason: reason.to_string(),
        evidence: evidence.to_vec(),
    })
}

/// The envelope `escalate` appends, addressed to the captain's npub.
pub(crate) fn escalate_envelope(
    project: &str,
    producer: &str,
    epoch: u64,
    to_npub: &str,
    reason: &str,
    evidence: &[String],
) -> Result<toron_core::envelope::Envelope, String> {
    use toron_core::envelope::{
        Class, ContextId, ContractId, ENVELOPE_V, Envelope, MsgId, Recipient,
    };
    let body =
        serde_json::to_value(escalate_v1(project, reason, evidence)?).map_err(|e| e.to_string())?;
    let env = Envelope {
        v: ENVELOPE_V,
        msg: MsgId {
            producer: producer.to_string(),
            epoch,
            seq: 0,
        },
        from: producer.to_string(),
        to: Recipient::Npub(to_npub.to_string()),
        class: Class::Control,
        contract: ContractId::new("escalate", 1),
        context: ContextId(project.to_string()),
        task: None,
        generation: 0,
        causation: None,
        deadline_ts_us: None,
        lease_ms: None,
        body,
    };
    env.validate().map_err(|e| format!("{e:?}"))?;
    Ok(env)
}

/// Lease workspace for a supervise host. The root keeps `supervise-<key>`.
/// A lead adds `--<context>` so it can run beside the root.
pub fn supervise_lease_workspace(project: &str, context: Option<&str>) -> String {
    let key = crate::state::state_key(project);
    match context {
        Some(c) if context_token(c) => format!("supervise-{key}--{c}"),
        _ => format!("supervise-{key}"),
    }
}

fn context_token(c: &str) -> bool {
    !c.is_empty()
        && c.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// Argv (no binary) for a lead relaunch: nested supervise, no pane payload.
pub fn lead_supervise_argv(project: &str, context: &str) -> Vec<String> {
    vec![
        "loop-mode".into(),
        "supervise".into(),
        project.into(),
        "--context".into(),
        context.into(),
    ]
}

/// Crew relaunch stays `crew control … relaunch`. A lead uses [`lead_supervise_argv`].
fn lead_facts_file(project: &str, context: Option<&str>) -> PathBuf {
    let name = match context {
        Some(c) if context_token(c) => {
            format!("{}--{c}-supervisor.jsonl", crate::state::state_key(project))
        }
        _ => format!("{}-supervisor.jsonl", crate::state::state_key(project)),
    };
    crate::config::state_dir().join("facts").join(name)
}

/// A facts file read from where this host left off (rv23 #4).
///
/// `next` is how far this host has read; `committed` is what the sidecar
/// (`<file>.read-<reader>`) says. [`FactsCursor::commit`] runs at the start
/// of the next `observe`, after the round that folded those facts, so a
/// restart re-reads at most the last round's lines (and the fold's bounded
/// [`crate::supervisor_program::RecentKeys`] still holds their keys). A file
/// shorter than `next` was replaced, and is read from the start.
#[derive(Debug)]
pub(crate) struct FactsCursor {
    path: PathBuf,
    sidecar: PathBuf,
    next: u64,
    committed: u64,
    /// Bytes read so far by this host (evidence and tests).
    pub(crate) bytes_read: u64,
}

impl FactsCursor {
    pub(crate) fn open(path: PathBuf, reader: &str) -> Self {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".read-{reader}"));
        let sidecar = path.with_file_name(name);
        let committed = std::fs::read_to_string(&sidecar)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        Self {
            path,
            sidecar,
            next: committed,
            committed,
            bytes_read: 0,
        }
    }

    /// Persist how far the last round read. A failed write only means a
    /// restart re-reads more.
    pub(crate) fn commit(&mut self) {
        if self.next == self.committed {
            return;
        }
        if crate::private_fs::write_private_atomic(&self.sidecar, self.next.to_string().as_bytes())
            .is_ok()
        {
            self.committed = self.next;
        }
    }

    /// The complete lines after `next`, and the offset just past them. The
    /// caller moves `next` there with [`FactsCursor::consume`] once it has
    /// used them; a partial last line is left for the next read.
    pub(crate) fn read_new(&mut self) -> (String, u64) {
        let Ok(mut f) = std::fs::File::open(&self.path) else {
            return (String::new(), self.next);
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.next {
            self.next = 0;
        }
        if len == self.next
            || std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(self.next)).is_err()
        {
            return (String::new(), self.next);
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return (String::new(), self.next);
        }
        self.bytes_read += buf.len() as u64;
        let used = buf.rfind('\n').map_or(0, |i| i + 1);
        buf.truncate(used);
        (buf, self.next + used as u64)
    }

    pub(crate) fn consume(&mut self, end: u64) {
        self.next = end;
    }
}

/// [`child_upward`] over the lines `cur` has not spent yet (rv23 #4: the
/// file is no longer re-read whole every round). A line is spent only while
/// the lead runs, because `child_upward` skips it otherwise; a stopped
/// lead's lines are read again once it runs.
fn lead_upward(
    cur: &mut FactsCursor,
    model: &SupModel,
    child: &str,
    now_ms: u64,
    host_dead: bool,
) -> Vec<SupMsg> {
    let (text, end) = cur.read_new();
    if model
        .children
        .get(child)
        .is_some_and(|c| c.state == ChildState::Running)
    {
        cur.consume(end);
    }
    child_upward(model, child, now_ms, &text, host_dead)
}

/// What a parent may learn from one lead's facts file. Pass, progress, and
/// done lines stay in the lead. An escalate line is one `ChildEscalated`.
/// A dead lease past `t_orphan_ms` is one `Adopt`.
fn child_upward(
    model: &SupModel,
    child: &str,
    now_ms: u64,
    text: &str,
    host_dead: bool,
) -> Vec<SupMsg> {
    let Some(c) = model.children.get(child) else {
        return Vec::new();
    };
    if c.spec.kind != ChildKind::Lead {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["kind"] != "escalate" {
            continue;
        }
        let Some(key) = v["key"].as_str().filter(|k| !k.is_empty()) else {
            continue;
        };
        if model.escalated_facts.contains(key) {
            continue;
        }
        if c.state != ChildState::Running {
            continue;
        }
        out.push(SupMsg::ChildEscalated {
            child: child.to_string(),
            reason: v["reason"].as_str().unwrap_or("escalate").to_string(),
            now_ms,
            fact_key: Some(key.to_string()),
        });
    }
    if let Some(msg) = crate::supervisor_program::adopt_msg(model, child, now_ms, host_dead) {
        out.push(msg);
    }
    out
}

fn relaunch_argv(project: &str, child: &str, kind: ChildKind) -> Vec<String> {
    match kind {
        ChildKind::Lead => lead_supervise_argv(project, child),
        ChildKind::Crew => {
            vec![
                "crew".into(),
                "control".into(),
                project.into(),
                child.into(),
                "relaunch".into(),
            ]
        }
    }
}

fn key_of(c: &SupCmd) -> String {
    match c.durability() {
        Durability::Durable { key } => key,
        Durability::Ephemeral => String::new(),
    }
}

// Test threads set this so a long fixture does not flip the process-wide
// env under a sibling test that is also folding.
#[cfg(test)]
thread_local! {
    static EXEC_FORCE_OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `FLYWHEEL_ALLOW_EXEC=0` is the log-only switch. Unset, or any other
/// value, keeps today's effects. Read on each command, not stored.
pub fn exec_allowed() -> bool {
    #[cfg(test)]
    if EXEC_FORCE_OFF.with(|c| c.get()) {
        return false;
    }
    std::env::var("FLYWHEEL_ALLOW_EXEC").ok().as_deref() != Some("0")
}

/// Relaunch spawns a process. Exit closes a pane. Both stay off the
/// journal when the switch is on: a settled row would confirm work nobody
/// did, and recover would feed `Relaunched`.
fn suppression_line(cmd: &SupCmd) -> Option<String> {
    if exec_allowed() {
        return None;
    }
    match cmd {
        SupCmd::Relaunch {
            child, generation, ..
        } => Some(format!(
            "suppressed Relaunch {child} gen {generation}: FLYWHEEL_ALLOW_EXEC=0"
        )),
        SupCmd::Exit { child, generation } => Some(format!(
            "suppressed Exit {child} gen {generation}: FLYWHEEL_ALLOW_EXEC=0"
        )),
        _ => None,
    }
}

fn note_suppressed(p: &CtxPaths, line: &str) -> Result<()> {
    let path = p.log.with_extension("suppressed");
    if let Some(d) = path.parent() {
        crate::private_fs::create_private_dir_all(d)?;
    }
    let mut f = crate::private_fs::open_private_append(&path)?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn dispatch(
    core: &Core,
    j: &mut dyn EffectJournal,
    ports: &mut dyn Ports,
    cmd: &SupCmd,
    rep: &mut RoundReport,
) -> Result<Vec<SupMsg>> {
    let n = rep.suppressed.len();
    let back = execute(j, ports, cmd, rep);
    for line in &rep.suppressed[n..] {
        note_suppressed(&core.paths, line)?;
    }
    Ok(back)
}

/// Run one command under the journal. Returns the messages its outcome
/// feeds back into the fold.
pub fn execute(
    j: &mut dyn EffectJournal,
    ports: &mut dyn Ports,
    cmd: &SupCmd,
    rep: &mut RoundReport,
) -> Vec<SupMsg> {
    if let Some(line) = suppression_line(cmd) {
        eprintln!("flywheel supervisor: {line}");
        rep.suppressed.push(line);
        return Vec::new();
    }
    let key = key_of(cmd);
    let generation = match j.begin(&key) {
        Begin::Skip => {
            rep.skipped += 1;
            return Vec::new();
        }
        Begin::Unavailable(why) => {
            rep.failed += 1;
            return vec![SupMsg::CmdFailed {
                key,
                why: format!("journal unavailable: {why}"),
            }];
        }
        // Not a failure of the child: a refused claim spends no restart.
        Begin::Held(holder) => {
            rep.held += 1;
            return vec![SupMsg::CmdHeld { key, holder }];
        }
        Begin::Run(g) => g,
    };
    let res: Result<Vec<SupMsg>, String> = match cmd {
        SupCmd::Relaunch {
            child,
            generation,
            kind,
        } => ports.relaunch(child, *generation, *kind).map(|_| {
            vec![SupMsg::Relaunched {
                child: child.clone(),
                generation: *generation,
            }]
        }),
        SupCmd::Exit { child, generation } => {
            let done = ports.exit(child, *generation).map(|_| vec![]);
            if done.is_ok() {
                crate::harness_caps::uninstall_if_configured();
            }
            done
        }
        SupCmd::AppendSteer {
            seq,
            child,
            op,
            generation,
            sup_epoch,
            urgent,
            delta_sha,
        } => ports
            .append_steer(&SteerSend {
                seq: *seq,
                child: child.clone(),
                op: *op,
                generation: *generation,
                sup_epoch: *sup_epoch,
                urgent: *urgent,
                delta_sha: delta_sha.clone(),
            })
            .map(|offset| vec![SupMsg::SteerAppended { seq: *seq, offset }]),
        SupCmd::Escalate {
            n,
            reason,
            evidence,
        } => {
            let dir = ports.tracker_dir();
            ports
                .escalate(*n, reason, evidence)
                .and_then(|_| captain_hold(evidence, reason, dir.as_deref(), rep))
                .map(|_| vec![])
        }
        SupCmd::Assign {
            task,
            child,
            generation,
        } => ports
            .assign(&AssignSend {
                task: task.clone(),
                child: child.clone(),
                generation: *generation,
            })
            .map(|_| vec![]),
        SupCmd::Park {
            child,
            incarnations,
        } => ports.park(child, *incarnations).map(|_| vec![]),
        SupCmd::Page {
            kind, pin, line, ..
        } => ports.page(kind, pin, line).map(|_| vec![]),
        SupCmd::Reject { n, cause, child } => ports.reject(*n, cause.tag(), child).map(|_| vec![]),
    };
    j.finish(&key, generation, res.is_ok());
    match res {
        Ok(v) => {
            rep.ran += 1;
            v
        }
        Err(why) => {
            rep.failed += 1;
            vec![SupMsg::CmdFailed { key, why }]
        }
    }
}

/// Fold `msgs` (and the messages their commands feed back) to quiescence.
pub fn feed(
    core: &mut Core,
    j: &mut dyn EffectJournal,
    ports: &mut dyn Ports,
    msgs: Vec<SupMsg>,
    rep: &mut RoundReport,
) -> Result<()> {
    let mut q: VecDeque<SupMsg> = msgs.into();
    while let Some(m) = q.pop_front() {
        rep.folded += 1;
        let before = core.seq;
        let cmds = core.append_fold(m)?;
        rep.logged += core.seq - before;
        for c in &cmds {
            q.extend(dispatch(core, j, ports, c, rep)?);
        }
    }
    Ok(())
}

/// One host round: read inputs, observe, tick, fold, execute.
pub fn round(
    core: &mut Core,
    j: &mut dyn EffectJournal,
    ports: &mut dyn Ports,
    extra: Vec<SupMsg>,
    now_ms: u64,
) -> Result<RoundReport> {
    let mut rep = RoundReport::default();
    let (inputs, off) = read_inputs(&core.paths, core.in_off);
    if off != core.in_off {
        core.in_off = off;
        core.pending_in_off = Some(off);
    }
    let mut msgs = extra;
    msgs.extend(inputs);
    msgs.extend(ports.observe(&core.model, now_ms));
    msgs.push(SupMsg::Tick { now_ms });
    feed(core, j, ports, msgs, &mut rep)?;
    // Inputs that changed nothing still advance the read position.
    if let Some(off) = core.pending_in_off.take() {
        core.in_off = off;
        // Persisted with the next logged message; a restart re-reads them
        // and the fold ignores them again (deterministic no-ops).
        core.pending_in_off = Some(off);
    }
    Ok(rep)
}

/// After a restart: run every command the log asked for that the journal
/// never settled. Commands are idempotent by key, so re-running one whose
/// outcome was lost is safe.
pub fn recover(
    core: &mut Core,
    j: &mut dyn EffectJournal,
    ports: &mut dyn Ports,
) -> Result<RoundReport> {
    let (msgs, _, _) = read_log(&core.paths)?;
    let (_, cmds) = crate::supervisor_program::replay(&msgs);
    let mut rep = RoundReport::default();
    let mut follow = Vec::new();
    for c in &cmds {
        // The log already recorded this append (a `steer@1`, or a notify from
        // before that contract). Do not send another envelope. A still-pending
        // steer is closed when its child restarts (`AppliedByRestart`).
        if let SupCmd::AppendSteer { seq, .. } = c
            && core
                .model
                .steers
                .get(seq)
                .is_some_and(|s| s.offset.is_some())
        {
            let key = key_of(c);
            match j.begin(&key) {
                Begin::Skip => rep.skipped += 1,
                Begin::Run(g) => {
                    j.finish(&key, g, true);
                    rep.skipped += 1;
                }
                Begin::Unavailable(why) => {
                    rep.failed += 1;
                    follow.push(SupMsg::CmdFailed {
                        key,
                        why: format!("journal unavailable: {why}"),
                    });
                }
                Begin::Held(_) => rep.held += 1,
            }
            continue;
        }
        let fed = dispatch(core, j, ports, c, &mut rep)?;
        // `execute` returns nothing for a Relaunch only when the journal skipped
        // it (a run feeds back `Relaunched` or `CmdFailed`). A settled relaunch is skipped, but if the host
        // died before `Relaunched` was logged the child would sit `Restarting`
        // until its lease ran out (and then be restarted again, spending the
        // budget). The settled row IS the confirmation: feed it, and only while
        // the model is still waiting for it (defence in depth: the fold ignores
        // a confirmation that is not current anyway). The log-only switch also
        // returns nothing, and that is not a confirmation: nobody spawned.
        if fed.is_empty()
            && exec_allowed()
            && let SupCmd::Relaunch {
                child, generation, ..
            } = c
            && core.model.children.get(child).is_some_and(|ch| {
                ch.state == crate::supervisor_program::ChildState::Restarting
                    && ch.generation == *generation
            })
        {
            follow.push(SupMsg::Relaunched {
                child: child.clone(),
                generation: *generation,
            });
        }
        follow.extend(fed);
    }
    feed(core, j, ports, follow, &mut rep)?;
    Ok(rep)
}

// ---- production journal ---------------------------------------------------

/// The effect-journal scope of one host. The root's is unchanged. A nested
/// lead's is its own: its fold numbers steers from 1 as well, so sharing the
/// root's scope made the nested `steer:1` read as settled and it was never
/// sent (rv23 #6, nested leads; found by the live fixture).
pub(crate) fn journal_scope(project: &str, context: Option<&str>) -> String {
    let root = format!("supervise:{}", crate::state::state_key(project));
    match context {
        Some(c) => format!("{root}:{c}"),
        None => root,
    }
}

/// The journal owner for supervisor context `ctx` (its lease key, one live
/// host each) at `sup_epoch`. No pid: a restarted host is recognised by its
/// context and its higher epoch, not by a process id.
pub(crate) fn journal_owner(ctx: &str, sup_epoch: u64) -> String {
    format!("{ctx}@e{sup_epoch}")
}

/// Pure: may `mine` take over a claim owned by `theirs`? Only a claim of the
/// SAME context at a LOWER sup_epoch (the dead predecessor). Another context,
/// an equal or newer epoch, or an owner of another shape is never taken.
pub(crate) fn takes_over(mine: &str, theirs: &str) -> bool {
    let split = |o: &str| {
        o.rsplit_once("@e")
            .and_then(|(c, e)| e.parse::<u64>().ok().map(|e| (c.to_string(), e)))
    };
    match (split(mine), split(theirs)) {
        (Some((mc, me)), Some((tc, te))) => mc == tc && te < me,
        _ => false,
    }
}

/// toron's `ToronJournal` as the effect journal.
pub struct ToronEffectJournal {
    j: toron_core::journal::ToronJournal,
    owner: String,
    lease_ms: i64,
}

impl ToronEffectJournal {
    /// Wrap `j`; claims are owned by `owner`.
    pub fn new(j: toron_core::journal::ToronJournal, owner: &str) -> Self {
        Self {
            j,
            owner: owner.into(),
            lease_ms: 60_000,
        }
    }

    /// Open `scope` on the shared comms index database.
    pub fn open(scope: &str, owner: &str) -> Result<Self> {
        let mb = crate::mail_shim::comms_mailbox()?;
        let pool = mb
            .index_pool()
            .cloned()
            .ok_or_else(|| anyhow!("supervisor journal needs the index pool"))?;
        let rt = crate::mail_shim::runtime()?;
        let db = toron_core::journal::JournalDb::new(pool, scope, 4096);
        Ok(Self::new(
            toron_core::journal::ToronJournal::spawn(db, rt.handle()),
            owner,
        ))
    }
}

impl EffectJournal for ToronEffectJournal {
    fn begin(&mut self, key: &str) -> Begin {
        use toron_core::kernel::{EffectState, JournalStore};
        let row = match self.j.try_record_intent(key, 3) {
            Ok(r) => r,
            Err(e) => return Begin::Unavailable(e.to_string()),
        };
        if row.state.is_terminal() {
            return Begin::Skip;
        }
        let now = chrono::Utc::now().timestamp_millis();
        let claim = |owner: &str| {
            self.j
                .try_claim(key, owner, now + self.lease_ms, now, row.generation)
        };
        let claimed = match claim(&self.owner) {
            Ok(Some(r)) => r,
            // Refused: another owner's lease is live. If that owner is this
            // context's predecessor (a lower sup_epoch, so a host that is
            // dead: the context lease admits one live host and the epoch
            // claim fenced it), take its claim over and run the key once more.
            // The claim keeps the predecessor's owner token until its lease
            // expires (the kernel refuses a cross-owner claim while a lease is
            // live); the fence is the generation the takeover bumps, so the
            // dead host's late settle cannot land. Waiting out the lease
            // instead made the fold count the refusal as a child failure: an
            // extra generation and a spent restart slot per host crash
            // (rv23 S2).
            Ok(None) => match row.owner.as_deref() {
                Some(theirs) if takes_over(&self.owner, theirs) => match claim(theirs) {
                    Ok(Some(r)) => r,
                    Ok(None) => return Begin::Held(theirs.to_string()),
                    Err(e) => return Begin::Unavailable(e.to_string()),
                },
                Some(theirs) => return Begin::Held(theirs.to_string()),
                None => return Begin::Held(String::new()),
            },
            Err(e) => return Begin::Unavailable(e.to_string()),
        };
        if claimed.state == EffectState::Prepared {
            let _ = self
                .j
                .settle(key, claimed.generation, EffectState::InFlight);
        }
        Begin::Run(claimed.generation)
    }

    fn finish(&mut self, key: &str, generation: u64, ok: bool) {
        use toron_core::kernel::{EffectState, JournalStore};
        let to = if ok {
            EffectState::Settled
        } else {
            EffectState::Failed
        };
        let _ = self.j.settle(key, generation, to);
    }
}

/// In-memory journal. Tests drive it; no production path constructs one.
#[cfg(test)]
#[derive(Default)]
pub struct MemJournal {
    /// key -> (settled?, ok, runs)
    pub rows: BTreeMap<String, (bool, bool, u64)>,
}

#[cfg(test)]
impl EffectJournal for MemJournal {
    fn begin(&mut self, key: &str) -> Begin {
        let r = self.rows.entry(key.into()).or_insert((false, false, 0));
        if r.0 {
            return Begin::Skip;
        }
        r.2 += 1;
        Begin::Run(r.2)
    }

    fn finish(&mut self, key: &str, generation: u64, ok: bool) {
        if let Some(r) = self.rows.get_mut(key)
            && r.2 == generation
        {
            r.0 = true;
            r.1 = ok;
        }
    }
}

// ---- production ports -----------------------------------------------------

/// herdr + mail + the captain page, as the supervisor's effect boundary.
pub struct LivePorts {
    cfg: crate::config::Config,
    project: String,
    inbox: crate::inbox_ports::FlywheelPorts,
    herdr_up: Option<bool>,
    facts_seen: BTreeSet<String>,
    /// rv23 #4: the inbox host's facts file, read incrementally.
    deaf: Option<FactsCursor>,
    /// rv23 #4: each lead child's facts file, read incrementally.
    lead_facts: BTreeMap<String, FactsCursor>,
    relaunch_bin: PathBuf,
    /// Set on a nested lead host so its facts file is not the root's.
    context: Option<String>,
    /// rv23 #6: the `sup_epoch` this host holds the lead at. `None` holds
    /// every steer (see [`steer_gate`]).
    lead: Option<u64>,
}

impl LivePorts {
    /// Ports for `project`.
    pub fn new(cfg: &crate::config::Config, project: &str, workspace: &str) -> Self {
        let relaunch_bin = std::env::var_os("FLYWHEEL_SUPERVISOR_RELAUNCH_BIN")
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("flywheel"));
        Self {
            cfg: cfg.clone(),
            project: project.into(),
            inbox: crate::inbox_ports::FlywheelPorts::new(cfg, project, workspace),
            herdr_up: None,
            facts_seen: BTreeSet::new(),
            deaf: None,
            lead_facts: BTreeMap::new(),
            relaunch_bin,
            context: None,
            lead: None,
        }
    }

    /// This host is the lead for `context` (its own facts file and lease).
    /// The lead this host claimed at startup (rv23 #6).
    pub(crate) fn with_lead(mut self, lead: Option<u64>) -> Self {
        self.lead = lead;
        self
    }

    pub fn in_context(mut self, context: &str) -> Self {
        if context_token(context) {
            self.context = Some(context.to_string());
        }
        self
    }

    fn facts_path(&self) -> PathBuf {
        lead_facts_file(&self.project, self.context.as_deref())
    }

    /// The toron context this host leads and steers in (rv23 #6).
    pub(crate) fn lead_context(&self) -> String {
        crate::lead_claim::lead_context(&self.project, self.context.as_deref())
    }

    /// The lead host's pid is dead. No lease file means the lead was never
    /// started here, which is not a death.
    fn lead_host_dead(&self, context: &str) -> bool {
        let ws = supervise_lease_workspace(&self.project, Some(context));
        match doorbell_rt::lease::read_lease(&crate::config::state_dir(), &ws) {
            Some(lease) => !doorbell_rt::lease::pid_alive(lease.pid),
            None => false,
        }
    }

    fn inbox_facts_path(&self) -> PathBuf {
        crate::config::state_dir().join("facts").join(format!(
            "{}-inbox.jsonl",
            crate::state::state_key(&self.project)
        ))
    }

    fn fact(&mut self, key: &str, mut v: Value) -> Result<(), String> {
        if !self.facts_seen.insert(key.to_string()) {
            return Ok(());
        }
        if let Some(o) = v.as_object_mut() {
            o.insert("key".into(), json!(key));
            o.insert("project".into(), json!(self.project));
            o.insert("ts".into(), json!(chrono::Utc::now().to_rfc3339()));
        }
        let path = self.facts_path();
        if let Some(d) = path.parent() {
            crate::private_fs::create_private_dir_all(d).map_err(|e| e.to_string())?;
        }
        let mut f = crate::private_fs::open_private_append(&path).map_err(|e| e.to_string())?;
        writeln!(f, "{v}").map_err(|e| e.to_string())
    }

    fn pane_of(&self, pin: &str) -> Option<String> {
        crate::state::load_spawn_registry(&self.project)
            .into_iter()
            .find(|e| e.mail_identity.as_deref() == Some(pin))
            .map(|e| e.pane_id)
    }

    fn client(&self) -> Option<crate::herdr_shim::Client> {
        crate::herdr_shim::Client::connect(Some(&self.cfg.socket_path())).ok()
    }

    fn page_captain(&self, line: &str) {
        crate::page_captain_dead_stop(&self.cfg, &self.project, line);
    }
}

impl Ports for LivePorts {
    fn tracker_dir(&self) -> Option<PathBuf> {
        Some(crate::project_dir(&self.cfg, &self.project))
    }

    fn observe(&mut self, model: &SupModel, now_ms: u64) -> Vec<SupMsg> {
        let mut out = Vec::new();
        // rv23 #4: the last round's facts are folded by now; record how far
        // they were read.
        let reader = self.context.clone().unwrap_or_else(|| "root".into());
        for c in self.lead_facts.values_mut() {
            c.commit();
        }
        // Lead facts and a dead lease, before the herdr edge, so a lead
        // failure still restarts while herdr reads up.
        let leads: Vec<String> = model
            .children
            .iter()
            .filter(|(_, c)| c.spec.kind == ChildKind::Lead)
            .map(|(id, _)| id.clone())
            .collect();
        for id in leads {
            let dead = self.lead_host_dead(&id);
            let project = self.project.clone();
            let cur = self
                .lead_facts
                .entry(id.clone())
                .or_insert_with(|| FactsCursor::open(lead_facts_file(&project, Some(&id)), &reader));
            out.extend(lead_upward(cur, model, &id, now_ms, dead));
        }
        // herdr edges.
        let up = self.client().is_some();
        if self.herdr_up != Some(up) {
            if self.herdr_up.is_some() || !up {
                out.push(if up {
                    SupMsg::HerdrUp { now_ms }
                } else {
                    SupMsg::HerdrDown { now_ms }
                });
            }
            self.herdr_up = Some(up);
        }
        // Crews the registry knows that the model does not.
        for e in crate::state::load_spawn_registry(&self.project) {
            let Some(pin) = e.mail_identity.clone() else {
                continue;
            };
            if model.children.contains_key(&pin) {
                continue;
            }
            let npub = self.inbox.npub_of(&pin);
            out.push(SupMsg::Spawn {
                spec: ChildSpec {
                    id: pin,
                    npub,
                    restart: Restart::Transient,
                    kind: ChildKind::Crew,
                    role: None,
                },
                now_ms,
            });
        }
        // Cursors, for children with a steer in flight (the ack).
        for s in model.steers.values() {
            if s.state != crate::supervisor_program::SteerState::Pending || s.offset.is_none() {
                continue;
            }
            let Some(npub) = model
                .children
                .get(&s.child)
                .and_then(|c| c.spec.npub.clone())
            else {
                continue;
            };
            if let Some(store) = self.inbox.store().cloned() {
                let (_, cursor) = crate::mail_shim::block_on(async move {
                    crate::inbox_ports::head_and_cursor(&store, &npub).await
                });
                out.push(SupMsg::Cursor {
                    child: s.child.clone(),
                    cursor,
                });
            }
        }
        // The inbox host's deaf facts, by the npub they name.
        let deaf_path = self.inbox_facts_path();
        let deaf = self
            .deaf
            .get_or_insert_with(|| FactsCursor::open(deaf_path, &reader));
        deaf.commit();
        let (buf, end) = deaf.read_new();
        deaf.consume(end);
        {
            {
                for l in buf.split_inclusive('\n') {
                    let Ok(v) = serde_json::from_str::<Value>(l.trim()) else {
                        continue;
                    };
                    if v["kind"] != "deaf" {
                        continue;
                    }
                    let (Some(npub), Some(key)) = (v["npub"].as_str(), v["key"].as_str()) else {
                        continue;
                    };
                    if let Some((id, _)) = model
                        .children
                        .iter()
                        .find(|(_, c)| c.spec.npub.as_deref() == Some(npub))
                    {
                        out.push(SupMsg::Deaf {
                            child: id.clone(),
                            key: key.to_string(),
                            now_ms,
                        });
                    }
                }
            }
        }
        out
    }

    fn relaunch(&mut self, child: &str, generation: u64, kind: ChildKind) -> Result<(), String> {
        if child == PASS {
            // The next interval is the relaunch of the pass child.
            return Ok(());
        }
        // Lead argv is a nested supervise. This host only runs under the
        // comms flag (`run_supervise`); a second host is refused by the lease.
        let st = std::process::Command::new(&self.relaunch_bin)
            .args(relaunch_argv(&self.project, child, kind))
            .env("FLYWHEEL_SUPERVISOR_GENERATION", generation.to_string())
            .status()
            .map_err(|e| format!("spawn relaunch: {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("relaunch {child}: {st}"))
        }
    }

    fn exit(&mut self, child: &str, _generation: u64) -> Result<(), String> {
        if child == PASS {
            return Ok(());
        }
        let pane = self
            .pane_of(child)
            .ok_or_else(|| format!("no pane for {child}"))?;
        let c = self.client().ok_or("herdr unavailable")?;
        crate::pane_write::write(
            &crate::pane_write::HerdrPanes(&c),
            &pane,
            &crate::pane_write::PaneWrite::Exit,
        )
        .map_err(|e| format!("{e:#}"))
    }

    fn append_steer(&mut self, s: &SteerSend) -> Result<u64, String> {
        // rv23 #6: never steer without a lead claimed at this epoch.
        steer_gate(self.lead, s.sup_epoch)?;
        // The delta comes from the side store by sha and is refused unless its
        // bytes hash to that sha. It rides the steer@1 body, never a pane.
        // The supervisor log keeps the sha (the fold's message), not these bytes.
        let delta = match &s.delta_sha {
            Some(sha) => Some(
                crate::steer_delta::load(&CtxPaths::default_dir(), &self.project, sha)
                    .map_err(|e| format!("{e:#}"))?,
            ),
            None => None,
        };
        let to = self
            .inbox
            .npub_of(&s.child)
            .ok_or_else(|| format!("no npub for {}", s.child))?;
        // The steer's context and `from` are this host's lead context and
        // holder: a nested lead steers in `project/<sub>`, not the root's.
        let ctx = self.lead_context();
        let producer = crate::lead_claim::lead_holder(&ctx);
        let epoch = self.inbox.claim_epoch_as(&producer)?;
        let env = steer_envelope(&ctx, &producer, epoch, &to, s, delta.as_ref())?;
        self.inbox.append_env(env)
    }

    fn assign(&mut self, a: &AssignSend) -> Result<u64, String> {
        let to = require_npub(&a.child, self.inbox.npub_of(&a.child))?;
        let (producer, epoch) = self.inbox.claim_producer_epoch()?;
        let env = assign_envelope(
            &self.project,
            &producer,
            epoch,
            &to,
            &a.task,
            &a.child,
            a.generation,
        )?;
        self.inbox.append_env(env)
    }

    fn escalate(&mut self, n: u64, reason: &str, evidence: &str) -> Result<(), String> {
        let reason_s = crate::redact::flatten(reason);
        let evidence_s = crate::redact::flatten(evidence);
        let evidence_v: Vec<String> = if evidence_s.is_empty() {
            Vec::new()
        } else {
            vec![evidence_s.clone()]
        };
        // No captain pin: there is no npub to address. The page below is the
        // same sink as before. A bound captain gets escalate@1 as well.
        // The body is the flattened text, so `toron next` does not show the token.
        if let Some(cap) = crate::state::load_captain_alias(&self.project) {
            let to = self
                .inbox
                .npub_of(&cap)
                .ok_or_else(|| format!("no npub for {cap}"))?;
            let (producer, epoch) = self.inbox.claim_producer_epoch()?;
            let env =
                escalate_envelope(&self.project, &producer, epoch, &to, &reason_s, &evidence_v)?;
            self.inbox.append_env(env)?;
        }
        let line = crate::redact::escalation_notice(&self.project, reason, evidence);
        eprintln!("{line}");
        self.fact(
            &format!("escalate:{n}:{reason_s}"),
            json!({"kind": "escalate", "reason": reason_s, "evidence": evidence_s}),
        )?;
        self.page_captain(&line);
        Ok(())
    }

    fn park(&mut self, child: &str, incarnations: u32) -> Result<(), String> {
        let child_s = crate::redact::flatten(child);
        let line = crate::redact::park_notice(&self.project, child, incarnations);
        eprintln!("{line}");
        self.fact(
            &format!("park:{child_s}"),
            json!({"kind": "park", "child": child_s, "incarnations": incarnations}),
        )?;
        self.page_captain(&line);
        Ok(())
    }

    fn page(&mut self, kind: &str, pin: &str, line: &str) -> Result<(), String> {
        let shown = crate::redact::present(line);
        eprintln!("{shown} — paging the captain once");
        self.fact(
            &format!("page:{kind}:{pin}:{}", chrono::Utc::now().timestamp()),
            json!({"kind": "page", "page": kind, "pin": pin}),
        )?;
        match kind {
            // The health pages are their own subject: the captain can tell a
            // dead dispatcher from a dead stop without reading the body.
            "dead_doorbell" => crate::page_captain_mail(
                &self.cfg,
                &self.project,
                "[loop-mode supervise] dead doorbell",
                &shown,
            ),
            "stale_heartbeat" => crate::page_captain_mail(
                &self.cfg,
                &self.project,
                "[loop-mode supervise] stale heartbeat",
                &shown,
            ),
            _ => self.page_captain(&shown),
        }
        Ok(())
    }

    fn reject(&mut self, n: u64, cause: &str, child: &str) -> Result<(), String> {
        let cause_s = crate::redact::flatten(cause);
        let child_s = crate::redact::flatten(child);
        eprintln!("{}", crate::redact::reject_notice(cause, child));
        self.fact(
            &format!("reject:{n}:{cause_s}"),
            json!({"kind": "reject", "cause": cause_s, "child": child_s}),
        )
    }
}

// ---- the host loop (`loop-mode supervise` under the comms flag) -----------

/// Settings from the environment (fixtures shorten these; production uses
/// the defaults).
#[derive(Clone, Debug)]
pub struct HostCfg {
    /// Loop period.
    pub tick_ms: u64,
    /// Steer ack window / child lease overrides.
    pub ack_ms: Option<u64>,
    /// Child lease override.
    pub lease_ms: Option<u64>,
    /// Do not run loop passes (crews only).
    pub passes: bool,
    /// What a failure restarts; `None` = the tier default (`one_for_one`).
    pub strategy: Option<crate::supervisor_program::Strategy>,
    /// Wake a round on a change to the files its reads come from (4b.7).
    /// Off = the plain tick, nothing else differs.
    pub hints: bool,
    /// `FLYWHEEL_SUPERVISOR_CLOCK_SCALE`. 1 in production. Captured at
    /// `from_env` so a later test cannot change a policy already built.
    pub clock_scale: u64,
    /// `FLYWHEEL_SUPERVISOR_JOIN`. `None` keeps [`Join::All`].
    pub join: Option<crate::supervisor_program::Join>,
    /// `FLYWHEEL_SUPERVISOR_MAX_WALL_MS`. `None` does not gate on the wall.
    pub max_wall_ms: Option<u64>,
}

impl HostCfg {
    /// Read `FLYWHEEL_SUPERVISOR_{TICK_MS,ACK_MS,LEASE_MS,PASSES,STRATEGY,HINTS}`
    /// and `FLYWHEEL_SUPERVISOR_CLOCK_SCALE` (fixtures only; 1 is production).
    pub fn from_env() -> Self {
        let num = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
        Self {
            tick_ms: num("FLYWHEEL_SUPERVISOR_TICK_MS").unwrap_or(1_000).max(10),
            ack_ms: num("FLYWHEEL_SUPERVISOR_ACK_MS"),
            lease_ms: num("FLYWHEEL_SUPERVISOR_LEASE_MS"),
            passes: std::env::var("FLYWHEEL_SUPERVISOR_PASSES").map_or(true, |v| v != "off"),
            strategy: std::env::var("FLYWHEEL_SUPERVISOR_STRATEGY")
                .ok()
                .and_then(|v| crate::supervisor_program::Strategy::parse(&v)),
            hints: std::env::var("FLYWHEEL_SUPERVISOR_HINTS").map_or(true, |v| v != "off"),
            clock_scale: clock_scale(),
            join: std::env::var("FLYWHEEL_SUPERVISOR_JOIN")
                .ok()
                .and_then(|v| crate::supervisor_program::Join::parse(&v)),
            max_wall_ms: num("FLYWHEEL_SUPERVISOR_MAX_WALL_MS"),
        }
    }

    fn policy(&self) -> Option<Policy> {
        let scale = self.clock_scale.max(1);
        if self.ack_ms.is_none()
            && self.lease_ms.is_none()
            && self.strategy.is_none()
            && self.join.is_none()
            && self.max_wall_ms.is_none()
            && scale == 1
        {
            return None;
        }
        let mut p = Policy::for_tier(Tier::Lead);
        if let Some(s) = self.strategy {
            p.strategy = s;
        }
        if let Some(a) = self.ack_ms {
            p.ack_ms = a;
        }
        let lease = self.lease_ms.unwrap_or(p.lease_ms);
        if self.lease_ms.is_some() {
            p.lease_ms = lease;
        }
        if let Some(j) = self.join {
            p.join = j;
        }
        if let Some(w) = self.max_wall_ms {
            p.max_wall_ms = Some(w);
        }
        p.t_orphan_ms = crate::supervisor_program::scaled_orphan_ms(lease, scale);
        Some(p)
    }
}

/// `br hold --kind captain` when the escalation names a bead.
///
/// A missing `br` is loud and does not fail the page. `FLYWHEEL_BR_BIN`
/// that exits non-zero fails the command. No bead id means no process.
///
/// rv23 #5 (review S3): the ids come from untrusted escalation text, so the
/// hold is gated like every other exec. `FLYWHEEL_ALLOW_EXEC=0` pages but
/// does not run `br` (a suppressed line names it). The run is bounded by
/// `br_hold_timeout()`: a hung `br` is killed at the deadline and fails the
/// command instead of parking the supervisor loop.
fn captain_hold(
    evidence: &str,
    reason: &str,
    cwd: Option<&Path>,
    rep: &mut RoundReport,
) -> Result<(), String> {
    let Some(argv) = crate::supervisor_program::bead_hold_argv(evidence, reason) else {
        return Ok(());
    };
    if !exec_allowed() {
        let line = format!(
            "suppressed br hold {}: FLYWHEEL_ALLOW_EXEC=0",
            argv[5..].join(" ")
        );
        eprintln!("flywheel supervisor: {line}");
        rep.suppressed.push(line);
        return Ok(());
    }
    let pinned = std::env::var("FLYWHEEL_BR_BIN").ok();
    let program = pinned.as_deref().unwrap_or("br");
    match run_bounded(program, &argv, cwd, br_hold_timeout()) {
        Err(Bounded::TimedOut(after)) if pinned.is_some() => Err(format!(
            "br hold timed out after {}ms (killed)",
            after.as_millis()
        )),
        Err(Bounded::TimedOut(after)) => {
            eprintln!(
                "flywheel supervisor: br hold timed out after {}ms (killed); the escalate page stands",
                after.as_millis()
            );
            Ok(())
        }
        Err(Bounded::Spawn(e)) if pinned.is_some() => Err(format!("br hold: {e}")),
        Err(Bounded::Spawn(e)) => {
            eprintln!("flywheel supervisor: br hold not run ({e}); the escalate page stands");
            Ok(())
        }
        Ok(st) if st.success() => Ok(()),
        Ok(st) if pinned.is_some() => Err(format!("br hold exited {st}")),
        Ok(st) => {
            eprintln!("flywheel supervisor: br hold failed ({st}); the escalate page stands");
            Ok(())
        }
    }
}

use std::time::Duration;

/// Default deadline for one `br hold`. `FLYWHEEL_BR_HOLD_TIMEOUT_MS`
/// overrides it (tests, slow trackers).
const BR_HOLD_TIMEOUT: Duration = Duration::from_secs(10);

fn br_hold_timeout() -> Duration {
    std::env::var("FLYWHEEL_BR_HOLD_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(BR_HOLD_TIMEOUT, Duration::from_millis)
}

enum Bounded {
    Spawn(std::io::Error),
    TimedOut(Duration),
}

/// Run `program argv` with stdin closed and wait at most `timeout`. Past
/// the deadline the child is killed and reaped, so nothing is left behind.
fn run_bounded(
    program: &str,
    argv: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<std::process::ExitStatus, Bounded> {
    let mut started = None;
    run_bounded_on(program, argv, cwd, timeout, &mut || {
        started.get_or_insert_with(std::time::Instant::now).elapsed()
    })
}

/// [`run_bounded`] on an injected clock: `elapsed` says how long the child
/// has run (first asked right after the spawn). Tests drive the deadline
/// with it instead of racing the machine's load.
fn run_bounded_on(
    program: &str,
    argv: &[String],
    cwd: Option<&Path>,
    timeout: Duration,
    elapsed: &mut dyn FnMut() -> Duration,
) -> Result<std::process::ExitStatus, Bounded> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(argv).stdin(std::process::Stdio::null());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().map_err(Bounded::Spawn)?;
    elapsed();
    loop {
        match child.try_wait() {
            Ok(Some(st)) => return Ok(st),
            Ok(None) if elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Bounded::TimedOut(elapsed()));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Bounded::Spawn(e));
            }
        }
    }
}

fn clock_scale() -> u64 {
    std::env::var("FLYWHEEL_SUPERVISOR_CLOCK_SCALE")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(1)
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// Startup claim. The store is the authority when `inbox_lead` can be read,
/// unless `FLYWHEEL_SUPERVISOR_EPOCH=fold`. The fold's counter runs only on
/// the other side of that gate.
fn claim_for_host(core: &mut Core, project: &str, lead_ctx: &str, me: &str) -> Result<HostClaim> {
    let forced = std::env::var("FLYWHEEL_SUPERVISOR_EPOCH").ok();
    let opened = crate::comms_send::store()
        .ok()
        .map(|store| store.with_operator(crate::inbox_ports::system_producer(project)));
    claim_with(core, project, lead_ctx, me, forced.as_deref(), opened)
}

/// What the startup claim settled: the fold's epoch, and the `sup_epoch` a
/// `lead@1` row on record names for this host's steering key, if one does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HostClaim {
    pub epoch: u64,
    /// `Some(e)`: toron's lead row names [`crate::lead_claim::lead_holder`]
    /// at `e`. `None`: no lead was claimed, so this host must not steer.
    pub lead: Option<u64>,
}

/// [`claim_for_host`] with the store and the switch passed in.
///
/// rv23 #6 (toron F7): the lead holder is the key the host steers with
/// ([`crate::lead_claim::lead_holder`]), never `me`; `me` (it carries the
/// pid) stays the fold's claimant. Fold mode claims the lead too: the fold
/// picks the epoch and the store records `lead@1` at it, so no mode steers
/// without a lead.
///
/// `lead_ctx` is [`crate::lead_claim::lead_context`]: the root claims the
/// project's lead, a nested lead claims `project/<sub>` and never the root's.
pub(crate) fn claim_with(
    core: &mut Core,
    project: &str,
    lead_ctx: &str,
    me: &str,
    forced: Option<&str>,
    opened: Option<toron_core::inbox::InboxStore>,
) -> Result<HostClaim> {
    let readable = match &opened {
        Some(store) => crate::mail_shim::block_on(store.lead_of(lead_ctx)).is_ok(),
        None => false,
    };
    if forced == Some("toron") && (opened.is_none() || !readable) {
        bail!("FLYWHEEL_SUPERVISOR_EPOCH=toron but inbox_lead cannot be read");
    }
    let holder = crate::lead_claim::lead_holder(lead_ctx);
    match crate::lead_claim::epoch_authority(forced, readable) {
        crate::lead_claim::EpochAuthority::Fold => {
            let epoch = core.claim(me)?;
            let lead = match opened.as_ref().filter(|_| readable) {
                Some(store) => match crate::mail_shim::block_on(
                    crate::lead_claim::append_lead_in(store, project, lead_ctx, &holder, epoch),
                ) {
                    Ok(()) => Some(epoch),
                    Err(e) => {
                        eprintln!(
                            "flywheel supervisor: fold epoch {epoch} has no lead@1 ({e}); steers are held"
                        );
                        None
                    }
                },
                None => {
                    eprintln!(
                        "flywheel supervisor: no readable lead table; steers are held (no lead claim)"
                    );
                    None
                }
            };
            Ok(HostClaim { epoch, lead })
        }
        crate::lead_claim::EpochAuthority::Toron => {
            let store = opened.context("comms store required for a lead claim")?;
            let step = crate::mail_shim::block_on(crate::lead_claim::sync_lead_in(
                &store,
                project,
                lead_ctx,
                &holder,
                core.model.accepted_epoch,
            ));
            let epoch = apply_lead(core, me, step)?;
            Ok(HostClaim {
                epoch,
                lead: Some(epoch),
            })
        }
    }
}

/// rv23 #6: a steer goes out only under a lead this host claimed at the
/// steer's own `sup_epoch`. toron would refuse it anyway (NoLead,
/// NotLeadHolder, StaleSupEpoch); refusing here names why before a send.
pub(crate) fn steer_gate(lead: Option<u64>, sup_epoch: u64) -> Result<(), String> {
    match lead {
        Some(e) if e == sup_epoch => Ok(()),
        Some(e) => Err(format!(
            "steer at sup_epoch {sup_epoch} but this host holds the lead at {e}; not sent"
        )),
        None => Err(format!(
            "no lead claim: this host holds no lead@1, so the steer at sup_epoch {sup_epoch} is not sent"
        )),
    }
}

/// Fold the epoch [`crate::lead_claim::sync_lead`] returned. A stale or
/// pending refusal is logged as the fold's own reject and does not move
/// `accepted_epoch`.
pub(crate) fn apply_lead(
    core: &mut Core,
    holder: &str,
    step: Result<crate::lead_claim::LeadStep, crate::lead_claim::LeadError>,
) -> Result<u64> {
    use crate::lead_claim::LeadError;
    match step {
        Ok(
            crate::lead_claim::LeadStep::CatchUp(epoch)
            | crate::lead_claim::LeadStep::Appended(epoch),
        ) => core.claim_stored(holder, epoch),
        Err(LeadError::Stale | LeadError::Pending) => {
            let current = core.model.accepted_epoch;
            let _ = core.claim_stored(holder, current);
            bail!("toron refused the lead claim")
        }
        Err(LeadError::Store(e)) => bail!("{e}"),
    }
}

/// `loop-mode supervise` under the comms flag: the same cadence, rehosted
/// on [`SupervisorProgram`]. Returns when the supervisor failed (intensity
/// exceeded: escalated, subtree cancelled) or was superseded.
pub fn run_supervise(
    cfg: &crate::config::Config,
    project: &str,
    work: bool,
    interval_secs: u64,
    to: &str,
    stall_minutes: u64,
    context: Option<&str>,
) -> Result<()> {
    if let Some(c) = context
        && !context_token(c)
    {
        bail!("--context must be letters, digits, '-' or '_'");
    }
    let hc = HostCfg::from_env();
    let dir = CtxPaths::default_dir();
    let paths = match context {
        Some(c) => CtxPaths::with_context(&dir, project, c),
        None => CtxPaths::new(&dir, project),
    };
    let me = match context {
        Some(c) => format!("supervise-{project}-{c}-{}", std::process::id()),
        None => format!("supervise-{project}-{}", std::process::id()),
    };
    // One live host per context (the log has one writer). A lead does not
    // take the root's lease, so the two processes can run together.
    let lease_ws = supervise_lease_workspace(project, context);
    let sd = crate::config::state_dir();
    let now_s = || chrono::Utc::now().timestamp().max(0) as u64;
    let mut lease = match doorbell_rt::lease::acquire_lease(&sd, &lease_ws, now_s()) {
        Ok(l) => l,
        Err(doorbell_rt::lease::LeaseDenied::Held(o)) => {
            bail!(
                "another supervisor host (pid {}) holds {project}; not starting a second",
                o.pid
            )
        }
        Err(_) => bail!("cannot write the supervisor lease"),
    };
    let result = host_loop(
        cfg,
        project,
        work,
        interval_secs,
        to,
        stall_minutes,
        &hc,
        &paths,
        &me,
        &lease_ws,
        &mut lease,
        context,
    );
    doorbell_rt::lease::release_lease(&sd, &lease_ws, &lease);
    result
}

#[allow(clippy::too_many_arguments)]
fn host_loop(
    cfg: &crate::config::Config,
    project: &str,
    work: bool,
    interval_secs: u64,
    to: &str,
    stall_minutes: u64,
    hc: &HostCfg,
    paths: &CtxPaths,
    me: &str,
    lease_ws: &str,
    lease: &mut doorbell_rt::lease::OwnerLease,
    context: Option<&str>,
) -> Result<()> {
    let mut core = Core::open(paths.clone(), me, Tier::Lead, hc.policy())?;
    // rv23 #6: a nested lead claims and steers in its own context.
    let lead_ctx = crate::lead_claim::lead_context(project, context);
    let claim = claim_for_host(&mut core, project, &lead_ctx, me)?;
    let epoch = claim.epoch;
    let mut journal = ToronEffectJournal::open(
        &journal_scope(project, context),
        &journal_owner(lease_ws, epoch),
    )?;
    let mut ports = LivePorts::new(cfg, project, project).with_lead(claim.lead);
    if let Some(c) = context {
        ports = ports.in_context(c);
    }
    eprintln!(
        "flywheel supervisor: {project} hosted on SupervisorProgram (sup_epoch {epoch}, {} children replayed, MaxR {}/{}s) — comms flag on",
        core.model.children.len(),
        core.model.policy.max_r,
        core.model.policy.max_t_ms / 1000
    );
    let rec = recover(&mut core, &mut journal, &mut ports)?;
    if rec.ran + rec.skipped > 0 {
        eprintln!(
            "flywheel supervisor: recovery ran {} command(s), {} already settled",
            rec.ran, rec.skipped
        );
    }
    let exe = std::env::current_exe().context("resolve the flywheel executable")?;
    let project_dir = crate::project_dir(cfg, project);
    let stall_secs = stall_minutes.max(1) * 60;
    let mut extra = Vec::new();
    if hc.passes && !core.model.children.contains_key(PASS) {
        extra.push(SupMsg::Spawn {
            spec: ChildSpec {
                id: PASS.into(),
                npub: None,
                restart: Restart::Permanent,
                kind: ChildKind::Crew,
                role: None,
            },
            now_ms: now_ms(),
        });
    }
    // 4b.7: the tick stays the truth; a change to the files a round reads wakes
    // the next round early. Dropping the channel (hints off) is the plain tick.
    let (hint_tx, hint_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let _watcher = if hc.hints {
        let (_, index_db) = crate::mail_shim::mailbox_roots();
        Some(crate::supervisor_hints::Watcher::spawn(
            crate::supervisor_hints::watched_paths(&index_db, &paths.inbox),
            hint_tx,
            std::time::Duration::from_millis(crate::supervisor_hints::POLL_MS),
            std::time::Duration::from_millis(crate::supervisor_hints::MIN_GAP_MS),
        ))
    } else {
        drop(hint_tx);
        None
    };
    let mut pass: Option<std::process::Child> = None;
    let mut last_pass_end =
        std::time::Instant::now() - std::time::Duration::from_secs(interval_secs);
    loop {
        let now = now_ms();
        if let Some(c) = pass.as_mut()
            && let Some(st) = c.try_wait().context("poll the pass child")?
        {
            pass = None;
            last_pass_end = std::time::Instant::now();
            extra.push(SupMsg::PassExit {
                ok: st.success(),
                now_ms: now,
            });
            // The dead-stop and wedge probes ride the end of a pass, as before.
            extra.extend(probe_msgs(
                cfg,
                project,
                &project_dir,
                stall_secs,
                &core.model,
                now,
            ));
        }
        let pass_live = core
            .model
            .children
            .get(PASS)
            .is_some_and(|c| c.state.live_for_host());
        if hc.passes
            && pass.is_none()
            && pass_live
            && last_pass_end.elapsed() >= std::time::Duration::from_secs(interval_secs)
        {
            let argv = crate::durable::supervise_pass_argv(project, work, to);
            pass = Some(
                std::process::Command::new(&exe)
                    .args(&argv)
                    .spawn()
                    .with_context(|| format!("spawn `{}`", argv.join(" ")))?,
            );
        }
        let rep = round(
            &mut core,
            &mut journal,
            &mut ports,
            std::mem::take(&mut extra),
            now,
        )?;
        if rep.failed > 0 {
            eprintln!(
                "flywheel supervisor: {} command(s) failed this round",
                rep.failed
            );
        }
        doorbell_rt::lease::refresh_lease(
            &crate::config::state_dir(),
            lease_ws,
            lease,
            chrono::Utc::now().timestamp().max(0) as u64,
        );
        match core.model.status {
            crate::supervisor_program::Status::Failed => {
                if let Some(mut c) = pass.take() {
                    let _ = c.kill();
                }
                bail!(
                    "loop-mode supervise stopping: restart intensity exceeded in {project} ({} restarts inside {}s, max {}); the subtree is cancelled and the captain was paged — fix the cause, then re-run supervise",
                    core.model.restarts.len(),
                    core.model.policy.max_t_ms / 1000,
                    core.model.policy.max_r
                );
            }
            crate::supervisor_program::Status::Superseded => {
                if let Some(mut c) = pass.take() {
                    let _ = c.kill();
                }
                bail!(
                    "loop-mode supervise stopping: superseded by {} at sup_epoch {}",
                    core.model.leader,
                    core.model.accepted_epoch
                );
            }
            crate::supervisor_program::Status::Running => {}
            crate::supervisor_program::Status::Halted => {
                if let Some(mut c) = pass.take() {
                    let _ = c.kill();
                }
                bail!(
                    "loop-mode supervise stopping: halt cancelled the subtree in {project} at sup_epoch {}",
                    core.model.accepted_epoch
                );
            }
        }
        crate::supervisor_hints::wait(&hint_rx, std::time::Duration::from_millis(hc.tick_ms));
    }
}

fn probe_msgs(
    cfg: &crate::config::Config,
    project: &str,
    project_dir: &Path,
    stall_secs: u64,
    model: &SupModel,
    now_ms: u64,
) -> Vec<SupMsg> {
    let now = now_ms / 1000;
    let mut out = Vec::new();
    let probe = crate::cmd::captain::probe_stall(cfg, project, project_dir, now, stall_secs);
    let action = crate::durable::dead_stop_action(&crate::durable::DeadStopInputs {
        work_remaining: probe.work_remaining,
        fleet_knowable: probe.fleet_knowable,
        any_pane_working: probe.any_pane_working,
        last_commit_age_secs: probe.last_commit_age_secs,
        stall_secs,
        already_escalated: model.stalled,
    });
    match action {
        crate::durable::DeadStopAction::Escalate(kind) => {
            let age = probe.last_commit_age_secs.unwrap_or(0);
            let line = match kind {
                crate::durable::DeadStopKind::SilentPanes => format!(
                    "loop-mode supervise: DEAD STOP in {project} — no pane working and no commit for {age}s (>{stall_secs}s) with work remaining"
                ),
                crate::durable::DeadStopKind::StuckWorking => format!(
                    "loop-mode supervise: STUCK PANES in {project} — pane(s) report working but nothing moved for {age}s (>{stall_secs}s)"
                ),
            };
            out.push(SupMsg::Stalled { line });
        }
        crate::durable::DeadStopAction::Clear if model.stalled => out.push(SupMsg::WorkMoved),
        _ => {}
    }
    if probe.last_commit_age_secs.is_some_and(|a| a < stall_secs) {
        out.push(SupMsg::WorkMoved);
    }
    let paged: std::collections::HashSet<String> = model.paged.iter().cloned().collect();
    for a in crate::probe_pane_wedges(cfg, project, project_dir, now, stall_secs, &paged) {
        if let crate::durable::PaneWedgeAction::Page {
            pin,
            pane_id,
            error,
        } = a
        {
            out.push(SupMsg::Wedged {
                line: crate::redact::wedge_notice(
                    project,
                    &pin,
                    &pane_id,
                    &error,
                    probe.last_commit_age_secs.unwrap_or(0),
                ),
                pin,
            });
        }
    }
    // Hardening slice 2 (bd-c0ha): a dead doorbell dispatcher or a stale
    // heartbeat on a binding herdr may clear pages once, then holds. The
    // model is the hold, so a replayed pass folds the same set.
    let mut hold = crate::health_page::HealthHold {
        dead_doorbells: model.dead_doorbells.clone(),
        stale_pins: model.stale_pins.clone(),
    };
    let presence = std::fs::read_to_string(crate::mail_shim::presence_path()).unwrap_or_default();
    let rows = crate::health_page::registry_rows(cfg, project);
    let live_panes = crate::live_herdr_panes(cfg);
    for event in crate::health_page::probe_events(
        &crate::config::state_dir(),
        project,
        now,
        &presence,
        &rows,
        live_panes.as_ref(),
        &mut hold,
    ) {
        out.push(match event {
            crate::health_page::HealthEvent::DeadDoorbell { label, line } => {
                SupMsg::DeadDoorbell { label, line }
            }
            crate::health_page::HealthEvent::DoorbellRecovered { label } => {
                SupMsg::DoorbellRecovered { label }
            }
            crate::health_page::HealthEvent::StaleHeartbeat { pin, line } => {
                SupMsg::StaleHeartbeat { pin, line }
            }
            crate::health_page::HealthEvent::HeartbeatRecovered { pin } => {
                SupMsg::HeartbeatRecovered { pin }
            }
        });
    }
    out
}

// ---- the operator/crew CLI (`flywheel supervisor ...`) --------------------

/// `flywheel supervisor` subcommands.
#[derive(clap::Subcommand, Debug)]
pub enum SupervisorCmd {
    /// A crew (or a script) reports a fact for the supervisor's log.
    Fact {
        project: String,
        /// Child (pin).
        #[arg(long)]
        child: String,
        /// progress | done | exit | proved | usage
        #[arg(long)]
        kind: String,
        /// The incarnation the report is for (required for progress/done/proved).
        #[arg(long, default_value_t = 0)]
        generation: u64,
        /// `usage` only. Self-reported. Never a cost gate.
        #[arg(long, default_value_t = 0)]
        input_tokens: u64,
        /// `usage` only.
        #[arg(long, default_value_t = 0)]
        output_tokens: u64,
        /// `usage` only, micro-dollars.
        #[arg(long, default_value_t = 0)]
        usd_micros: u64,
    },
    /// Steer a child: redirect | pause | resume | cancel.
    Steer {
        project: String,
        #[arg(long)]
        child: String,
        /// Nested lead (its own log and `project/<lead>` lead). Omit for the
        /// root host.
        #[arg(long)]
        context: Option<String>,
        #[arg(long)]
        op: String,
        /// The sup_epoch the sender holds (default: the log's accepted one).
        #[arg(long)]
        sup_epoch: Option<u64>,
        /// redirect only: a JSON file with the typed delta (goal, acceptance,
        /// scope.paths, priority, budget), at most 8 KiB.
        #[arg(long)]
        delta_file: Option<PathBuf>,
    },
    /// Print the CrewView from the log (read-only; replays it).
    View {
        project: String,
        #[arg(long)]
        json: bool,
    },
    /// Whether the marked supervisor hook block is in `file`.
    Hooks {
        /// Operator file to read. Omit to use `FLYWHEEL_SUPERVISOR_HOOK_FILE`.
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Cancel this context's subtree. Same door as `flywheel halt`.
    Halt {
        project: String,
        /// Nested lead. Omit for the root host.
        #[arg(long)]
        context: Option<String>,
        /// Epoch the sender holds. Omit to use the log's accepted epoch.
        #[arg(long)]
        sup_epoch: Option<u64>,
        /// bd-4fgc: stable request key for this halt (same slot contract as
        /// `flywheel halt --request-key`).
        #[arg(long)]
        request_key: Option<String>,
    },
}

/// Parse a steer op word.
pub fn parse_op(s: &str) -> Result<SteerOp> {
    Ok(match s {
        "redirect" => SteerOp::Redirect,
        "pause" => SteerOp::Pause,
        "resume" => SteerOp::Resume,
        "cancel" => SteerOp::Cancel,
        other => bail!("unknown steer op {other:?} (redirect|pause|resume|cancel)"),
    })
}

/// Run a `flywheel supervisor` subcommand.
pub fn run_cli(cmd: &SupervisorCmd) -> Result<()> {
    match cmd {
        SupervisorCmd::Fact {
            project,
            child,
            kind,
            generation,
            input_tokens,
            output_tokens,
            usd_micros,
        } => {
            let p = CtxPaths::new(&CtxPaths::default_dir(), project);
            let now_ms = now_ms();
            let msg = match kind.as_str() {
                "progress" => SupMsg::Progress {
                    child: child.clone(),
                    generation: *generation,
                    now_ms,
                },
                "done" => SupMsg::Done {
                    child: child.clone(),
                    generation: *generation,
                    now_ms,
                },
                "exit" => SupMsg::Exit {
                    child: child.clone(),
                    now_ms,
                },
                "proved" => SupMsg::Proved {
                    child: child.clone(),
                    generation: *generation,
                    now_ms,
                },
                "usage" => SupMsg::Usage {
                    child: child.clone(),
                    input_tokens: *input_tokens,
                    output_tokens: *output_tokens,
                    usd_micros: *usd_micros,
                    now_ms,
                },
                other => bail!("unknown fact kind {other:?} (progress|done|exit|proved|usage)"),
            };
            append_input(&p, &msg)?;
            println!("queued {kind} for {child}");
            Ok(())
        }
        SupervisorCmd::Steer {
            project,
            child,
            context,
            op,
            sup_epoch,
            delta_file,
        } => {
            let dir = CtxPaths::default_dir();
            let p = match context.as_deref() {
                Some(c) if !c.is_empty() => {
                    if !context_token(c) {
                        bail!("--context must be letters, digits, '-' or '_'");
                    }
                    CtxPaths::with_context(&dir, project, c)
                }
                _ => CtxPaths::new(&dir, project),
            };
            let op = parse_op(op)?;
            // Validate and stash the delta BEFORE queueing the steer: a bad
            // file queues nothing.
            let delta_sha = match (op, delta_file) {
                (SteerOp::Redirect, Some(f)) => {
                    let raw = std::fs::read(f).with_context(|| format!("read {}", f.display()))?;
                    let d = crate::steer_delta::parse(&raw)?;
                    Some(crate::steer_delta::store(
                        &CtxPaths::default_dir(),
                        project,
                        &d,
                    )?)
                }
                (SteerOp::Redirect, None) => {
                    bail!("a redirect needs --delta-file (the new goal, as JSON); nothing queued")
                }
                (_, Some(_)) => bail!("--delta-file is for redirect only; nothing queued"),
                (_, None) => None,
            };
            let epoch = match sup_epoch {
                Some(e) => *e,
                None => {
                    let (msgs, _, _) = read_log(&p)?;
                    crate::supervisor_program::replay(&msgs).0.accepted_epoch
                }
            };
            append_input(
                &p,
                &SupMsg::Control {
                    op,
                    child: child.clone(),
                    sup_epoch: epoch,
                    delta_sha,
                    now_ms: now_ms(),
                },
            )?;
            println!(
                "queued steer {} for {child} under sup_epoch {epoch}",
                op.tag()
            );
            Ok(())
        }
        SupervisorCmd::View { project, json } => {
            let p = CtxPaths::new(&CtxPaths::default_dir(), project);
            let (msgs, _, _) = read_log(&p)?;
            let (m, _) = crate::supervisor_program::replay(&msgs);
            let rows = crew_view(&m);
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "status": m.status, "sup_epoch": m.accepted_epoch, "leader": m.leader,
                        "restarts_in_window": m.restarts.len(), "goal_proved": m.goal_proved,
                        "rows": rows,
                    }))?
                );
            } else {
                print_view(&m, &rows);
            }
            Ok(())
        }
        SupervisorCmd::Hooks { file } => {
            let from_env = std::env::var("FLYWHEEL_SUPERVISOR_HOOK_FILE").ok();
            let path = file
                .as_deref()
                .or_else(|| from_env.as_deref().map(std::path::Path::new));
            println!("{}", crate::harness_caps::doctor_line(path));
            Ok(())
        }
        SupervisorCmd::Halt {
            project,
            context,
            sup_epoch,
            request_key,
        } => queue_halt(
            project,
            context.as_deref(),
            *sup_epoch,
            request_key.as_deref(),
        ),
    }
}

/// Append one `Halt` to the context's inbox. The host folds it on the next round.
///
/// bd-4fgc: a halt is an effectful dispatch (it stops a subtree), so it carries
/// the same request-key receipt as the other doors. The slot is
/// `<project>-halt`, namespaced per door, and the gate runs BEFORE the append —
/// a settled slot replays and an unresolved one refuses instead of queueing a
/// second halt. The accepted epoch is resolved before the gate on purpose: a
/// read that fails must not burn the receipt.
pub fn queue_halt(
    project: &str,
    context: Option<&str>,
    sup_epoch: Option<u64>,
    request_key: Option<&str>,
) -> Result<()> {
    let dir = CtxPaths::default_dir();
    let p = match context {
        Some(c) if !c.is_empty() => CtxPaths::with_context(&dir, project, c),
        _ => CtxPaths::new(&dir, project),
    };
    let epoch = match sup_epoch {
        Some(e) => e,
        None => {
            let (msgs, _, _) = read_log(&p)?;
            crate::supervisor_program::replay(&msgs).0.accepted_epoch
        }
    };
    let request_key = request_key.map(str::trim).filter(|k| !k.is_empty());
    // The digest rides the OFFERED inputs, not the resolved epoch: a retry of
    // the same call always matches, while a different context (or a new
    // explicit epoch) is a different halt and needs its own key.
    let epoch_part = match sup_epoch {
        Some(e) => e.to_string(),
        None => "auto".to_string(),
    };
    let halt_digest = crate::request_receipts::payload_digest(
        "halt-v1",
        &[context.unwrap_or("-"), epoch_part.as_str()],
    );
    let mut receipt: Option<crate::request_receipts::DoorReceiptGuard> = None;
    if let Some(key) = request_key {
        let slot = crate::request_receipts::door_slot(project, "halt", key);
        match crate::request_receipts::receipt_gate(
            crate::request_receipts::load_door_receipt(project, "halt", &slot).as_ref(),
            &halt_digest,
        ) {
            crate::request_receipts::Gate::Proceed => {
                receipt = Some(crate::request_receipts::DoorReceiptGuard::begin(
                    project,
                    "halt",
                    &slot,
                    &halt_digest,
                )?);
            }
            crate::request_receipts::Gate::Replay => {
                println!(
                    "halt request key {key:?} already settled this slot ({slot}) — replaying, no \
                     second halt queued (sup_epoch {epoch})"
                );
                return Ok(());
            }
            crate::request_receipts::Gate::InFlight => bail!(
                "halt request key {key:?} is IN FLIGHT (slot {slot}) — a second halt could land \
                 while the first is still folding. Retry with a fresh --request-key once it \
                 settles."
            ),
            crate::request_receipts::Gate::Conflict => bail!(
                "halt request key {key:?} was already used for a DIFFERENT context or epoch (the \
                 recorded payload digest differs) — refusing to hand back another halt's slot \
                 under this key. Use a fresh --request-key for the new halt."
            ),
        }
    }
    append_input(
        &p,
        &SupMsg::Halt {
            sup_epoch: epoch,
            now_ms: now_ms(),
        },
    )?;
    println!("queued halt for {project} under sup_epoch {epoch}");
    if let Some(g) = receipt {
        g.settle(&format!("halt queued sup_epoch={epoch}"));
    }
    Ok(())
}

fn print_view(m: &SupModel, rows: &[CrewRow]) {
    println!(
        "supervisor {:?} sup_epoch {} leader {} restarts {}/{} goal_proved {}",
        m.status,
        m.accepted_epoch,
        m.leader,
        m.restarts.len(),
        m.policy.max_r,
        m.goal_proved
    );
    for r in rows {
        let steer = r
            .pending_steer
            .map(|(s, o)| format!(" steer#{s}:{}", o.tag()))
            .unwrap_or_default();
        println!(
            "  {:<20} {:?} gen {} abnormal {}{steer}",
            r.child, r.state, r.generation, r.abnormal
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed delta sha for tests that only need one.
    const D: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    use crate::supervisor_program::{ChildState, Status, SteerState};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct World {
        obs: Vec<SupMsg>,
        relaunched: Vec<(String, u64)>,
        exited: Vec<(String, u64)>,
        steers: Vec<SteerSend>,
        escalated: Vec<(u64, String)>,
        parked: Vec<String>,
        pages: Vec<(String, String)>,
        rejects: Vec<(u64, String)>,
        assigns: Vec<(String, String, u64)>,
        next_offset: u64,
        fail_relaunch: bool,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<World>>);

    impl Ports for Fake {
        fn observe(&mut self, _m: &SupModel, _now: u64) -> Vec<SupMsg> {
            std::mem::take(&mut self.0.lock().unwrap().obs)
        }
        fn relaunch(&mut self, c: &str, g: u64, _kind: ChildKind) -> Result<(), String> {
            let mut w = self.0.lock().unwrap();
            if w.fail_relaunch {
                return Err("herdr said no".into());
            }
            w.relaunched.push((c.into(), g));
            Ok(())
        }
        fn exit(&mut self, c: &str, g: u64) -> Result<(), String> {
            self.0.lock().unwrap().exited.push((c.into(), g));
            Ok(())
        }
        fn append_steer(&mut self, s: &SteerSend) -> Result<u64, String> {
            let mut w = self.0.lock().unwrap();
            w.next_offset += 10;
            w.steers.push(s.clone());
            Ok(w.next_offset)
        }
        fn assign(&mut self, a: &AssignSend) -> Result<u64, String> {
            let mut w = self.0.lock().unwrap();
            w.assigns
                .push((a.task.clone(), a.child.clone(), a.generation));
            Ok(1)
        }
        fn escalate(&mut self, n: u64, r: &str, _e: &str) -> Result<(), String> {
            self.0.lock().unwrap().escalated.push((n, r.into()));
            Ok(())
        }
        fn park(&mut self, c: &str, _n: u32) -> Result<(), String> {
            self.0.lock().unwrap().parked.push(c.into());
            Ok(())
        }
        fn page(&mut self, k: &str, p: &str, _l: &str) -> Result<(), String> {
            self.0.lock().unwrap().pages.push((k.into(), p.into()));
            Ok(())
        }
        fn reject(&mut self, n: u64, c: &str, _child: &str) -> Result<(), String> {
            self.0.lock().unwrap().rejects.push((n, c.into()));
            Ok(())
        }
    }

    fn open(dir: &Path) -> Core {
        let paths = CtxPaths::new(dir, "proj");
        let mut c = Core::open(paths, "lead-1", Tier::Lead, None).unwrap();
        c.claim("lead-1").unwrap();
        c
    }

    /// Process-wide. Restored on drop. The window is the test body.
    /// `mode_t` is 16 bits on macOS and 32 on other Unix.
    #[cfg(unix)]
    struct UmaskGuard {
        prev: u32,
    }

    #[cfg(unix)]
    impl UmaskGuard {
        fn set(mask: u32) -> Self {
            Self {
                prev: unsafe { set_umask(mask) },
            }
        }
    }

    #[cfg(unix)]
    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            unsafe {
                set_umask(self.prev);
            }
        }
    }

    #[cfg(all(unix, target_os = "macos"))]
    unsafe fn set_umask(mask: u32) -> u32 {
        unsafe extern "C" {
            fn umask(mask: u16) -> u16;
        }
        // SAFETY: the guard restores the previous mask on drop. The test
        // body is the only window, and it runs under the env lock.
        unsafe { umask(mask as u16) as u32 }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    unsafe fn set_umask(mask: u32) -> u32 {
        unsafe extern "C" {
            fn umask(mask: u32) -> u32;
        }
        // SAFETY: the guard restores the previous mask on drop.
        unsafe { umask(mask) }
    }

    /// Every new comms file under umask 022. A creator that forgets the
    /// explicit mode lands at 0644, which this walk rejects.
    #[cfg(unix)]
    #[test]
    fn supervisor_log_and_inbox_are_owner_only_under_umask_022() {
        let _home = crate::config::lock_env();
        let _umask = UmaskGuard::set(0o022);
        let d = tempfile::tempdir().unwrap();
        let paths = CtxPaths::new(d.path(), "modes");
        append_log(
            &paths,
            &Entry {
                seq: 1,
                in_off: None,
                msg: crate::supervisor_program::SupMsg::WorkMoved,
            },
        )
        .unwrap();
        append_input(&paths, &crate::supervisor_program::SupMsg::WorkMoved).unwrap();

        let cfg = crate::config::Config::default();
        let mut inbox = crate::inbox_ports::FlywheelPorts::new(&cfg, "modes", "ws");
        crate::inbox_host::Ports::fact(&mut inbox, "modes-fact", json!({"kind": "bind"})).unwrap();
        crate::inbox_host::Ports::save_budget(&mut inbox, &BTreeMap::new()).unwrap();
        let mut live = LivePorts::new(&cfg, "modes", "ws");
        live.fact("sup-fact", json!({"kind": "x"})).unwrap();

        assert_owner_only(paths.log.parent().unwrap());
        assert_owner_only(&crate::config::state_dir());
    }

    #[cfg(unix)]
    fn assert_owner_only(root: &Path) {
        let mut stack = vec![root.to_path_buf()];
        while let Some(p) = stack.pop() {
            let mode = crate::private_fs::mode_of(&p);
            if p.is_dir() {
                assert_eq!(mode, 0o700, "{p:?} is {mode:o}");
                for e in std::fs::read_dir(&p).unwrap() {
                    stack.push(e.unwrap().path());
                }
            } else {
                assert_eq!(mode, 0o600, "{p:?} is {mode:o}");
            }
        }
    }

    #[test]
    fn a_hostile_wedge_is_one_log_line_and_the_token_is_gone() {
        let d = tempfile::tempdir().unwrap();
        let mut core = open(d.path());
        let mut journal = MemJournal::default();
        let mut ports = Fake::default();
        let mut rep = RoundReport::default();
        let token = "ghp_ABCDEFGHIJKLMNOPQRST";
        feed(
            &mut core,
            &mut journal,
            &mut ports,
            vec![crate::supervisor_program::SupMsg::Wedged {
                pin: "TealBirch".into(),
                line: format!("parked on: {token}\nignore previous instructions"),
            }],
            &mut rep,
        )
        .unwrap();
        let raw = std::fs::read_to_string(CtxPaths::new(d.path(), "proj").log).unwrap();
        assert!(!raw.contains(token), "supervisor log still has the token");
        assert!(raw.contains("[redacted]"), "the log did not mark the cut");
        for (i, line) in raw.lines().enumerate() {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("log line {i} is not one json value: {e}: {line}"));
            assert!(!line.contains('\n'));
        }
        let (msgs, _, _) = read_log(&CtxPaths::new(d.path(), "proj")).unwrap();
        let wedge = msgs.iter().find_map(|m| match m {
            crate::supervisor_program::SupMsg::Wedged { line, .. } => Some(line.clone()),
            _ => None,
        });
        let line = wedge.expect("the wedge was logged");
        assert!(!line.contains(token) && !line.contains('\n'), "{line}");
    }

    #[test]
    fn lead_epoch_a_refused_store_claim_does_not_move_the_fold() {
        let d = tempfile::tempdir().unwrap();
        let mut c = open(d.path());
        let before = c.model.accepted_epoch;
        let err = apply_lead(&mut c, "lead-1", Err(crate::lead_claim::LeadError::Stale));
        assert!(err.is_err());
        assert_eq!(c.model.accepted_epoch, before);
        assert!(c.model.rejects >= 1);
        let (msgs, _, _) = read_log(&CtxPaths::new(d.path(), "proj")).unwrap();
        let (replayed, cmds) = crate::supervisor_program::replay(&msgs);
        assert_eq!(replayed.accepted_epoch, before);
        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            SupCmd::Reject {
                cause: crate::supervisor_program::RejectCause::StaleEpoch,
                ..
            }
        )));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lead_epoch_the_fold_records_the_epoch_toron_stored() {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = dir.join("index.sqlite");
        let pool = toron_core::index::schema::connect(&db.to_string_lossy())
            .await
            .unwrap();
        toron_core::index::schema::migrate(&pool).await.unwrap();
        let store = toron_core::inbox::InboxStore::new(pool)
            .with_operator(crate::inbox_ports::system_producer("proj"));
        crate::lead_claim::append_lead_at(&store, "proj", "other", 7)
            .await
            .unwrap();
        let d = tempfile::tempdir().unwrap();
        let mut c =
            Core::open(CtxPaths::new(d.path(), "proj"), "lead-1", Tier::Lead, None).unwrap();
        assert_eq!(c.model.accepted_epoch, 0);
        let step = crate::lead_claim::sync_lead(&store, "proj", "lead-1", 0)
            .await
            .unwrap();
        let epoch = apply_lead(&mut c, "lead-1", Ok(step)).unwrap();
        assert_eq!(epoch, 8);
        assert_eq!(c.model.accepted_epoch, 8);
        assert_eq!(store.lead_of("proj").await.unwrap().unwrap().sup_epoch, 8);
    }

    /// rv23 #6: an operated in-memory comms store for `project`.
    fn op_store(project: &str) -> toron_core::inbox::InboxStore {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = dir.join("index.sqlite");
        crate::mail_shim::block_on(async move {
            let pool = toron_core::index::schema::connect(&db.to_string_lossy())
                .await
                .unwrap();
            toron_core::index::schema::migrate(&pool).await.unwrap();
            toron_core::inbox::InboxStore::new(pool)
        })
        .with_operator(crate::inbox_ports::system_producer(project))
    }

    fn lead_row(store: &toron_core::inbox::InboxStore, project: &str) -> (String, u64) {
        let s = store.clone();
        let p = project.to_string();
        let row = crate::mail_shim::block_on(async move { s.lead_of(&p).await })
            .unwrap()
            .expect("a lead@1 row");
        (row.holder, row.sup_epoch)
    }

    /// rv23 #6 (toron F7): the lead holder on record is the key the host
    /// steers with. toron accepts a steer only when `from` equals the holder,
    /// and the old holder (`supervise-<project>-<pid>`) fenced every steer.
    #[test]
    fn the_lead_holder_is_the_key_the_host_steers_with() {
        let store = op_store("proj");
        let d = tempfile::tempdir().unwrap();
        let me = "supervise-proj-4242";
        let mut c = Core::open(CtxPaths::new(d.path(), "proj"), me, Tier::Lead, None).unwrap();
        let claim = claim_with(&mut c, "proj", "proj", me, None, Some(store.clone())).unwrap();
        assert_eq!(claim.lead, Some(claim.epoch), "toron mode holds the lead");
        let (holder, epoch) = lead_row(&store, "proj");
        assert_eq!(epoch, claim.epoch);
        let s = SteerSend {
            sup_epoch: claim.epoch,
            ..send(SteerOp::Pause, None)
        };
        let producer = crate::inbox_ports::system_producer("proj");
        let env = steer_envelope("proj", &producer, 0, NPUB, &s, None).unwrap();
        assert_eq!(holder, env.from, "the holder is the steer's `from`");
        assert_ne!(holder, me, "the pid-bearing name is not the holder");
        // And the store keeps the steer (F7: from == holder at the epoch).
        let st = store.clone();
        let kept = crate::mail_shim::block_on(async move {
            let claim = st.claim_epoch(&producer, "test").await.unwrap();
            let mut env = env;
            env.msg.epoch = claim.epoch;
            st.append_envelope(&env).await
        });
        assert!(kept.is_ok(), "the holder's steer is accepted: {kept:?}");
    }

    /// rv23 #6: fold mode claims the lead too. The fold picks the epoch and
    /// the store records `lead@1` at it, so no mode steers without a lead.
    #[test]
    fn fold_mode_claims_the_lead_at_the_fold_epoch() {
        let store = op_store("proj");
        let d = tempfile::tempdir().unwrap();
        let me = "supervise-proj-4243";
        let mut c = Core::open(CtxPaths::new(d.path(), "proj"), me, Tier::Lead, None).unwrap();
        let claim = claim_with(&mut c, "proj", "proj", me, Some("fold"), Some(store.clone())).unwrap();
        assert_eq!(claim.lead, Some(claim.epoch));
        assert_eq!(
            lead_row(&store, "proj"),
            (crate::lead_claim::lead_holder("proj"), claim.epoch)
        );
    }

    /// rv23 #6: no store, no lead, no steer. The gate names why.
    #[test]
    fn no_lead_claim_holds_every_steer() {
        let d = tempfile::tempdir().unwrap();
        let me = "supervise-proj-4244";
        let mut c = Core::open(CtxPaths::new(d.path(), "proj"), me, Tier::Lead, None).unwrap();
        let claim = claim_with(&mut c, "proj", "proj", me, None, None).unwrap();
        assert_eq!(claim.lead, None);
        let e = claim.epoch;
        assert!(steer_gate(claim.lead, e).unwrap_err().contains("no lead claim"));
        assert!(steer_gate(Some(e), e + 1).is_err(), "another epoch is not ours");
        assert!(steer_gate(Some(e), e).is_ok());
    }

    /// rv23 #6: the live port checks the gate before it builds anything.
    #[test]
    fn live_append_steer_is_gated_on_the_lead() {
        let src = include_str!("supervisor_host.rs");
        let at = src
            .find("    fn append_steer(&mut self, s: &SteerSend) -> Result<u64, String> {\n        // rv23")
            .expect("LivePorts::append_steer");
        let body = &src[at..at + 400];
        assert!(body.contains("steer_gate(self.lead, s.sup_epoch)?;"), "{body}");
        let main = src.find("fn host_loop(").expect("host_loop");
        assert!(src[main..].contains(".with_lead(claim.lead)"));
    }

    /// rv23 #6, nested leads: a nested lead host claims and steers in its
    /// own context (`project/<sub>`), never the project's. Both leads hold at
    /// once, each one's steer is kept, and a steer into the other's context is
    /// `NotLeadHolder` both ways.
    #[test]
    fn a_nested_lead_claims_and_steers_in_its_own_context() {
        use crate::lead_claim::{lead_context, lead_holder, store_reject_cause};
        use crate::supervisor_program::RejectCause;
        let store = op_store("proj");
        let sub = lead_context("proj", Some("plane"));
        assert_eq!(lead_context("proj", None), "proj");
        assert_eq!(sub, "proj/plane");
        let d = tempfile::tempdir().unwrap();
        let (rme, lme) = ("supervise-proj-1", "supervise-proj-plane-2");
        let mut root = Core::open(CtxPaths::new(d.path(), "proj"), rme, Tier::Lead, None).unwrap();
        let nested = CtxPaths::with_context(d.path(), "proj", "plane");
        let mut lead = Core::open(nested, lme, Tier::Lead, None).unwrap();
        let rc = claim_with(&mut root, "proj", "proj", rme, None, Some(store.clone())).unwrap();
        let lc = claim_with(&mut lead, "proj", &sub, lme, None, Some(store.clone())).unwrap();
        assert_eq!(lc.lead, Some(lc.epoch));
        assert_eq!(lead_row(&store, "proj"), (lead_holder("proj"), rc.epoch));
        assert_eq!(lead_row(&store, &sub), (lead_holder(&sub), lc.epoch));
        assert_ne!(lead_holder("proj"), lead_holder(&sub));
        // `as_ctx` signs as that context's holder; `into` is the steer's context.
        let steer = |as_ctx: &str, into: &str, child: &str, sup_epoch: u64| {
            let s = SteerSend {
                child: child.into(),
                sup_epoch,
                ..send(SteerOp::Pause, None)
            };
            let producer = lead_holder(as_ctx);
            let mut env = steer_envelope(into, &producer, 0, NPUB, &s, None).unwrap();
            let st = store.clone();
            crate::mail_shim::block_on(async move {
                env.msg.epoch = st.claim_epoch(&producer, "test").await.unwrap().epoch;
                st.append_envelope(&env).await
            })
        };
        assert!(steer("proj", "proj", "r", rc.epoch).is_ok(), "root steer");
        assert!(steer(&sub, &sub, "l", lc.epoch).is_ok(), "nested steer");
        let into_sub = steer("proj", &sub, "x", lc.epoch).unwrap_err();
        assert_eq!(
            store_reject_cause(&into_sub),
            Some(RejectCause::NotLeadHolder),
            "{into_sub}"
        );
        let into_root = steer(&sub, "proj", "y", rc.epoch).unwrap_err();
        assert_eq!(
            store_reject_cause(&into_root),
            Some(RejectCause::NotLeadHolder),
            "{into_root}"
        );
    }

    /// rv23 #6, nested leads: the live port steers in its lead context and
    /// the host claims in it.
    #[test]
    fn the_live_host_claims_and_steers_in_its_lead_context() {
        let src = include_str!("supervisor_host.rs");
        let main = src.find("fn host_loop(").expect("host_loop");
        let body = &src[main..];
        assert!(body.contains("lead_context(project, context);"), "host_loop");
        assert!(body.contains("claim_for_host(&mut core, project, &lead_ctx, me)?"));
        let at = src
            .find("    fn append_steer(&mut self, s: &SteerSend) -> Result<u64, String> {\n        // rv23")
            .expect("LivePorts::append_steer");
        let steer = &src[at..at + 1400];
        assert!(steer.contains("let ctx = self.lead_context();"), "{steer}");
        assert!(steer.contains("steer_envelope(&ctx, &producer,"), "{steer}");
    }

    #[test]
    fn lead_epoch_recover_does_not_resend_a_steer_the_log_already_appended() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let epoch = c.model.accepted_epoch;
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Control {
                op: SteerOp::Redirect,
                child: "a".into(),
                sup_epoch: epoch,
                delta_sha: Some(D.into()),
                now_ms: 2_000,
            }],
            2_000,
        );
        assert_eq!(f.0.lock().unwrap().steers.len(), 1);
        assert!(c.model.steers.values().any(|s| s.offset.is_some()));
        let mut fresh = MemJournal::default();
        recover(&mut c, &mut fresh, &mut f.clone()).unwrap();
        assert_eq!(
            f.0.lock().unwrap().steers.len(),
            1,
            "a steer the log already appended is not sent again"
        );
    }

    #[test]
    fn lead_epoch_store_stale_and_the_fold_reject_are_one_cause() {
        let err =
            toron_core::inbox::InboxError::Reject(toron_core::delivery::Reject::StaleSupEpoch {
                sent: 0,
                current: 1,
            });
        assert_eq!(
            crate::lead_claim::store_reject_cause(&err),
            Some(crate::supervisor_program::RejectCause::StaleEpoch)
        );
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Control {
                op: SteerOp::Cancel,
                child: "a".into(),
                sup_epoch: 0,
                delta_sha: None,
                now_ms: 2_000,
            }],
            2_000,
        );
        let tags: Vec<_> =
            f.0.lock()
                .unwrap()
                .rejects
                .iter()
                .map(|(_, tag)| tag.clone())
                .collect();
        assert_eq!(tags, vec!["stale_epoch".to_string()]);
    }

    fn spawn(id: &str) -> SupMsg {
        spawn_as(id, ChildKind::Crew)
    }

    fn lead_model(state: ChildState) -> SupModel {
        let (mut m, _) = crate::supervisor_program::replay(&[spawn_as("plane", ChildKind::Lead)]);
        m.children.get_mut("plane").unwrap().state = state;
        m
    }

    fn escalated_keys(out: &[SupMsg]) -> Vec<String> {
        out.iter()
            .filter_map(|m| match m {
                SupMsg::ChildEscalated { fact_key, .. } => fact_key.clone(),
                _ => None,
            })
            .collect()
    }

    /// rv23 #4: a lead's facts are read from where this host left off (not
    /// the whole file every round). A partial line waits for its newline, a
    /// stopped lead's lines are read again once it runs, a restart resumes at
    /// the committed offset, and a replaced (shorter) file is read from the
    /// start.
    #[test]
    fn lead_facts_are_read_incrementally() {
        use std::io::Write as _;
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("proj--plane-supervisor.jsonl");
        let line = |k: &str| format!("{}\n", json!({"kind": "escalate", "key": k, "reason": "r"}));
        let add = |s: &str| {
            let mut h = std::fs::OpenOptions::new().append(true).open(&f).unwrap();
            h.write_all(s.as_bytes()).unwrap();
        };
        let run = lead_model(ChildState::Running);
        std::fs::write(&f, line("k1")).unwrap();
        let mut c = FactsCursor::open(f.clone(), "root");
        assert_eq!(escalated_keys(&lead_upward(&mut c, &run, "plane", 1, false)), ["k1"]);
        let after_first = c.bytes_read;
        assert!(escalated_keys(&lead_upward(&mut c, &run, "plane", 2, false)).is_empty());
        assert_eq!(c.bytes_read, after_first, "nothing new is no bytes read");
        add(&format!("{}{{\"kind\":\"esc", line("k2")));
        assert_eq!(escalated_keys(&lead_upward(&mut c, &run, "plane", 3, false)), ["k2"]);
        add("alate\",\"key\":\"k3\",\"reason\":\"r\"}\n");
        let stopped = lead_model(ChildState::Restarting);
        assert!(escalated_keys(&lead_upward(&mut c, &stopped, "plane", 4, false)).is_empty());
        assert_eq!(
            escalated_keys(&lead_upward(&mut c, &run, "plane", 5, false)),
            ["k3"],
            "a line a stopped lead skipped is read once it runs"
        );
        c.commit();
        let mut again = FactsCursor::open(f.clone(), "root");
        assert!(
            escalated_keys(&lead_upward(&mut again, &run, "plane", 6, false)).is_empty(),
            "a restart resumes at the committed offset"
        );
        let mut other = FactsCursor::open(f.clone(), "plane");
        assert_eq!(
            escalated_keys(&lead_upward(&mut other, &run, "plane", 6, false)),
            ["k1", "k2", "k3"],
            "each reader keeps its own offset"
        );
        std::fs::write(&f, line("k9")).unwrap();
        assert_eq!(escalated_keys(&lead_upward(&mut again, &run, "plane", 7, false)), ["k9"]);
    }

    /// rv23 #4: the live observe reads lead and deaf facts through cursors.
    #[test]
    fn live_observe_reads_facts_through_cursors() {
        let src = include_str!("supervisor_host.rs");
        let at = src.find("impl Ports for LivePorts {").expect("LivePorts");
        let body = &src[at..at + 4000];
        assert!(body.contains("out.extend(lead_upward(cur, model, &id, now_ms, dead));"));
        assert!(body.contains("let (buf, end) = deaf.read_new();"));
        assert!(!body.contains("std::fs::read_to_string(lead_facts_file"));
    }

    fn spawn_as(id: &str, kind: ChildKind) -> SupMsg {
        SupMsg::Spawn {
            spec: ChildSpec {
                id: id.into(),
                npub: Some(format!("npub-{id}")),
                restart: Restart::Transient,
                kind,
                role: None,
            },
            now_ms: 0,
        }
    }

    fn go(c: &mut Core, j: &mut MemJournal, f: &Fake, extra: Vec<SupMsg>, now: u64) -> RoundReport {
        let mut p = f.clone();
        round(c, j, &mut p, extra, now).unwrap()
    }

    #[test]
    fn a_crashed_crew_is_relaunched_once_at_the_next_generation() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let rep = go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Exit {
                child: "a".into(),
                now_ms: 2_000,
            }],
            2_000,
        );
        assert_eq!(f.0.lock().unwrap().relaunched, vec![("a".to_string(), 2)]);
        assert_eq!(rep.ran, 1);
        assert_eq!(
            c.model.children["a"].state,
            ChildState::Running,
            "Relaunched fed back"
        );
        // A tick later changes nothing and relaunches nothing.
        go(&mut c, &mut j, &f, vec![], 3_000);
        assert_eq!(f.0.lock().unwrap().relaunched.len(), 1);
    }

    /// Drops `FLYWHEEL_ALLOW_EXEC` and the relaunch-bin override.
    struct ClearExec;

    impl Drop for ClearExec {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("FLYWHEEL_ALLOW_EXEC") }
            unsafe { std::env::remove_var("FLYWHEEL_SUPERVISOR_RELAUNCH_BIN") }
            unsafe { std::env::remove_var("RELAUNCH_MARKER") }
        }
    }

    fn relaunch_script(dir: &Path) -> PathBuf {
        let bin = dir.join("relaunch.sh");
        std::fs::write(&bin, "#!/bin/sh\necho $$ > \"$RELAUNCH_MARKER\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        bin
    }

    /// Forces the switch off on this thread. Drop clears it.
    struct ForceOff;

    impl ForceOff {
        fn new() -> Self {
            EXEC_FORCE_OFF.with(|c| c.set(true));
            Self
        }
    }

    impl Drop for ForceOff {
        fn drop(&mut self) {
            EXEC_FORCE_OFF.with(|c| c.set(false));
        }
    }

    #[test]
    fn allow_exec_zero_records_the_crash_and_does_not_spawn() {
        let _home = crate::config::lock_env();
        let _clear = ClearExec;
        // The env itself is the switch. Hold it only for this assert so a
        // sibling fold in this process does not go log-only.
        unsafe { std::env::set_var("FLYWHEEL_ALLOW_EXEC", "0") };
        assert!(!exec_allowed());
        unsafe { std::env::set_var("FLYWHEEL_ALLOW_EXEC", "1") };
        assert!(exec_allowed());
        unsafe { std::env::remove_var("FLYWHEEL_ALLOW_EXEC") };
        assert!(exec_allowed());
        let _off = ForceOff::new();
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("spawned");
        unsafe {
            std::env::set_var(
                "FLYWHEEL_SUPERVISOR_RELAUNCH_BIN",
                relaunch_script(d.path()),
            )
        };
        unsafe { std::env::set_var("RELAUNCH_MARKER", &marker) };
        let cfg = crate::config::Config::default();
        let mut live = LivePorts::new(&cfg, "exec", "exec");
        let mut c = open(d.path());
        let mut j = MemJournal::default();
        let rep = round(
            &mut c,
            &mut j,
            &mut live,
            vec![
                spawn("a"),
                SupMsg::Exit {
                    child: "a".into(),
                    now_ms: 2_000,
                },
            ],
            2_000,
        )
        .unwrap();
        assert!(!marker.exists(), "relaunch spawned under the switch");
        assert_eq!(c.model.children["a"].state, ChildState::Restarting);
        assert!(
            rep.suppressed
                .iter()
                .any(|l| l.contains("suppressed Relaunch"))
        );
        let note = std::fs::read_to_string(
            CtxPaths::new(d.path(), "proj")
                .log
                .with_extension("suppressed"),
        )
        .unwrap();
        assert!(note.contains("suppressed Relaunch"), "{note}");
        let log = std::fs::read_to_string(CtxPaths::new(d.path(), "proj").log).unwrap();
        assert!(
            log.contains("\"kind\":\"exit\""),
            "the crash is in the log: {log}"
        );
        // Recover must not invent a relaunch the switch refused.
        recover(&mut c, &mut j, &mut live).unwrap();
        assert!(!marker.exists(), "recover spawned under the switch");
        assert_eq!(c.model.children["a"].state, ChildState::Restarting);

        drop(_off);
        let d2 = tempfile::tempdir().unwrap();
        let marker2 = d2.path().join("spawned");
        unsafe {
            std::env::set_var(
                "FLYWHEEL_SUPERVISOR_RELAUNCH_BIN",
                relaunch_script(d2.path()),
            )
        };
        unsafe { std::env::set_var("RELAUNCH_MARKER", &marker2) };
        let mut live2 = LivePorts::new(&cfg, "exec", "exec");
        let mut c2 = open(d2.path());
        let mut j2 = MemJournal::default();
        round(
            &mut c2,
            &mut j2,
            &mut live2,
            vec![
                spawn("a"),
                SupMsg::Exit {
                    child: "a".into(),
                    now_ms: 2_000,
                },
            ],
            2_000,
        )
        .unwrap();
        assert!(marker2.exists(), "unset switch did not relaunch");
        assert_eq!(c2.model.children["a"].state, ChildState::Running);
        unsafe { std::env::remove_var("RELAUNCH_MARKER") };
    }

    #[test]
    fn readme_names_the_verbs_that_run_later() {
        let readme =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
        let later = readme
            .split("### Runs later")
            .nth(1)
            .expect("README is missing the runs-later list");
        let later = later.split("## Command map").next().unwrap();
        for needle in [
            "Relaunch",
            "hook install",
            "--delta-file",
            "loop-mode supervise",
            "supervisor fact",
            "supervisor steer",
            "supervisor view",
            "next",
            "ack",
            "done",
            "inbox",
            "FLYWHEEL_ALLOW_EXEC=0",
        ] {
            assert!(later.contains(needle), "runs-later list missing {needle}");
        }
    }

    fn open_with(dir: &Path, strategy: crate::supervisor_program::Strategy) -> Core {
        let mut policy = Policy::for_tier(Tier::Lead);
        policy.strategy = strategy;
        let mut c = Core::open(
            CtxPaths::new(dir, "proj"),
            "lead-1",
            Tier::Lead,
            Some(policy),
        )
        .unwrap();
        c.claim("lead-1").unwrap();
        c
    }

    #[test]
    fn a_rest_for_one_cascade_stops_then_relaunches_and_a_kill_mid_cascade_finishes_it_exactly_once()
     {
        use crate::supervisor_program::Strategy;
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (
            open_with(d.path(), Strategy::RestForOne),
            MemJournal::default(),
            Fake::default(),
        );
        go(
            &mut c,
            &mut j,
            &f,
            vec![spawn("a"), spawn("b"), spawn("c")],
            1_000,
        );
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Exit {
                child: "b".into(),
                now_ms: 2_000,
            }],
            2_000,
        );
        {
            let w = f.0.lock().unwrap();
            assert_eq!(w.exited, vec![("c".to_string(), 1)]);
            assert_eq!(
                w.relaunched,
                vec![("b".to_string(), 2), ("c".to_string(), 2)],
                "never a"
            );
        }
        // Crash model: c's relaunch ran but its journal row never settled.
        // Only unsettled commands may re-run.
        j.rows.get_mut("relaunch:c:2").unwrap().0 = false;
        drop(c);
        let f2 = Fake::default();
        let mut c2 = open_with(d.path(), Strategy::RestForOne);
        let mut p2 = f2.clone();
        recover(&mut c2, &mut j, &mut p2).unwrap();
        let w2 = f2.0.lock().unwrap();
        assert_eq!(
            w2.relaunched,
            vec![("c".to_string(), 2)],
            "exactly the unsettled relaunch, once"
        );
        assert!(w2.exited.is_empty(), "the settled Exit does not run again");
    }

    #[test]
    fn a_kill_between_a_settled_relaunch_and_its_feedback_does_not_strand_the_child_restarting() {
        use crate::supervisor_program::Strategy;
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (
            open_with(d.path(), Strategy::RestForOne),
            MemJournal::default(),
            Fake::default(),
        );
        go(&mut c, &mut j, &f, vec![spawn("a"), spawn("b")], 1_000);
        // The crash window: the exit is logged, the relaunch ran and settled,
        // the host died before `Relaunched` was fed back.
        let cmds = c
            .append_fold(SupMsg::Exit {
                child: "a".into(),
                now_ms: 2_000,
            })
            .unwrap();
        let mut rep = RoundReport::default();
        let mut p = f.clone();
        for cmd in &cmds {
            execute(&mut j, &mut p, cmd, &mut rep);
        }
        let want: Vec<(String, u64)> = vec![("a".into(), 2), ("b".into(), 2)];
        assert_eq!(f.0.lock().unwrap().relaunched, want);
        drop(c);
        let f2 = Fake::default();
        let mut c2 = open_with(d.path(), Strategy::RestForOne);
        let mut p2 = f2.clone();
        recover(&mut c2, &mut j, &mut p2).unwrap();
        assert!(
            f2.0.lock().unwrap().relaunched.is_empty(),
            "nothing runs twice"
        );
        assert_eq!(
            c2.model.children["a"].state,
            ChildState::Running,
            "confirmed from the settled journal row"
        );
        assert_eq!(c2.model.children["b"].state, ChildState::Running);
        // A second recovery over the now-complete log changes nothing (a no-op
        // message is never logged, so the log does not grow either).
        drop(c2);
        let mut c3 = open_with(d.path(), Strategy::RestForOne); // opening logs its own rebind/claim
        let n = read_log(&CtxPaths::new(d.path(), "proj")).unwrap().0.len();
        recover(&mut c3, &mut j, &mut f2.clone()).unwrap();
        assert_eq!(
            read_log(&CtxPaths::new(d.path(), "proj")).unwrap().0.len(),
            n,
            "recovery over a complete log appends nothing"
        );
    }

    #[test]
    fn the_strategy_word_reaches_the_policy() {
        let _g = crate::config::lock_env();
        unsafe { std::env::remove_var("FLYWHEEL_SUPERVISOR_CLOCK_SCALE") };
        unsafe { std::env::remove_var("FLYWHEEL_SUPERVISOR_JOIN") };
        unsafe { std::env::remove_var("FLYWHEEL_SUPERVISOR_MAX_WALL_MS") };
        use crate::supervisor_program::Strategy;
        let cfg = HostCfg {
            strategy: Some(Strategy::OneForAll),
            ..HostCfg::from_env()
        };
        assert_eq!(cfg.policy().unwrap().strategy, Strategy::OneForAll);
        let none = HostCfg {
            strategy: None,
            ack_ms: None,
            lease_ms: None,
            ..HostCfg::from_env()
        };
        assert!(
            none.policy().is_none(),
            "no override: the tier default stands"
        );
    }

    #[test]
    fn p4b_close_br_hold_uses_the_pinned_binary_and_skips_a_beadless_page() {
        use std::os::unix::fs::PermissionsExt;
        let _g = crate::config::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("br");
        let logf = dir.path().join("argv");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n", logf.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        unsafe { std::env::set_var("FLYWHEEL_BR_BIN", &script) };
        let mut j = MemJournal::default();
        let mut rep = RoundReport::default();
        let mut ports = Fake::default();
        let back = execute(
            &mut j,
            &mut ports,
            &SupCmd::Escalate {
                n: 1,
                reason: "budget".into(),
                evidence: "see bd-close1".into(),
            },
            &mut rep,
        );
        assert!(back.is_empty(), "{back:?}");
        let got = std::fs::read_to_string(&logf).unwrap();
        assert_eq!(got, "hold\n--kind\ncaptain\n--corr\nbd-close1\nbd-close1\n");
        std::fs::remove_file(&logf).unwrap();
        let back = execute(
            &mut j,
            &mut ports,
            &SupCmd::Escalate {
                n: 2,
                reason: "budget".into(),
                evidence: "no bead here".into(),
            },
            &mut rep,
        );
        assert!(back.is_empty());
        assert!(!logf.exists(), "a page with no bead id does not spawn br");
        std::fs::write(&script, "#!/bin/sh\nexit 3\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut j = MemJournal::default();
        let back = execute(
            &mut j,
            &mut ports,
            &SupCmd::Escalate {
                n: 1,
                reason: "budget".into(),
                evidence: "bd-close1".into(),
            },
            &mut rep,
        );
        assert!(
            matches!(back.first(), Some(SupMsg::CmdFailed { .. })),
            "{back:?}"
        );
        unsafe { std::env::remove_var("FLYWHEEL_BR_BIN") };
    }

    /// rv23 #5: a `br` that writes its argv to `logf`, then runs `tail`.
    fn br_script(dir: &Path, tail: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("br");
        let logf = dir.join("argv");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n{tail}\n",
                logf.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, logf)
    }

    fn escalate_bd(n: u64) -> SupCmd {
        SupCmd::Escalate {
            n,
            reason: "budget".into(),
            evidence: "see bd-hold1".into(),
        }
    }

    /// rv23 #5 (review S3): log-only mode must not exec `br` either. The page
    /// still goes out; the hold is a suppressed line, like Relaunch and Exit.
    #[test]
    fn allow_exec_zero_pages_but_does_not_run_br_hold() {
        let _g = crate::config::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let (script, logf) = br_script(dir.path(), "exit 0");
        unsafe { std::env::set_var("FLYWHEEL_BR_BIN", &script) };
        let _off = ForceOff::new();
        let mut j = MemJournal::default();
        let mut rep = RoundReport::default();
        let mut ports = Fake::default();
        let back = execute(&mut j, &mut ports, &escalate_bd(1), &mut rep);
        unsafe { std::env::remove_var("FLYWHEEL_BR_BIN") };
        assert!(back.is_empty(), "{back:?}");
        assert_eq!(
            ports.0.lock().unwrap().escalated,
            vec![(1, "budget".to_string())],
            "the page is not an exec: it still goes out"
        );
        assert!(!logf.exists(), "FLYWHEEL_ALLOW_EXEC=0 must not spawn br");
        assert!(
            rep.suppressed
                .iter()
                .any(|l| l.contains("br hold") && l.contains("FLYWHEEL_ALLOW_EXEC=0")),
            "the skipped hold is named: {:?}",
            rep.suppressed
        );
    }

    /// rv23 #5 (review S3): the hold runs `br` in the ports' tracker dir
    /// (the project's, where its `.beads` is), not the host's cwd.
    #[test]
    fn br_hold_runs_in_the_tracker_dir() {
        struct InDir(Fake, PathBuf);
        impl Ports for InDir {
            fn observe(&mut self, m: &SupModel, now: u64) -> Vec<SupMsg> {
                self.0.observe(m, now)
            }
            fn relaunch(&mut self, c: &str, g: u64, k: ChildKind) -> Result<(), String> {
                self.0.relaunch(c, g, k)
            }
            fn exit(&mut self, c: &str, g: u64) -> Result<(), String> {
                self.0.exit(c, g)
            }
            fn append_steer(&mut self, s: &SteerSend) -> Result<u64, String> {
                self.0.append_steer(s)
            }
            fn assign(&mut self, a: &AssignSend) -> Result<u64, String> {
                self.0.assign(a)
            }
            fn escalate(&mut self, n: u64, r: &str, e: &str) -> Result<(), String> {
                self.0.escalate(n, r, e)
            }
            fn park(&mut self, c: &str, i: u32) -> Result<(), String> {
                self.0.park(c, i)
            }
            fn page(&mut self, k: &str, p: &str, l: &str) -> Result<(), String> {
                self.0.page(k, p, l)
            }
            fn reject(&mut self, n: u64, c: &str, ch: &str) -> Result<(), String> {
                self.0.reject(n, c, ch)
            }
            fn tracker_dir(&self) -> Option<PathBuf> {
                Some(self.1.clone())
            }
        }
        let _g = crate::config::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let cwdf = dir.path().join("cwd");
        let (script, logf) = br_script(dir.path(), &format!("pwd -P > {}", cwdf.display()));
        unsafe { std::env::set_var("FLYWHEEL_BR_BIN", &script) };
        let mut j = MemJournal::default();
        let mut rep = RoundReport::default();
        let mut ports = InDir(Fake::default(), proj.clone());
        let back = execute(&mut j, &mut ports, &escalate_bd(1), &mut rep);
        unsafe { std::env::remove_var("FLYWHEEL_BR_BIN") };
        assert!(back.is_empty(), "{back:?}");
        assert!(logf.exists(), "br ran");
        assert_eq!(
            std::fs::read_to_string(&cwdf).unwrap().trim(),
            proj.canonicalize().unwrap().to_string_lossy(),
            "br hold runs in the tracker dir"
        );
    }

    /// rv23 #5: the live ports' tracker dir is the project dir `br ready`
    /// and `br init` already use.
    #[test]
    fn live_ports_hold_br_in_the_project_dir() {
        let src = include_str!("supervisor_host.rs");
        let at = src.find("impl Ports for LivePorts {").expect("LivePorts");
        assert!(
            src[at..at + 200].contains("Some(crate::project_dir(&self.cfg, &self.project))"),
            "{}",
            &src[at..at + 200]
        );
    }

    /// rv23 #5 (review S3): a hung `br` must not park the supervisor loop.
    /// Past the deadline the child is killed and reaped. The deadline is an
    /// injected clock that passes only once `br` has written its pid, so the
    /// test does not race the machine's load (it was flaky at a real 300 ms).
    #[test]
    fn a_hung_br_hold_is_killed_at_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let pidf = dir.path().join("pid");
        let (script, logf) = br_script(
            dir.path(),
            &format!("echo $$ > {}\nexec sleep 20", pidf.display()),
        );
        let deadline = Duration::from_secs(10);
        // Zero until br is up and has its pid written, then the deadline.
        let mut clock = || match std::fs::read_to_string(&pidf) {
            Ok(s) if s.ends_with('\n') => deadline,
            _ => Duration::ZERO,
        };
        let started = std::time::Instant::now();
        let got = run_bounded_on(
            script.to_str().unwrap(),
            &["hold".into()],
            None,
            deadline,
            &mut clock,
        );
        let took = started.elapsed();
        assert!(logf.exists(), "br did run");
        assert!(matches!(got, Err(Bounded::TimedOut(_))), "past the deadline is a timeout");
        assert!(
            took < Duration::from_secs(15),
            "killed at the deadline, not waited out (br hangs 20 s): {took:?}"
        );
        let pid = std::fs::read_to_string(&pidf).unwrap();
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "the hung br (pid {}) is gone", pid.trim());
    }

    /// The same deadline through `execute`, on the real clock: a pinned `br`
    /// that hangs fails the escalate loud as "timed out", well before the hang
    /// ends. Only the upper bound is timed, with a wide margin.
    #[test]
    fn a_hung_pinned_br_hold_fails_the_escalate_as_timed_out() {
        let _g = crate::config::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let (script, _) = br_script(dir.path(), "exec sleep 20");
        unsafe { std::env::set_var("FLYWHEEL_BR_BIN", &script) };
        unsafe { std::env::set_var("FLYWHEEL_BR_HOLD_TIMEOUT_MS", "300") };
        let mut j = MemJournal::default();
        let mut rep = RoundReport::default();
        let mut ports = Fake::default();
        let started = std::time::Instant::now();
        let back = execute(&mut j, &mut ports, &escalate_bd(1), &mut rep);
        let took = started.elapsed();
        unsafe { std::env::remove_var("FLYWHEEL_BR_BIN") };
        unsafe { std::env::remove_var("FLYWHEEL_BR_HOLD_TIMEOUT_MS") };
        assert!(
            took < Duration::from_secs(15),
            "the hold waits for its deadline, not for br (20 s): {took:?}"
        );
        assert!(
            matches!(back.first(), Some(SupMsg::CmdFailed { why, .. }) if why.contains("timed out")),
            "{back:?}"
        );
    }

    /// What a hinted loop publishes after each round.
    fn spawn_loop(
        mut c: Core,
        mut j: MemJournal,
        f: Fake,
        rx: std::sync::mpsc::Receiver<()>,
        tick: std::time::Duration,
        stop: Arc<std::sync::atomic::AtomicBool>,
        acked: Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<Core> {
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let now = 10_000 + t0.elapsed().as_millis() as u64;
                round(&mut c, &mut j, &mut f.clone(), vec![], now).unwrap();
                if c.model
                    .steers
                    .get(&1)
                    .is_some_and(|s| s.state == SteerState::Acked)
                {
                    acked.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                crate::supervisor_hints::wait(&rx, tick);
            }
            c
        })
    }

    fn steered_core(d: &Path) -> (Core, MemJournal, Fake) {
        let (mut c, mut j, f) = (open(d), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let epoch = c.model.accepted_epoch;
        let ctl = SupMsg::Control {
            op: SteerOp::Redirect,
            child: "a".into(),
            sup_epoch: epoch,
            delta_sha: Some(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            ),
            now_ms: 2_000,
        };
        go(&mut c, &mut j, &f, vec![ctl], 2_000);
        assert_eq!(c.model.steers[&1].state, SteerState::Pending);
        (c, j, f)
    }

    #[test]
    fn with_a_hint_a_steer_ack_is_folded_within_200ms_of_a_5s_tick() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let d = tempfile::tempdir().unwrap();
        let (c, j, f) = steered_core(d.path());
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        let (stop, acked) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let h = spawn_loop(
            c,
            j,
            f.clone(),
            rx,
            std::time::Duration::from_secs(5),
            stop.clone(),
            acked.clone(),
        );
        std::thread::sleep(std::time::Duration::from_millis(150)); // the loop is now waiting out its tick
        f.0.lock().unwrap().obs.push(SupMsg::Cursor {
            child: "a".into(),
            cursor: 10,
        }); // toron next
        let t = std::time::Instant::now();
        tx.send(()).unwrap();
        while !acked.load(Ordering::Relaxed) && t.elapsed() < std::time::Duration::from_secs(2) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let took = t.elapsed();
        stop.store(true, Ordering::Relaxed);
        let _ = tx.send(());
        let core = h.join().unwrap();
        assert!(
            acked.load(Ordering::Relaxed) && took < std::time::Duration::from_millis(200),
            "ack took {took:?}"
        );
        assert_eq!(core.model.steers[&1].state, SteerState::Acked);
    }

    #[test]
    fn without_the_hint_the_same_ack_waits_for_the_tick_and_folds_the_same_model() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        // Hinted run.
        let (c, j, f) = steered_core(d1.path());
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        let (stop, acked) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let h = spawn_loop(
            c,
            j,
            f.clone(),
            rx,
            std::time::Duration::from_millis(400),
            stop.clone(),
            acked.clone(),
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        f.0.lock().unwrap().obs.push(SupMsg::Cursor {
            child: "a".into(),
            cursor: 10,
        });
        tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        let _ = tx.send(());
        let hinted = h.join().unwrap();
        // The channel dropped: only the tick drives it.
        let (c, j, f) = steered_core(d2.path());
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        drop(tx);
        let (stop, acked) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let h = spawn_loop(
            c,
            j,
            f.clone(),
            rx,
            std::time::Duration::from_millis(400),
            stop.clone(),
            acked.clone(),
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
        f.0.lock().unwrap().obs.push(SupMsg::Cursor {
            child: "a".into(),
            cursor: 10,
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(
            !acked.load(Ordering::Relaxed),
            "no hint: not yet (the tick is 400 ms)"
        );
        let t = std::time::Instant::now();
        while !acked.load(Ordering::Relaxed) && t.elapsed() < std::time::Duration::from_secs(2) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        let plain = h.join().unwrap();
        assert!(
            acked.load(Ordering::Relaxed),
            "the tick still folds it: a lost hint costs one tick"
        );
        assert_eq!(plain.model.steers[&1].state, hinted.model.steers[&1].state);
        assert_eq!(
            crate::supervisor_program::crew_view(&plain.model),
            crate::supervisor_program::crew_view(&hinted.model)
        );
    }

    #[test]
    fn the_hints_word_defaults_on_and_off_turns_them_off() {
        let _g = crate::config::lock_env();
        unsafe { std::env::remove_var("FLYWHEEL_SUPERVISOR_HINTS") };
        assert!(HostCfg::from_env().hints);
        unsafe { std::env::set_var("FLYWHEEL_SUPERVISOR_HINTS", "off") };
        assert!(!HostCfg::from_env().hints);
        unsafe { std::env::remove_var("FLYWHEEL_SUPERVISOR_HINTS") };
    }

    // ---- 4b.8: steer@1 body. The doorbell rings; the pane never sees the delta.

    fn send(op: SteerOp, sha: Option<&str>) -> SteerSend {
        SteerSend {
            seq: 1,
            child: "a".into(),
            op,
            generation: 2,
            sup_epoch: 2,
            urgent: false,
            delta_sha: sha.map(Into::into),
        }
    }

    const NPUB: &str = "abababababababababababababababababababababababababababababababab";

    fn built(
        s: &SteerSend,
        delta: Option<&crate::steer_delta::Delta>,
    ) -> toron_core::envelope::Envelope {
        steer_envelope("proj", NPUB, 0, NPUB, s, delta).unwrap()
    }

    #[test]
    fn append_steer_redirect_is_steer_v1_urgent_with_delta_and_resume_is_normal() {
        let delta = crate::steer_delta::parse(
            br#"{"goal":"port the parser to the new AST","acceptance":"cargo test"}"#,
        )
        .unwrap();
        // urgent: false on purpose. Priority follows the op, not the flag.
        let redirect = built(&send(SteerOp::Redirect, Some(&delta.sha)), Some(&delta));
        assert_eq!(redirect.contract.name, "steer");
        assert_eq!(redirect.contract.major, 1);
        assert_eq!(redirect.body["priority"], "urgent");
        assert_eq!(redirect.body["op"], "redirect");
        assert_eq!(redirect.body["sup_epoch"], 2);
        assert!(redirect.body.get("ack_by").is_none());
        let obj = redirect.body["delta"].as_object().expect("delta object");
        assert_eq!(obj["goal"], "port the parser to the new AST");
        let canonical = serde_json::to_string(&redirect.body["delta"]).unwrap();
        assert_eq!(
            crate::steer_delta::sha_hex(canonical.as_bytes()),
            delta.sha,
            "body delta hashes to the sha the log stores"
        );
        // No epoch: the caller passes 0. The body records that, it does not invent one.
        let mut none = send(SteerOp::Redirect, Some(&delta.sha));
        none.sup_epoch = 0;
        assert_eq!(built(&none, Some(&delta)).body["sup_epoch"], 0);

        let resume = built(&send(SteerOp::Resume, None), None);
        assert_eq!(resume.contract.name, "steer");
        assert_eq!(resume.contract.major, 1);
        assert_eq!(resume.body["priority"], "normal");
        assert_eq!(resume.body["op"], "resume");
        assert!(resume.body.get("delta").is_none(), "resume omits delta");

        // A pause is urgent and still has no delta. The delta bytes are not a pane line.
        let pause = steer_v1(&send(SteerOp::Pause, None), Some(&delta));
        assert_eq!(pause.priority, "urgent");
        assert!(pause.delta.is_none());
        let rendered = serde_json::to_string(&redirect.body).unwrap();
        assert!(rendered.contains("port the parser"));
        let wake = crate::pane_write::wake_line(
            crate::pane_write::Wm(7),
            crate::pane_write::Urgency::Urgent,
        );
        assert!(!wake.contains("port the parser") && !wake.contains(&delta.sha));
    }

    #[test]
    fn escalate_v1_requires_a_reason_and_an_empty_evidence_list_stays_empty() {
        let ok = escalate_v1("proj", "restart_intensity", &[]).unwrap();
        assert_eq!(ok.context, "proj");
        assert_eq!(ok.reason, "restart_intensity");
        assert!(ok.evidence.is_empty());
        let one = escalate_v1("proj", "restart_intensity", &["log:1".into()]).unwrap();
        assert_eq!(one.evidence, vec!["log:1".to_string()]);
        assert!(escalate_v1("proj", "  ", &[]).is_err());
        let env = escalate_envelope("proj", NPUB, 0, NPUB, "restart_intensity", &[]).unwrap();
        assert_eq!(env.contract.name, "escalate");
        assert_eq!(env.contract.major, 1);
    }

    fn steer_cmd(op: &str, delta_file: Option<PathBuf>) -> SupervisorCmd {
        SupervisorCmd::Steer {
            project: "proj".into(),
            child: "a".into(),
            context: None,
            op: op.into(),
            sup_epoch: Some(2),
            delta_file,
        }
    }

    fn queued() -> String {
        std::fs::read_to_string(CtxPaths::new(&CtxPaths::default_dir(), "proj").inbox)
            .unwrap_or_default()
    }

    #[test]
    fn the_cli_validates_the_delta_first_and_a_bad_one_queues_nothing() {
        let _g = crate::config::lock_env();
        let d = tempfile::tempdir().unwrap();
        let w = |name: &str, body: &[u8]| {
            let p = d.path().join(name);
            std::fs::write(&p, body).unwrap();
            p
        };
        // No file: refused.
        assert!(run_cli(&steer_cmd("redirect", None)).is_err());
        // Unknown key, wrong shape, not JSON, too big: refused.
        for (n, b) in [
            ("k.json", br#"{"goal":"x","rm":"-rf"}"#.to_vec()),
            ("s.json", br#"{"goal":5}"#.to_vec()),
            ("j.json", b"nope".to_vec()),
            (
                "big.json",
                format!(r#"{{"goal":"{}"}}"#, "a".repeat(9000)).into_bytes(),
            ),
        ] {
            assert!(
                run_cli(&steer_cmd("redirect", Some(w(n, &b)))).is_err(),
                "{n}"
            );
        }
        // A file on another op: refused.
        assert!(run_cli(&steer_cmd("pause", Some(w("ok.json", br#"{"goal":"x"}"#)))).is_err());
        assert_eq!(queued(), "", "nothing was queued by any refused call");
        assert!(
            !CtxPaths::default_dir()
                .join(format!("{}.deltas", crate::state::state_key("proj")))
                .exists(),
            "and nothing stashed"
        );
    }

    #[test]
    fn a_good_delta_is_stashed_owner_only_and_the_input_names_only_its_sha() {
        let _g = crate::config::lock_env();
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("d.json");
        std::fs::write(
            &f,
            br#"{"scope":{"paths":["src/x.rs"]},"goal":"secret-ish goal text"}"#,
        )
        .unwrap();
        run_cli(&steer_cmd("redirect", Some(f))).unwrap();
        let line = queued();
        let m: SupMsg = serde_json::from_str(line.trim()).unwrap();
        let SupMsg::Control {
            op: SteerOp::Redirect,
            delta_sha: Some(sha),
            ..
        } = m
        else {
            panic!("{line}")
        };
        assert!(
            !line.contains("secret-ish"),
            "the input file carries the sha, not the delta"
        );
        let got = crate::steer_delta::load(&CtxPaths::default_dir(), "proj", &sha).unwrap();
        assert_eq!(got.sha, sha);
        #[cfg(unix)]
        {
            let dir =
                CtxPaths::default_dir().join(format!("{}.deltas", crate::state::state_key("proj")));
            assert_eq!(crate::private_fs::mode_of(&dir), 0o700);
            assert_eq!(
                crate::private_fs::mode_of(&dir.join(format!("{sha}.json"))),
                0o600
            );
        }
        // Other ops still queue without a file.
        run_cli(&steer_cmd("cancel", None)).unwrap();
    }

    #[test]
    fn a_kill_at_any_point_replays_to_the_same_model_and_view() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a"), spawn("b")], 1_000);
        let epoch = c.model.accepted_epoch;
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Control {
                op: SteerOp::Redirect,
                child: "a".into(),
                sup_epoch: epoch,
                delta_sha: Some(D.into()),
                now_ms: 2_000,
            }],
            2_000,
        );
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Exit {
                child: "b".into(),
                now_ms: 3_000,
            }],
            3_000,
        );
        let want = c.model.clone();
        drop(c);
        // "kill -9": reopen from the log alone.
        let paths = CtxPaths::new(d.path(), "proj");
        let (msgs, _, _) = read_log(&paths).unwrap();
        let (replayed, _) = crate::supervisor_program::replay(&msgs);
        assert_eq!(replayed, want);
        assert_eq!(crew_view(&replayed), crew_view(&want));
        assert!(
            replayed
                .steers
                .values()
                .any(|s| s.state == SteerState::Pending)
        );
        assert_eq!(replayed.children.len(), 2, "same child set");
    }

    #[test]
    fn reopening_claims_the_next_epoch_and_keeps_the_children() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        assert_eq!(c.model.accepted_epoch, 1);
        drop(c);
        let c2 = open(d.path());
        assert_eq!(c2.model.accepted_epoch, 2);
        assert!(c2.model.children.contains_key("a"));
        assert_eq!(c2.model.status, Status::Running);
    }

    #[test]
    fn a_different_host_process_over_the_same_log_runs_it_and_replay_agrees() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        drop(c);
        let mut c2 =
            Core::open(CtxPaths::new(d.path(), "proj"), "lead-2", Tier::Lead, None).unwrap();
        c2.claim("lead-2").unwrap();
        assert_eq!(
            c2.model.status,
            Status::Running,
            "a restart is not its own supersession"
        );
        assert_eq!(c2.model.leader, "lead-2");
        let (msgs, _, _) = read_log(&CtxPaths::new(d.path(), "proj")).unwrap();
        let (replayed, _) = crate::supervisor_program::replay(&msgs);
        assert_eq!(replayed, c2.model);
    }

    #[test]
    fn recovery_runs_what_the_log_asked_and_the_journal_never_settled_and_skips_the_rest() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a"), spawn("b")], 1_000);
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Exit {
                child: "a".into(),
                now_ms: 2_000,
            }],
            2_000,
        );
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Exit {
                child: "b".into(),
                now_ms: 2_100,
            }],
            2_100,
        );
        assert_eq!(f.0.lock().unwrap().relaunched.len(), 2);
        // Crash model: the second relaunch's journal row never settled.
        j.rows.get_mut("relaunch:b:2").unwrap().0 = false;
        drop(c);
        let f2 = Fake::default();
        let mut c2 = open(d.path());
        let mut p2 = f2.clone();
        let rep = recover(&mut c2, &mut j, &mut p2).unwrap();
        assert_eq!(
            f2.0.lock().unwrap().relaunched,
            vec![("b".to_string(), 2)],
            "only the unsettled one"
        );
        assert!(rep.skipped >= 1);
    }

    #[test]
    fn a_failed_relaunch_is_fed_back_and_spends_the_budget() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        f.0.lock().unwrap().fail_relaunch = true;
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let rep = go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Exit {
                child: "a".into(),
                now_ms: 2_000,
            }],
            2_000,
        );
        assert!(rep.failed >= 1);
        assert!(c.model.failed_cmds >= 1);
        assert!(
            matches!(c.model.children["a"].state, ChildState::Parked)
                || c.model.status == Status::Failed
                || c.model.restarts.len() >= 2,
            "each failed relaunch is another incarnation, bounded by poison/intensity"
        );
        assert!(c.model.children["a"].generation >= 2);
    }

    #[test]
    fn a_steer_is_appended_urgent_then_acked_by_the_cursor_and_the_gate_opens() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let e = c.model.accepted_epoch;
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Control {
                op: SteerOp::Cancel,
                child: "a".into(),
                sup_epoch: e,
                delta_sha: None,
                now_ms: 2_000,
            }],
            2_000,
        );
        let s = f.0.lock().unwrap().steers.clone();
        assert_eq!(s.len(), 1);
        assert!(s[0].urgent && s[0].op == SteerOp::Cancel && s[0].generation == 1);
        assert_eq!(
            c.model.steers[&1].offset,
            Some(10),
            "the offset came back as a fact"
        );
        // While pending, the child's done is refused (P13).
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Done {
                child: "a".into(),
                generation: 1,
                now_ms: 2_500,
            }],
            2_500,
        );
        assert_eq!(
            f.0.lock().unwrap().rejects.last().unwrap().1,
            "steer_pending"
        );
        // The cursor reaches the steer: acked.
        f.0.lock().unwrap().obs.push(SupMsg::Cursor {
            child: "a".into(),
            cursor: 10,
        });
        go(&mut c, &mut j, &f, vec![], 3_000);
        assert_eq!(c.model.steers[&1].state, SteerState::Acked);
    }

    #[test]
    fn a_stale_epoch_cancel_from_the_input_file_is_rejected_and_nothing_is_sent() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        c.claim("lead-1").unwrap(); // epoch 2
        append_input(
            c.paths(),
            &SupMsg::Control {
                op: SteerOp::Cancel,
                child: "a".into(),
                sup_epoch: 1,
                delta_sha: None,
                now_ms: 2_000,
            },
        )
        .unwrap();
        go(&mut c, &mut j, &f, vec![], 2_000);
        let w = f.0.lock().unwrap();
        assert!(w.steers.is_empty(), "no envelope, no interrupt");
        assert_eq!(w.rejects.last().unwrap().1, "stale_epoch");
        assert!(c.model.steers.is_empty());
    }

    #[test]
    fn inputs_are_read_once_across_a_restart_and_a_half_line_waits() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let p = c.paths().clone();
        append_input(
            &p,
            &SupMsg::Exit {
                child: "a".into(),
                now_ms: 2_000,
            },
        )
        .unwrap();
        // A torn write: no newline yet.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&p.inbox)
            .unwrap()
            .write_all(b"{\"kind\":\"exit\"")
            .unwrap();
        go(&mut c, &mut j, &f, vec![], 2_000);
        assert_eq!(f.0.lock().unwrap().relaunched.len(), 1);
        drop(c);
        let mut c2 = open(d.path());
        go(&mut c2, &mut j, &f, vec![], 3_000);
        assert_eq!(f.0.lock().unwrap().relaunched.len(), 1, "not re-read");
    }

    #[test]
    fn quiet_ticks_do_not_grow_the_log() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        let before = std::fs::read_to_string(&c.paths().log)
            .unwrap()
            .lines()
            .count();
        for t in 2..20u64 {
            go(&mut c, &mut j, &f, vec![], t * 100);
        }
        let after = std::fs::read_to_string(&c.paths().log)
            .unwrap()
            .lines()
            .count();
        assert_eq!(before, after);
    }

    #[test]
    fn the_fourth_failure_pages_once_and_stops_the_supervisor() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a"), spawn("b")], 1_000);
        let ex = |id: &str, t| SupMsg::Exit {
            child: id.into(),
            now_ms: t,
        };
        for (i, id) in ["a", "b", "a", "b"].iter().enumerate() {
            go(
                &mut c,
                &mut j,
                &f,
                vec![ex(id, 2_000 + i as u64)],
                2_000 + i as u64,
            );
        }
        let w = f.0.lock().unwrap();
        assert_eq!(w.relaunched.len(), 3);
        assert_eq!(w.escalated.len(), 1, "one escalation");
        assert_eq!(w.escalated[0].1, "restart_intensity");
        drop(w);
        assert_eq!(c.model.status, Status::Failed);
        assert_eq!(
            f.0.lock().unwrap().exited.len(),
            2,
            "the subtree was cancelled"
        );
    }

    #[test]
    fn a_failed_log_replays_failed_but_a_new_host_is_a_new_incarnation_that_runs() {
        // D6: after "restart intensity exceeded" the message says re-run
        // supervise; a re-run used to exit at once until the log was deleted.
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a"), spawn("b")], 1_000);
        let ex = |id: &str, t| SupMsg::Exit {
            child: id.into(),
            now_ms: t,
        };
        for (i, id) in ["a", "b", "a", "b"].iter().enumerate() {
            go(
                &mut c,
                &mut j,
                &f,
                vec![ex(id, 2_000 + i as u64)],
                2_000 + i as u64,
            );
        }
        assert_eq!(c.model.status, Status::Failed);
        drop(c);
        let paths = CtxPaths::new(d.path(), "proj");
        // The log alone still says Failed: replay is faithful (P8).
        let (msgs, _, _) = read_log(&paths).unwrap();
        assert_eq!(
            crate::supervisor_program::replay(&msgs).0.status,
            Status::Failed
        );
        // A new host process claims the next epoch and runs.
        let mut c2 = Core::open(paths.clone(), "lead-2", Tier::Lead, None).unwrap();
        c2.claim("lead-2").unwrap();
        assert_eq!(
            c2.model.status,
            Status::Running,
            "a re-run is a fresh incarnation"
        );
        assert!(
            c2.model.restarts.is_empty(),
            "the intensity window starts over"
        );
        assert_eq!(c2.model.escalations, 1, "the escalation fact is kept");
        // The same effect journal: a re-run keeps the old keys settled, so the
        // new incarnation's commands must not collide with them.
        let j2 = &mut j;
        let before = f.0.lock().unwrap().relaunched.len();
        go(&mut c2, j2, &f, vec![spawn("a")], 10_000);
        go(&mut c2, j2, &f, vec![ex("a", 10_100)], 10_100);
        assert_eq!(c2.model.status, Status::Running);
        assert_eq!(
            f.0.lock().unwrap().relaunched.len(),
            before + 1,
            "the new host supervises the crew again"
        );
        let (msgs, _, _) = read_log(&paths).unwrap();
        let replayed = crate::supervisor_program::replay(&msgs).0;
        assert_eq!(replayed.status, Status::Running);
        assert_eq!(replayed, c2.model, "replay of the extended log agrees");
    }

    #[test]
    fn a_stale_claims_settle_does_not_close_the_key() {
        let mut j = MemJournal::default();
        let g1 = match j.begin("relaunch:a:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        let g2 = match j.begin("relaunch:a:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        assert!(g2 > g1);
        j.finish("relaunch:a:2", g1, true);
        assert!(
            matches!(j.begin("relaunch:a:2"), Begin::Run(_)),
            "the older claim settled nothing"
        );
        j.finish("relaunch:a:2", g2 + 1, true);
        assert_eq!(
            j.begin("relaunch:a:2"),
            Begin::Skip,
            "the current claim settles it"
        );
    }

    // ---- the real journal -------------------------------------------------

    fn toron_journal() -> (ToronEffectJournal, tokio::runtime::Runtime) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap().keep();
        let db = dir.join("index.sqlite");
        let pool = rt.block_on(async {
            let pool = toron_core::index::schema::connect(&db.to_string_lossy())
                .await
                .unwrap();
            toron_core::index::schema::migrate(&pool).await.unwrap();
            pool
        });
        let jd = toron_core::journal::JournalDb::new(pool, "supervise:test", 64);
        (
            ToronEffectJournal::new(
                toron_core::journal::ToronJournal::spawn(jd, rt.handle()),
                "me",
            ),
            rt,
        )
    }

    /// rv23 #6, nested leads: the root's settled `steer:1` does not settle
    /// the nested lead's own `steer:1` (the two journals share one database).
    #[test]
    fn a_nested_lead_has_its_own_effect_journal() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        let pool = rt.block_on(async {
            let pool = toron_core::index::schema::connect(&db.to_string_lossy())
                .await
                .unwrap();
            toron_core::index::schema::migrate(&pool).await.unwrap();
            pool
        });
        let open = |scope: &str, owner: &str| {
            let jd = toron_core::journal::JournalDb::new(pool.clone(), scope, 64);
            ToronEffectJournal::new(
                toron_core::journal::ToronJournal::spawn(jd, rt.handle()),
                owner,
            )
        };
        let (rs, ls) = (journal_scope("proj", None), journal_scope("proj", Some("plane")));
        assert_eq!(rs, format!("supervise:{}", crate::state::state_key("proj")));
        let mut root = open(&rs, "root");
        let mut lead = open(&ls, "lead");
        let g = match root.begin("steer:1") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        root.finish("steer:1", g, true);
        assert_eq!(root.begin("steer:1"), Begin::Skip);
        assert!(
            matches!(lead.begin("steer:1"), Begin::Run(_)),
            "the nested lead's steer:1 is not the root's"
        );
    }

    /// Two hosts' journals over ONE store (a kill -9 and a restart).
    fn toron_journals(
        owners: &[&str],
    ) -> (Vec<ToronEffectJournal>, tokio::runtime::Runtime) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = tempfile::tempdir().unwrap().keep();
        let db = dir.join("index.sqlite");
        let pool = rt.block_on(async {
            let pool = toron_core::index::schema::connect(&db.to_string_lossy())
                .await
                .unwrap();
            toron_core::index::schema::migrate(&pool).await.unwrap();
            pool
        });
        let js = owners
            .iter()
            .map(|o| {
                let jd = toron_core::journal::JournalDb::new(pool.clone(), "supervise:test", 64);
                ToronEffectJournal::new(toron_core::journal::ToronJournal::spawn(jd, rt.handle()), o)
            })
            .collect();
        (js, rt)
    }

    /// rv23 S2, the regression: the host dies (kill -9) holding a live claim
    /// on `relaunch:b:2`. The restarted host (same context, next sup_epoch)
    /// must take that claim over and run the relaunch, not be refused (a
    /// refusal became `CmdFailed`, an extra generation and a spent slot).
    #[test]
    fn a_restarted_host_takes_over_its_dead_predecessors_relaunch_claim() {
        let dead = journal_owner("supervise-p", 1);
        let next = journal_owner("supervise-p", 2);
        let other_ctx = journal_owner("supervise-p--lead", 1);
        let newer = journal_owner("supervise-p", 3);
        let (mut js, _rt) = toron_journals(&[&dead, &next, &other_ctx, &newer]);
        let g1 = match js[0].begin("relaunch:b:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        // The predecessor never settles: it was killed.
        let g2 = match js[1].begin("relaunch:b:2") {
            Begin::Run(g) => g,
            other => panic!("the restarted host must take over: {other:?}"),
        };
        assert!(g2 > g1, "the takeover fences the dead claim");
        js[1].finish("relaunch:b:2", g2, true);
        assert_eq!(js[1].begin("relaunch:b:2"), Begin::Skip);
        // Never taken: another context's live claim, or a NEWER epoch's.
        assert!(matches!(js[2].begin("relaunch:x:2"), Begin::Run(_)));
        assert_eq!(js[1].begin("relaunch:x:2"), Begin::Held(other_ctx.clone()));
        assert!(matches!(js[3].begin("relaunch:y:2"), Begin::Run(_)));
        assert_eq!(js[1].begin("relaunch:y:2"), Begin::Held(newer.clone()));
    }

    /// rv23 S2, the nested-lead half: the lead host dies (kill -9) holding
    /// its own `relaunch:b:2`. Its successor in the SAME lead context at the
    /// next sup_epoch takes that claim over and runs it once. No extra
    /// generation and no extra restart slot: the fold sees `Relaunched`, never
    /// `CmdFailed`. The root never steals the lead's claim, and a newer epoch
    /// of the lead fences the older one.
    #[test]
    fn a_restarted_nested_lead_takes_over_its_own_relaunch_claim() {
        let key = crate::state::state_key("proj");
        let lead_ws = supervise_lease_workspace("proj", Some("plane"));
        assert_eq!(lead_ws, format!("supervise-{key}--plane"));
        let lead1 = journal_owner(&lead_ws, 1);
        let lead2 = journal_owner(&lead_ws, 2);
        let lead3 = journal_owner(&lead_ws, 3);
        let root = journal_owner(&supervise_lease_workspace("proj", None), 2);
        assert_eq!(lead1, format!("supervise-{key}--plane@e1"));
        let (mut js, _rt) = toron_journals(&[&lead1, &lead2, &lead3, &root]);
        // The wiring, not just the rule: the owner is the context lease plus
        // the epoch, so a successor recomputes the SAME context and only the
        // epoch moves. A pid or the root's workspace here would never take over.
        let src = include_str!("supervisor_host.rs");
        let at = src
            .find("let mut journal = ToronEffectJournal::open(")
            .expect("host_loop journal");
        assert!(
            src[at..at + 240].contains("journal_owner(lease_ws, epoch)"),
            "{}",
            &src[at..at + 240]
        );
        // The dead lead claimed the relaunch and never settled.
        let g1 = match js[0].begin("relaunch:x:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        // Its successor, same lead context at the next epoch, takes it over.
        let g2 = match js[1].begin("relaunch:x:2") {
            Begin::Run(g) => g,
            other => panic!("the restarted lead must take over: {other:?}"),
        };
        assert!(g2 > g1, "the takeover fences the dead claim");
        // The fence is the generation: the dead host's late settle, at the
        // generation it held, cannot land on the taken-over row.
        js[0].finish("relaunch:x:2", g1, true);
        let g3 = match js[1].begin("relaunch:x:2") {
            Begin::Run(g) => g,
            other => panic!("a stale settle must not close the claim: {other:?}"),
        };
        js[1].finish("relaunch:x:2", g3, true);
        assert_eq!(js[1].begin("relaunch:x:2"), Begin::Skip);
        // The root is another context: it never steals the lead's live claim.
        let held = match js[1].begin("relaunch:y:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        assert_eq!(js[3].begin("relaunch:y:2"), Begin::Held(lead2.clone()));
        // A newer epoch of the lead does take the older one's claim.
        let g4 = match js[2].begin("relaunch:y:2") {
            Begin::Run(g) => g,
            other => panic!("the newer epoch of the lead must take over: {other:?}"),
        };
        assert!(g4 > held, "the newer epoch supersedes the older claim");
        // The fold half: the dead host logged the exit, claimed the relaunch,
        // and was killed before it settled. The successor must run that one
        // command at its original generation, spending no second slot.
        let d = tempfile::tempdir().unwrap();
        let me = "supervise-proj-plane-1";
        let mut dead =
            Core::open(CtxPaths::with_context(d.path(), "proj", "plane"), me, Tier::Lead, None)
                .unwrap();
        dead.claim(me).unwrap();
        assert_eq!(dead.model.accepted_epoch, 1);
        let f = Fake::default();
        go(&mut dead, &mut MemJournal::default(), &f, vec![spawn("b")], 1_000);
        let cmds = dead
            .append_fold(SupMsg::Exit {
                child: "b".into(),
                now_ms: 2_000,
            })
            .unwrap();
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                SupCmd::Relaunch {
                    child,
                    generation: 2,
                    ..
                } if child == "b"
            )),
            "the exit asks for relaunch:b:2: {cmds:?}"
        );
        assert!(matches!(js[0].begin("relaunch:b:2"), Begin::Run(_)));
        drop(dead); // kill -9: the lead's claim is never settled.
        let me2 = "supervise-proj-plane-2";
        let mut live =
            Core::open(CtxPaths::with_context(d.path(), "proj", "plane"), me2, Tier::Lead, None)
                .unwrap();
        live.claim(me2).unwrap();
        assert_eq!(live.model.accepted_epoch, 2);
        let f2 = Fake::default();
        let mut p2 = f2.clone();
        let rep = recover(&mut live, &mut js[1], &mut p2).unwrap();
        assert_eq!(rep.failed, 0, "a takeover is not a failure: {rep:?}");
        assert_eq!(rep.held, 0, "the lead's successor is its heir, not a rival");
        assert_eq!(
            f2.0.lock().unwrap().relaunched,
            vec![("b".to_string(), 2)],
            "the lead's claim runs once"
        );
        let b = live.model.children.get("b").expect("b");
        assert_eq!(b.state, ChildState::Running);
        assert_eq!(b.generation, 2, "no extra generation");
        assert_eq!(live.model.restarts.len(), 1, "no extra restart slot");
        assert_eq!(live.model.failed_cmds, 0);
    }

    #[test]
    fn takes_over_only_the_same_context_at_a_lower_epoch() {
        assert!(takes_over("supervise-p@e2", "supervise-p@e1"));
        assert!(!takes_over("supervise-p@e2", "supervise-p@e2"));
        assert!(!takes_over("supervise-p@e2", "supervise-p@e3"));
        assert!(!takes_over("supervise-p@e2", "supervise-p--lead@e1"));
        assert!(!takes_over("supervise-p@e2", "supervise-p-123"));
        assert!(!takes_over("supervise-p-123", "supervise-p@e1"));
    }

    /// A held claim reaches the fold as `CmdHeld`, never `CmdFailed`.
    #[test]
    fn a_held_claim_is_reported_as_held_not_failed() {
        struct HeldJ;
        impl EffectJournal for HeldJ {
            fn begin(&mut self, _: &str) -> Begin {
                Begin::Held("supervise-p@e1".into())
            }
            fn finish(&mut self, _: &str, _: u64, _: bool) {}
        }
        let f = Fake::default();
        let mut p = f.clone();
        let mut rep = RoundReport::default();
        let back = execute(
            &mut HeldJ,
            &mut p,
            &SupCmd::Relaunch {
                child: "b".into(),
                generation: 2,
                kind: ChildKind::Crew,
            },
            &mut rep,
        );
        assert_eq!(
            back,
            vec![SupMsg::CmdHeld {
                key: "relaunch:b:2".into(),
                holder: "supervise-p@e1".into()
            }]
        );
        assert_eq!((rep.failed, rep.held), (0, 1));
        assert!(f.0.lock().unwrap().relaunched.is_empty(), "nothing ran");
    }

    #[test]
    fn toron_journal_runs_a_key_once_and_reruns_one_that_never_settled() {
        let (mut j, _rt) = toron_journal();
        let g = match j.begin("relaunch:a:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        j.finish("relaunch:a:2", g, true);
        assert_eq!(j.begin("relaunch:a:2"), Begin::Skip, "settled: never again");
        // Claimed, effect lost, never settled: runs again under a newer claim.
        let g1 = match j.begin("relaunch:b:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        let g2 = match j.begin("relaunch:b:2") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        assert!(g2 > g1, "a newer claim fences the older");
        j.finish("relaunch:b:2", g1, true); // the stale claim settles nothing
        assert!(matches!(j.begin("relaunch:b:2"), Begin::Run(_)));
        // A failed settle is terminal too.
        let g3 = match j.begin("park:c") {
            Begin::Run(g) => g,
            other => panic!("{other:?}"),
        };
        j.finish("park:c", g3, false);
        assert_eq!(j.begin("park:c"), Begin::Skip);
    }

    #[test]
    fn the_host_runs_end_to_end_on_the_toron_journal() {
        let d = tempfile::tempdir().unwrap();
        let (mut j, _rt) = toron_journal();
        let f = Fake::default();
        let mut c = open(d.path());
        let mut p = f.clone();
        round(&mut c, &mut j, &mut p, vec![spawn("a")], 1_000).unwrap();
        round(
            &mut c,
            &mut j,
            &mut p,
            vec![SupMsg::Exit {
                child: "a".into(),
                now_ms: 2_000,
            }],
            2_000,
        )
        .unwrap();
        assert_eq!(f.0.lock().unwrap().relaunched, vec![("a".to_string(), 2)]);
        // Same command again (a replay): the journal says settled.
        let mut rep = RoundReport::default();
        let again = execute(
            &mut j,
            &mut p,
            &SupCmd::Relaunch {
                child: "a".into(),
                generation: 2,
                kind: ChildKind::Crew,
            },
            &mut rep,
        );
        assert!(again.is_empty() && rep.skipped == 1);
        assert_eq!(f.0.lock().unwrap().relaunched.len(), 1);
    }

    // ---- the one path ------------------------------------------------------

    /// ADR-0018 §3 item 4: the supervisor is ONE mechanism.
    ///
    /// The rehost landed behind the comms flag with the legacy child-process
    /// loop kept for the flag-off case, so `loop-mode supervise` had two
    /// implementations and a config flag to choose between them. The program
    /// does everything the legacy loop did, so the legacy loop and its gate
    /// are gone.
    ///
    /// INVERTED: put the gate or the legacy pass back and this goes red —
    /// which is the defect it pins.
    #[test]
    fn supervise_has_one_path_the_program() {
        let src = include_str!("main.rs");
        let a = src.find("fn cmd_loop_supervise(").unwrap();
        let body = &src[a..a + 2_600];
        assert!(
            body.contains("supervisor_host::run_supervise("),
            "the command runs the SupervisorProgram"
        );
        assert!(
            !body.contains("comms_send::enabled(cfg)"),
            "no config flag picks between two supervisors"
        );
        assert!(
            !body.contains("supervise_pass_argv("),
            "the legacy child-process pass is gone"
        );
        assert!(
            !body.contains("std::process::Command::new(&exe)"),
            "the legacy loop spawned the pass itself"
        );
        assert!(
            !src.contains("SUPERVISE_MAX_CONSECUTIVE_FAILURES")
                && !src.contains("supervise_failure_count("),
            "the legacy failure-count rule is gone with the legacy loop"
        );
    }

    #[test]
    fn an_assign_does_not_write_a_pane() {
        use crate::supervisor_program::ReadyBead;
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(&mut c, &mut j, &f, vec![spawn("a")], 1_000);
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::ReadyBeads {
                beads: vec![ReadyBead {
                    id: "t1".into(),
                    scope_paths: vec!["src/a.rs".into()],
                    deps_proved: true,
                    role: "a".into(),
                }],
                now_ms: 1_500,
            }],
            1_500,
        );
        let w = f.0.lock().unwrap();
        assert!(w.exited.is_empty(), "assign must not Exit a pane");
        assert!(w.steers.is_empty(), "assign must not write pane text");
        assert!(w.relaunched.is_empty());
        assert_eq!(w.assigns, vec![("t1".into(), "a".into(), 1)]);
        drop(w);
        assert!(require_npub("a", None).is_err());
        let npub = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let prod = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let env = assign_envelope("flywheel", prod, 4, npub, "bd-1", "a", 1).unwrap();
        assert_eq!(env.contract.name, "task");
        assert_eq!(env.body["bead"], "bd-1");
        assert_eq!(env.body["title"], "bd-1");
        assert!(assign_envelope("flywheel", prod, 4, "pin-not-npub", "t1", "a", 1).is_err());
        assert!(
            j.rows.keys().any(|k| k == "assign:t1:a:1"),
            "assign is journaled, not written to a pane: {:?}",
            j.rows.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_lead_relaunch_is_recorded_and_does_not_touch_a_pane() {
        let d = tempfile::tempdir().unwrap();
        let (mut c, mut j, f) = (open(d.path()), MemJournal::default(), Fake::default());
        go(
            &mut c,
            &mut j,
            &f,
            vec![spawn_as("plane", ChildKind::Lead)],
            1_000,
        );
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::ChildEscalated {
                child: "plane".into(),
                reason: "down".into(),
                now_ms: 2_000,
                fact_key: None,
            }],
            2_000,
        );
        let w = f.0.lock().unwrap();
        assert_eq!(w.relaunched, vec![("plane".to_string(), 2)]);
        assert!(w.exited.is_empty(), "lead relaunch must not Exit a pane");
        assert!(
            w.steers.is_empty(),
            "lead relaunch must not write pane text"
        );
        assert_eq!(
            relaunch_argv("proj", "plane", ChildKind::Lead),
            lead_supervise_argv("proj", "plane")
        );
        assert_eq!(
            lead_supervise_argv("proj", "plane"),
            ["loop-mode", "supervise", "proj", "--context", "plane"]
        );
        let crew = relaunch_argv("proj", "plane", ChildKind::Crew);
        assert_eq!(crew, ["crew", "control", "proj", "plane", "relaunch"]);
        assert!(!crew.iter().any(|a| a == "--context"));
    }

    /// Records each relaunch's argv. The fake port drops `kind`, so only a
    /// real `LivePorts` spawn proves a lead is nested supervise.
    fn relaunch_args_script(dir: &Path) -> PathBuf {
        let bin = dir.join("relaunch-args.sh");
        std::fs::write(
            &bin,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> \"$RELAUNCH_MARKER\"\necho --- >> \"$RELAUNCH_MARKER\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        bin
    }

    #[test]
    fn a_live_lead_relaunch_spawns_nested_supervise() {
        let _home = crate::config::lock_env();
        let _clear = ClearExec;
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("argv");
        unsafe {
            std::env::set_var(
                "FLYWHEEL_SUPERVISOR_RELAUNCH_BIN",
                relaunch_args_script(d.path()),
            )
        };
        unsafe { std::env::set_var("RELAUNCH_MARKER", &marker) };
        let cfg = crate::config::Config::default();
        let mut live = LivePorts::new(&cfg, "proj", "proj");
        let mut c = open(d.path());
        let mut j = MemJournal::default();
        round(
            &mut c,
            &mut j,
            &mut live,
            vec![
                spawn_as("plane", ChildKind::Lead),
                SupMsg::ChildEscalated {
                    child: "plane".into(),
                    reason: "down".into(),
                    now_ms: 2_000,
                    fact_key: None,
                },
            ],
            2_000,
        )
        .unwrap();
        let got = std::fs::read_to_string(&marker).expect("relaunch script did not run");
        assert!(
            got.contains("loop-mode\nsupervise\nproj\n--context\nplane\n"),
            "lead relaunch argv: {got}"
        );
        assert!(
            !got.lines().any(|l| l == "crew"),
            "a lead relaunch must not be a pane crew control: {got}"
        );
        assert_eq!(c.model.children["plane"].state, ChildState::Running);
        assert_eq!(c.model.children["plane"].spec.kind, ChildKind::Lead);
    }

    #[test]
    fn the_host_loop_runs_on_a_plain_thread_and_the_fold_never_shells_out() {
        let me = include_str!("supervisor_host.rs");
        let prog = include_str!("supervisor_program.rs");
        // The fold is pure: no process, fs, clock, or runtime in it.
        let pure = &prog[..prog.find("#[cfg(test)]").unwrap()];
        for banned in [
            "std::process",
            "std::fs",
            "std::time",
            "SystemTime",
            "Instant",
            "tokio",
            "block_on",
            "HashMap",
        ] {
            assert!(
                !pure.contains(banned),
                "supervisor_program must stay pure: {banned}"
            );
        }
        // The host never types into a pane except through the PaneWrite sink.
        let host = &me[..me.find("#[cfg(test)]").unwrap()];
        for call in crate::pane_write::PANE_TEXT_CALLS {
            assert!(
                !host.contains(call),
                "pane text call {call} outside the sink"
            );
        }
    }

    #[test]
    fn the_host_loop_waits_on_the_hint_channel_and_arms_the_watcher_unless_hints_are_off() {
        let me = include_str!("supervisor_host.rs");
        // Production slice only: the loop starts at the single `fn host_loop`
        // and ends where the test module starts, so neither the test-only
        // imports at the top nor this test's own assertions below can match.
        // (Truncating at the first `#[cfg(test)]` broke when test-only
        // imports moved above the loop; searching the whole file
        // self-matched this very assertion.)
        let start = me.find("fn host_loop(").expect("host loop exists");
        let end = me.find("mod tests").expect("test module exists");
        let body = &me[start..end];
        assert!(
            body.contains("supervisor_hints::wait(&hint_rx"),
            "the loop waits on the hints"
        );
        assert!(
            body.contains("supervisor_hints::Watcher::spawn("),
            "and something feeds them"
        );
        assert!(
            body.contains("if hc.hints {"),
            "the off switch gates the watcher"
        );
        assert!(
            body.contains("watched_paths(&index_db, &paths.inbox)"),
            "the watcher covers toron's index and this context's own input file"
        );
        assert!(
            !body.contains("thread::sleep(std::time::Duration::from_millis(hc.tick_ms))"),
            "no plain sleep left in the loop"
        );
    }

    struct ScaleGuard;

    impl ScaleGuard {
        fn set(v: &str) -> Self {
            unsafe { std::env::set_var("FLYWHEEL_SUPERVISOR_CLOCK_SCALE", v) };
            Self
        }
    }

    impl Drop for ScaleGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("FLYWHEEL_SUPERVISOR_CLOCK_SCALE") };
        }
    }

    #[test]
    fn a_dead_lead_is_adopted_once_after_the_scaled_window() {
        let _home = crate::config::lock_env();
        let _scale = ScaleGuard::set("1200000");
        let policy = HostCfg::from_env()
            .policy()
            .expect("the clock scale is a policy override");
        assert_eq!(
            policy.t_orphan_ms, 1,
            "1_200_000 ms window / scale 1_200_000 is 1 ms, not a real wait"
        );
        let d = tempfile::tempdir().unwrap();
        let mut c = Core::open(
            CtxPaths::new(d.path(), "proj"),
            "root",
            Tier::Lead,
            Some(policy),
        )
        .unwrap();
        c.claim("root").unwrap();
        let (mut j, f) = (MemJournal::default(), Fake::default());
        go(
            &mut c,
            &mut j,
            &f,
            vec![spawn_as("plane", ChildKind::Lead), spawn("plain")],
            0,
        );
        let nested = CtxPaths::with_context(d.path(), "proj", "plane");
        let mut lead = Core::open(nested.clone(), "plane-host", Tier::Lead, None).unwrap();
        lead.claim("plane-host").unwrap();
        let (mut jl, fl) = (MemJournal::default(), Fake::default());
        go(&mut lead, &mut jl, &fl, vec![spawn("a"), spawn("b")], 0);
        let nested_before = read_log(&nested).unwrap().0.len();
        let view_before: Vec<_> = crew_view(&c.model)
            .iter()
            .map(|r| r.child.clone())
            .collect();

        let mut sleeper = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let ws = supervise_lease_workspace("proj", Some("plane"));
        let path = doorbell_rt::lease::owner_file(&crate::config::state_dir(), &ws);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lease = doorbell_rt::lease::OwnerLease {
            pid: sleeper.id(),
            boot_id: "test".into(),
            host: "test".into(),
            started_unix: 1,
            heartbeat_unix: 1,
            kind: None,
        };
        std::fs::write(&path, serde_json::to_string(&lease).unwrap()).unwrap();
        let cfg = crate::config::Config::default();
        let mut live = LivePorts::new(&cfg, "proj", "proj");
        let alive = live.observe(&c.model, 1);
        assert!(
            alive.iter().all(|m| !matches!(m, SupMsg::Adopt { .. })),
            "a live lead host is not adopted: {alive:?}"
        );
        sleeper.kill().unwrap();
        let _ = sleeper.wait();
        let too_soon = live.observe(&c.model, 0);
        assert!(
            too_soon.iter().all(|m| !matches!(m, SupMsg::Adopt { .. })),
            "dead, but still inside the window: {too_soon:?}"
        );
        let due = live.observe(&c.model, 1);
        let adopt: Vec<_> = due
            .iter()
            .filter(|m| matches!(m, SupMsg::Adopt { .. }))
            .cloned()
            .collect();
        assert_eq!(adopt.len(), 1, "one adopt after the window: {due:?}");
        go(&mut c, &mut j, &f, adopt, 1);
        assert_eq!(c.model.accepted_epoch, 2);
        assert!(c.model.adopted.contains("plane"));
        assert_eq!(c.model.leader, "root");
        assert_eq!(c.model.children["plane"].state, ChildState::Running);
        assert_eq!(c.model.children["plain"].state, ChildState::Running);
        let view_after: Vec<_> = crew_view(&c.model)
            .iter()
            .map(|r| r.child.clone())
            .collect();
        assert_eq!(view_before, view_after, "view keeps the same child set");
        let again = live.observe(&c.model, 5);
        assert!(
            again.iter().all(|m| !matches!(m, SupMsg::Adopt { .. })),
            "the same death is one adopt: {again:?}"
        );
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Control {
                op: SteerOp::Cancel,
                child: "plane".into(),
                sup_epoch: 1,
                delta_sha: None,
                now_ms: 5,
            }],
            5,
        );
        let tags: Vec<_> =
            f.0.lock()
                .unwrap()
                .rejects
                .iter()
                .map(|(_, t)| t.clone())
                .collect();
        assert_eq!(tags, vec!["stale_epoch".to_string()]);
        assert_eq!(
            read_log(&nested).unwrap().0.len(),
            nested_before,
            "adoption does not rewrite the lead's log"
        );
        assert_eq!(lead.model.children["a"].state, ChildState::Running);
        assert_eq!(lead.model.children["b"].state, ChildState::Running);
        let (msgs, _, _) = read_log(&CtxPaths::new(d.path(), "proj")).unwrap();
        let (replayed, _) = crate::supervisor_program::replay(&msgs);
        assert_eq!(replayed.accepted_epoch, c.model.accepted_epoch);
        assert_eq!(replayed.adopted, c.model.adopted);
    }

    #[test]
    fn a_lead_escalation_is_one_notice_and_loop_passes_stay_inside() {
        let _home = crate::config::lock_env();
        let d = tempfile::tempdir().unwrap();
        let mut c = open(d.path());
        let (mut j, f) = (MemJournal::default(), Fake::default());
        go(
            &mut c,
            &mut j,
            &f,
            vec![spawn_as("plane", ChildKind::Lead), spawn("plain")],
            0,
        );
        let mut lead = Core::open(
            CtxPaths::with_context(d.path(), "proj", "plane"),
            "plane-host",
            Tier::Lead,
            None,
        )
        .unwrap();
        lead.claim("plane-host").unwrap();
        let (mut jl, fl) = (MemJournal::default(), Fake::default());
        go(&mut lead, &mut jl, &fl, vec![spawn("a"), spawn("b")], 0);
        let mut bump = |child: &str, now: u64| {
            go(
                &mut lead,
                &mut jl,
                &fl,
                vec![SupMsg::ChildEscalated {
                    child: child.into(),
                    reason: "down".into(),
                    now_ms: now,
                    fact_key: None,
                }],
                now,
            );
            if lead.model.status == crate::supervisor_program::Status::Running
                && lead.model.children[child].state == ChildState::Restarting
            {
                let generation = lead.model.children[child].generation;
                go(
                    &mut lead,
                    &mut jl,
                    &fl,
                    vec![SupMsg::Relaunched {
                        child: child.into(),
                        generation,
                    }],
                    now,
                );
            }
        };
        // Two crews, so poison does not park either one before the window fills.
        bump("a", 1_000);
        bump("b", 2_000);
        bump("a", 3_000);
        bump("b", 4_000);
        assert_eq!(
            lead.model.status,
            crate::supervisor_program::Status::Failed,
            "the fourth crew failure escalates the lead"
        );
        let (n, reason) = fl.0.lock().unwrap().escalated[0].clone();
        let cfg = crate::config::Config::default();
        LivePorts::new(&cfg, "proj", "proj")
            .in_context("plane")
            .escalate(n, &reason, "crew-a")
            .unwrap();
        let path = lead_facts_file("proj", Some("plane"));
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(
            "{\"kind\":\"pass\",\"n\":1}\n{\"kind\":\"progress\"}\n{\"kind\":\"done\"}\n",
        );
        std::fs::write(&path, text).unwrap();
        let mut root = LivePorts::new(&cfg, "proj", "proj");
        let msgs = root.observe(&c.model, 4_000);
        let ups: Vec<_> = msgs
            .iter()
            .filter(|m| matches!(m, SupMsg::ChildEscalated { .. }))
            .cloned()
            .collect();
        assert_eq!(ups.len(), 1, "one notice, not one per pass: {msgs:?}");
        assert!(
            msgs.iter().all(|m| !matches!(
                m,
                SupMsg::Progress { .. } | SupMsg::Done { .. } | SupMsg::PassExit { .. }
            )),
            "pass, progress, and done stay in the lead: {msgs:?}"
        );
        go(&mut c, &mut j, &f, ups, 4_000);
        assert_eq!(
            f.0.lock().unwrap().relaunched,
            vec![("plane".to_string(), 2)]
        );
        let generation = c.model.children["plane"].generation;
        go(
            &mut c,
            &mut j,
            &f,
            vec![SupMsg::Relaunched {
                child: "plane".into(),
                generation,
            }],
            4_000,
        );
        let again = root.observe(&c.model, 5_000);
        assert!(
            again
                .iter()
                .all(|m| !matches!(m, SupMsg::ChildEscalated { .. })),
            "the same escalate fact does not restart the lead again: {again:?}"
        );
    }
}

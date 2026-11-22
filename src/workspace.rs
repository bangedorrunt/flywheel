// governed-by: ADR-0027
//! One Workspace Tick — Control Plane as Elm/Redux on Doorbell + 30013
//! Single owner of control-plane types: WorkspaceModel, Msg, Cmd, tick.
//! The two existing ticks (doorbell_tick::tick, durable::tick) become
//! private slice reducers called ONLY from WorkspaceModel::tick.
//! Clock enters tick only as Msg payload (Tick{now}); no Instant now.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// Assignment receipt — durable row cached in Model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Receipt {
    pub state: String,
    pub wake: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_path: Option<String>,
}

/// Monotonic receipt progress (`owed -> acked -> resulted`, with `degraded`
/// terminal). Unknown states have no rank and keep the permissive behavior.
fn receipt_progress(state: &str) -> Option<u8> {
    match state {
        "owed" => Some(0),
        "acked" => Some(1),
        "resulted" => Some(2),
        "degraded" => Some(3),
        _ => None,
    }
}

/// Which resume shape a handoff produced. The default return is the delta,
/// never a full transcript; FullBrief names itself when it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandoffStrategy {
    DeltaSinceLastSeen,
    FullBrief,
}

/// One run the pane may have seen, oldest first. `seq` orders; `id` names.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffRun {
    pub id: String,
    pub seq: u64,
}

/// Auditable handoff row, keyed (project, pin, bead) in the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffRow {
    pub project: String,
    pub pin: String,
    pub bead: String,
    /// Last run the pane saw (inclusive). The delta starts after this.
    pub from_run: String,
    /// Newest run covered when the row was recorded.
    pub to_run: String,
    pub strategy: HandoffStrategy,
    /// Handoff artifact path.
    pub artifact: String,
    /// Run that consumed this handoff, once a resume lands. Informational:
    /// a consumed row still replays its delta idempotently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consuming_run: Option<String>,
}

/// Model key for one handoff row.
pub fn handoff_key(project: &str, pin: &str, bead: &str) -> (String, String, String) {
    (project.into(), pin.into(), bead.into())
}

/// Stamp one handoff row: coverage `from_run..=to_run`, delta strategy.
#[cfg_attr(not(test), allow(dead_code))]
pub fn record_handoff(
    project: &str,
    pin: &str,
    bead: &str,
    from_run: &str,
    to_run: &str,
    artifact: &str,
) -> HandoffRow {
    HandoffRow {
        project: project.into(),
        pin: pin.into(),
        bead: bead.into(),
        from_run: from_run.into(),
        to_run: to_run.into(),
        strategy: HandoffStrategy::DeltaSinceLastSeen,
        artifact: artifact.into(),
        consuming_run: None,
    }
}

/// The delta a resume sends: only runs the pane has NOT seen.
///
/// - No row → every run, FullBrief ("no handoff row").
/// - Stale row (`to_run` unknown, or `from_run` unknown while runs exist)
///   → every run, FullBrief ("stale handoff row"): coverage is unprovable,
///   so a delta would risk skipping context.
/// - Otherwise → runs after `from_run`, DeltaSinceLastSeen (possibly empty:
///   the pane is already current, so the resume sends nothing).
#[cfg_attr(not(test), allow(dead_code))]
pub fn handoff_delta<'a>(
    row: Option<&HandoffRow>,
    runs: &'a [HandoffRun],
) -> (Vec<&'a HandoffRun>, HandoffStrategy, &'static str) {
    let Some(row) = row else {
        return (
            runs.iter().collect(),
            HandoffStrategy::FullBrief,
            "no handoff row: full brief",
        );
    };
    let ids: std::collections::HashSet<&str> = runs.iter().map(|r| r.id.as_str()).collect();
    if !ids.contains(row.to_run.as_str())
        || (!ids.contains(row.from_run.as_str()) && !runs.is_empty())
    {
        return (
            runs.iter().collect(),
            HandoffStrategy::FullBrief,
            "stale handoff row: full brief",
        );
    }
    let after = match runs.iter().position(|r| r.id == row.from_run) {
        Some(i) => &runs[i + 1..],
        None => &[][..],
    };
    (
        after.iter().collect(),
        HandoffStrategy::DeltaSinceLastSeen,
        "delta since target last seen",
    )
}

/// One primitive for the respawn resume AND the session-pickup resume brief:
/// renders which strategy fired and exactly which runs travel. The delta
/// carries only unseen runs; the fallback names itself as a full brief.
#[cfg_attr(not(test), allow(dead_code))]
pub fn resume_brief(row: Option<&HandoffRow>, runs: &[HandoffRun]) -> (String, HandoffStrategy) {
    let (unseen, strategy, reason) = handoff_delta(row, runs);
    let ids: Vec<&str> = unseen.iter().map(|r| r.id.as_str()).collect();
    let text = match strategy {
        HandoffStrategy::DeltaSinceLastSeen => {
            if ids.is_empty() {
                format!(
                    "handoff delta: already current at {} — nothing to resend",
                    row.map(|r| r.to_run.as_str()).unwrap_or("?")
                )
            } else {
                format!(
                    "handoff delta since {}: {}",
                    row.map(|r| r.from_run.as_str()).unwrap_or("?"),
                    ids.join(",")
                )
            }
        }
        HandoffStrategy::FullBrief => {
            format!("full brief ({reason}): {}", ids.join(","))
        }
    };
    (text, strategy)
}

/// Read the stored row for (project, pin, bead) and return the resume delta.
/// Effect-layer entry point for respawn / session-pickup wiring.
#[cfg_attr(not(test), allow(dead_code))]
pub fn resume_runs_for<'a>(
    model: &'a WorkspaceModel,
    project: &str,
    pin: &str,
    bead: &str,
    runs: &'a [HandoffRun],
) -> (Vec<&'a HandoffRun>, HandoffStrategy, &'static str) {
    let row = model.handoffs.get(&handoff_key(project, pin, bead));
    handoff_delta(row, runs)
}

/// Presence verdict per pin; missing key = Unknown (invariant 9).
/// Governed-by: ADR-0010 D3. Busy/Idle/Dead carry the evidence source:
/// "presence" (20001 wire authority), "hc-evidence" (native busy,
/// never authority), "legacy" (pre-D3 snapshot), "endpoint" (death).
/// Unknown is never promoted, never absorbs a wake, never prompts.
/// Native hc idle is never trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub enum Presence {
    #[default]
    Unknown,
    Busy {
        source: String,
    },
    Idle {
        source: String,
    },
    Dead {
        source: String,
    },
}

impl Presence {
    /// Wire authority: 20001/jsonl state words. The word map itself lives
    /// in core (`parse_presence_word`, bd-z5ou); this only attaches the
    /// evidence source. blocked reads as Busy (occupied, needs decision
    /// via digest not prompt); done reads as Idle (available). Anything
    /// unparseable is Unknown, never idle.
    pub fn from_str(s: &str) -> Self {
        match doorbell_core::parse_presence_word(s) {
            doorbell_core::PresenceVerdict::Busy => Self::Busy {
                source: "presence".into(),
            },
            doorbell_core::PresenceVerdict::Blocked => Self::Busy {
                source: "presence-blocked: no typing target".into(),
            },
            doorbell_core::PresenceVerdict::Idle => Self::Idle {
                source: "presence".into(),
            },
            doorbell_core::PresenceVerdict::Dead => Self::Dead {
                source: "presence".into(),
            },
            doorbell_core::PresenceVerdict::Unknown => Self::Unknown,
        }
    }
    /// Status-render token: verdict plus source, so `flywheel status`
    /// prints verdict+source per pin (D3). Unknown renders bare.
    pub fn status_token(&self) -> String {
        match self {
            Self::Unknown => "Unknown".into(),
            Self::Busy { source } => format!("Busy({source})"),
            Self::Idle { source } => format!("Idle({source})"),
            Self::Dead { source } => format!("Dead({source})"),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Presence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(deserializer)?;
        if let Some(s) = v.as_str() {
            return Ok(match s {
                "Busy" | "Working" | "Blocked" => Self::Busy {
                    source: "legacy".into(),
                },
                "Idle" | "Done" => Self::Idle {
                    source: "legacy".into(),
                },
                "Dead" => Self::Dead {
                    source: "legacy".into(),
                },
                _ => Self::Unknown,
            });
        }
        let source_of = |key: &str| {
            v.get(key)
                .and_then(|inner| inner.get("source"))
                .and_then(|s| s.as_str())
                .unwrap_or("presence")
                .to_string()
        };
        if v.get("Busy").is_some() {
            return Ok(Self::Busy {
                source: source_of("Busy"),
            });
        }
        if v.get("Idle").is_some() {
            return Ok(Self::Idle {
                source: source_of("Idle"),
            });
        }
        if v.get("Dead").is_some() {
            return Ok(Self::Dead {
                source: source_of("Dead"),
            });
        }
        Ok(Self::Unknown)
    }
}

/// D3 verdict resolution: exact verdicts only. Known presence verdicts
/// pass through untouched. Unknown/missing presence never promotes:
/// native hc busy yields Busy labeled "hc-evidence" (evidence,
/// not authority — consumers still never prompt from it), native idle is
/// never trusted (stays Unknown), and only endpoint death overrides to
/// Dead. The CLI status render calls this with known=None (no live
/// Model in the render path); the tick path passes Model.presence.
pub fn verdict_for(known: Option<&Presence>, plane_status: &str, endpoint_alive: bool) -> Presence {
    if !endpoint_alive {
        return Presence::Dead {
            source: "endpoint".into(),
        };
    }
    match known {
        Some(Presence::Busy { source }) => Presence::Busy {
            source: source.clone(),
        },
        Some(Presence::Idle { source }) => Presence::Idle {
            source: source.clone(),
        },
        Some(Presence::Dead { source }) => Presence::Dead {
            source: source.clone(),
        },
        Some(Presence::Unknown) | None => match plane_status.trim().to_ascii_lowercase().as_str() {
            "working" | "busy" => Presence::Busy {
                source: "hc-evidence".into(),
            },
            "dead" => Presence::Dead {
                source: "endpoint".into(),
            },
            _ => Presence::Unknown,
        },
    }
}

/// Human decision question — at most one per corr (invariant 11).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Question {
    pub corr: String,
    pub body: String,
    #[serde(default)]
    pub created_at: u64,
}

/// One open captain call, folded from the open_human cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenDecision {
    pub corr: String,
    pub body: String,
    pub created_at: u64,
}

/// OPEN DECISIONS fold: every open_human entry, oldest first (corr
/// tiebreak — stable across folds). Never truncated: a buried decision
/// renders no matter how many routine rows surround it.
pub fn open_decisions(model: &WorkspaceModel) -> Vec<OpenDecision> {
    let mut rows: Vec<OpenDecision> = model
        .open_human
        .values()
        .map(|q| OpenDecision {
            corr: q.corr.clone(),
            body: q.body.clone(),
            created_at: q.created_at,
        })
        .collect();
    rows.sort_by(|a, b| (a.created_at, &a.corr).cmp(&(b.created_at, &b.corr)));
    rows
}

pub use crate::workspace_reporting::{Divergence, DivergenceSpec};
/// Single-home re-exports (bd-fuql): wake-state types + pure folds live in
/// `doorbell_core::wake_core`; the tick borrows them through these names
/// so external users (`main.rs` digest rendering) keep resolving.
pub use doorbell_core::wake_core::{
    DigestEntry, PresentedWake, ReringDecision, ReringEntry, digest_detail_truncate, render_digest,
};

/// Compatibility wrapper for the established workspace reporting seam. The
/// projection implementation lives in `workspace_reporting`; this function
/// keeps the source-level and call-level contract stable for snapshot readers.
pub fn record_divergence(spec: &DivergenceSpec<'_>) -> Vec<Divergence> {
    crate::workspace_reporting::record_divergence(spec)
}

pub fn render_divergence(rows: &[Divergence]) -> String {
    crate::workspace_reporting::render_divergence(rows)
}

/// One workspace model — honest composition, not a god object.
///
/// OWNERSHIP (GS-033 / ADR-0027 Option A): one live RAM owner.
/// `cmd_internal_doorbell` constructs the single `WorkspaceModel` for the
/// workspace lifetime and seeds `swarm` from ActiveLoop (same
/// `seed_swarm_resume` the loop uses) plus assignment from receipts.
/// `swarm_loop` must not construct a second `WorkspaceModel::default()` —
/// process split may remain (durable replay), but two
/// RAM WorkspaceModels is Dual, not the end-state. The loop holds only
/// `LoopModel` (effect runner + crash-resume slice) and folds swarm-slice
/// Msgs through `tick` via a throwaway composition (`WorkspaceModel {
/// swarm, ..Default::default() }` inside `tick_loop_owner` only), not a
/// live owner. That Default wrap is transitional throwaway fold — never
/// invent-green as "no Default".
///
/// Tripwires enforce Option A: the live loop body contains no
/// `WorkspaceModel::default()` and no Default-wrapped WorkspaceModel;
/// throwaway Default wrap is allowed only in `tick_loop_owner`. The
/// dispatcher seeds swarm resume. Two-process + one-RAM +
/// seed-from-receipts is a slice toward one Model. The end-state is one RAM owner.
/// `seq` is a per-process diagnostic counter, not shared truth.
/// bd-b2b3-serialize: the run-state checkpoint log stores this model, so it
/// must round trip. Every field is a map, set, integer, or string of owned data,
/// so a round trip is EXACT — the log can reconstruct the model byte-for-byte
/// rather than approximately.
///
/// This derive is a CAPABILITY, not a behaviour change. `tick` stays pure and
/// keeps no I/O: the snapshot is written by the INTERPRETER (RtHost), after
/// the effect layer has finished mutating the model, because
/// `handle_workspace_cmds` mutates it after the fold returns. Snapshotting at
/// the fold boundary would record an incomplete model that cannot resume.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkspaceModel {
    /// Host-resolved policy carried as model data so tick remains pure.
    pub policy: crate::workspace_policy::WorkspacePolicy,
    pub doorbell: crate::doorbell_tick::DoorbellModel,
    pub swarm: crate::durable::LoopModel,
    pub assignment: BTreeMap<String, Receipt>,
    pub roles: BTreeMap<String, String>,
    pub presence: BTreeMap<String, Presence>,
    pub open_human: BTreeMap<String, Question>,
    pub seq: u64,
    /// ONE wake home (bd-fuql): WaitCorr waits, ladder entries, shown
    /// wakes, and pending digests live in this table
    /// (`doorbell_core::wake_core`: `waits`/`ladders`/`shown`/`digests`) —
    /// D1/D2/D5 contracts unchanged, single persistence, single decision,
    /// single why.
    pub wake: doorbell_core::wake_core::WakeModel,
    /// ADR-0010 D2 (bd-8jlv): ack watermark per pin — highest handled
    /// presentation seq. Entries with seq <= watermark are handled fact.
    pub wake_acked: std::collections::BTreeMap<String, u64>,
    /// ADR-0010 D5 (bd-65he): per-class tick counts (journal+counter
    /// for routine; proves benign/routine classes spend zero prompts).
    pub wake_counters: std::collections::BTreeMap<String, u64>,
    /// bd-context-handoff-064d: auditable handoff rows keyed
    /// (project, pin, bead). Pure model data; the effect layer (respawn /
    /// session-pickup) reads them via `resume_runs_for`.
    pub handoffs: BTreeMap<(String, String, String), HandoffRow>,
    /// ADR-0010 D5 (bd-65he): last wall-clock seen on a now-bearing Msg.
    pub last_now: u64,
    /// ADR-0010 D5 (bd-65he): wall-clock of the last emitted digest.
    pub last_digest_unix: u64,
    /// GS-048: last committed session topology (sticky-open source of truth).
    /// `None` until a DecisionReady lands; open stays open unless
    /// loop/swarm/join/none is explicit on the MaterialContext Msg.
    pub session_topology: Option<String>,
    /// GS-048: last announced vehicle line (`vehicle: …` or `none: raw`).
    pub last_vehicle: Option<String>,
}

impl WorkspaceModel {
    /// Construct the live model at the host boundary. After this point policy
    /// is ordinary model data and reducers never read process environment.
    pub fn from_environment() -> Self {
        Self {
            policy: crate::workspace_policy::WorkspacePolicy::from_environment(),
            ..Self::default()
        }
    }
}

/// Default digest batching interval (secs). Override with
/// FLYWHEEL_DIGEST_SECS.
pub const DIGEST_INTERVAL_DEFAULT_SECS: u64 = 300;
/// Default digest queue depth that forces a batch. Override with
/// FLYWHEEL_DIGEST_THRESHOLD.
pub const DIGEST_THRESHOLD_DEFAULT: usize = 10;

/// Deterministic default digest interval. Host overrides live in
/// `WorkspacePolicy::from_environment`, never in reducer helpers.
#[allow(dead_code)]
pub const fn digest_interval_secs() -> u64 {
    DIGEST_INTERVAL_DEFAULT_SECS
}

/// Deterministic default digest threshold. Host overrides live in
/// `WorkspacePolicy::from_environment`, never in reducer helpers.
#[allow(dead_code)]
pub const fn digest_threshold() -> usize {
    DIGEST_THRESHOLD_DEFAULT
}

/// Wake severity, lowest to highest. Tick class is the max over its cmds;
/// a tick with no cmds is Benign.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WakeClass {
    Benign,
    Routine,
    CaptainRelevant,
    Urgent,
}

impl WakeClass {
    pub fn label(self) -> &'static str {
        match self {
            WakeClass::Benign => "benign",
            WakeClass::Routine => "routine",
            WakeClass::CaptainRelevant => "captain",
            WakeClass::Urgent => "urgent",
        }
    }
}

/// Single definition lives in `doorbell-core` (bd-ene2) — the daemon
/// FileHost presence filter shares it; this re-export keeps call sites.
pub use doorbell_core::{is_act_now_line, is_urgent_line};

/// Edge-triage predicate (bd-l72k): a machine-rendered addressed kind-9
/// wake (`doorbell::doorbell_line(Channel, _)` head). Routine chatter:
/// advances markers with zero prompts, never enters the digest. Urgent
/// lines never carry this head, but callers must test urgency first.
/// Unknown/unmarked prompts default CaptainRelevant (digest-visible),
/// never silently routine.
pub fn is_routine_wake_line(line: &str) -> bool {
    line.contains("team room")
}

/// Classify one emitted cmd for wake spend.
///
/// Edge triage (bd-l72k): addressed kind-9 chatter is Routine — it is
/// stripped before delivery (zero prompts, counters only), so a Routine
/// tick may textually contain a pre-strip Prompt. Captain-relevant
/// prompts are stripped the same way but keep their digest entry.
/// Only urgent prompts deliver.
pub fn classify_cmd(cmd: &Cmd) -> WakeClass {
    match cmd {
        Cmd::Prompt { line, .. } if is_urgent_line(line) => WakeClass::Urgent,
        Cmd::Prompt { line, .. } if is_routine_wake_line(line) => WakeClass::Routine,
        Cmd::Prompt { .. }
        | Cmd::Audit { .. }
        | Cmd::FactMiss { .. }
        | Cmd::LoudMiss { .. }
        | Cmd::Respawn { .. }
        | Cmd::Diagnose { .. } => WakeClass::CaptainRelevant,
        Cmd::Wait => WakeClass::Benign,
        Cmd::Digest { .. } => WakeClass::CaptainRelevant,
        _ => WakeClass::Routine,
    }
}

/// Wedge alarm: a non-empty queue older than two batch intervals means
/// the digest path cannot deliver — fail loud, never silently batch.
pub fn digest_overdue(queue_len: usize, last_digest_unix: u64, now: u64) -> bool {
    digest_overdue_with_policy(
        queue_len,
        last_digest_unix,
        now,
        &crate::workspace_policy::WorkspacePolicy::default(),
    )
}

pub fn digest_overdue_with_policy(
    queue_len: usize,
    last_digest_unix: u64,
    now: u64,
    policy: &crate::workspace_policy::WorkspacePolicy,
) -> bool {
    queue_len > 0 && now.saturating_sub(last_digest_unix) > 2 * policy.digest_interval_secs
}

/// bd-2fxx: deadline-attached holds. The wait value IS the
/// needed-by unix secs (stamped now+TTL at the WaitCorr effect site,
/// decided in the Sweep arm). No new store: these folds read it.
pub fn sorted_holds(model: &WorkspaceModel) -> Vec<(String, u64)> {
    let mut holds: Vec<(String, u64)> = model
        .wake
        .waits
        .iter()
        .map(|(c, d)| (c.clone(), *d))
        .collect();
    holds.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
    holds
}

/// bd-2fxx: digest render with the hold section appended, earliest
/// needed-by first; overdue rows carry ESCALATE. Pure — both the afk
/// CLI and the Digest effect share it. render_digest delegates with
/// no holds so entries-only callers stay unbroken.
pub fn render_digest_with_holds(
    entries: &[DigestEntry],
    holds: &[(String, u64)],
    now: u64,
) -> String {
    let mut out = render_digest(entries, now);
    if holds.is_empty() {
        out.push_str("holds: none\n");
        return out;
    }
    let _ = writeln!(out, "holds ({} earliest-deadline first):", holds.len());
    for (corr, needed_by) in holds {
        if *needed_by <= now {
            let overdue = now.saturating_sub(*needed_by);
            let _ = writeln!(
                out,
                "- ESCALATE {corr} needed-by {needed_by} ({overdue}s overdue)"
            );
        } else {
            let remaining = needed_by.saturating_sub(now);
            let _ = writeln!(out, "- {corr} needed-by {needed_by} ({remaining}s left)");
        }
    }
    out
}

/// bd-zfnx.6: a parked corr lives at most this long before the Sweep arm
/// times it out (deadline path removes + Audit wait-timeout record).
pub const PENDING_WAIT_TTL_SECS: u64 = 3600;
/// bd-zfnx.6: hard cap on parked corrs. Over-cap inserts evict the
/// earliest deadline first, each with an Audit wait-timeout record.
pub const PENDING_WAIT_CAP: usize = 512;

/// Default grace window between re-rings (secs). Override with
/// FLYWHEEL_DOORBELL_GRACE_SECS.
pub const DOORBELL_GRACE_DEFAULT_SECS: u64 = 90;
/// Default max rings before escalation. Override with
/// FLYWHEEL_DOORBELL_RING_MAX.
pub const DOORBELL_RING_MAX_DEFAULT: u32 = 3;

/// Deterministic default grace window. Host overrides live in
/// `WorkspacePolicy::from_environment`, never in reducer helpers.
#[allow(dead_code)]
pub const fn doorbell_grace_secs() -> u64 {
    DOORBELL_GRACE_DEFAULT_SECS
}

/// Deterministic default ring cap. Host overrides live in
/// `WorkspacePolicy::from_environment`, never in reducer helpers.
#[allow(dead_code)]
pub const fn doorbell_ring_max() -> u32 {
    DOORBELL_RING_MAX_DEFAULT
}

/// D1 shrunk prompt: the payload rides the toron message, hc gets
/// the doorbell only.
pub fn doorbell_mail_line(unacked: usize, pin: &str, _corr: &str) -> String {
    // bd-ofa1s: name who it is for and the exact command; the correlation id
    // stays typed data, never buried in prose (bd-tdpix).
    format!(
        "{} Urgent mail: {unacked} unacked message(s) for {pin} in project flywheel. Read and reply with: toron next --project flywheel --as {pin}",
        doorbell_core::agent_text::WAKE_PREFIX
    )
}

/// ADR-0010 D1 (bd-w98c residual): pure ladder-registration predicate over
/// one inbox row. Scope is deliberately toron's own SLA scan scope
/// (`check_ack_sla_expired`): ack required + high/urgent importance. The
/// read-state check (``read_ts_us`` null) is the caller's job — the SlaFired
/// hit list only reaches rows that are still unacked, and the InboxRows arm
/// folds the read fact separately, so this predicate stays the overdue-shape
/// test. `id`-less rows (no toron message id) are never tracked: the ladder
/// keys by corr, and an id-less row cannot be honestly deduped.
fn inbox_row_ack_overdue(row: &serde_json::Value) -> bool {
    if row.get("ack_required").and_then(serde_json::Value::as_bool) != Some(true) {
        return false;
    }
    if row
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .is_empty()
    {
        return false;
    }
    matches!(
        row.get("importance").and_then(serde_json::Value::as_str),
        Some("high" | "urgent")
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Msg {
    /// Untrusted wire bytes. Per-kind folds (20001 presence with the E5
    /// trust rule, 30013 roles, corr-tagged 14 assignment); kind-9 is the
    /// doorbell-slice ring. Never panics on unparseable input.
    WatchNdjson {
        line: String,
    },
    /// Opaque fetch blob for one pin. Forwarded to the doorbell slice only;
    /// assignment enters only after re-entering as InboxRows.
    InboxItems {
        raw: String,
        pin: String,
    },
    /// Trusted projection rows. Each row needs a non-empty corr (empty corr
    /// is standing-empty silence); vocab validated in apply_assignment.
    InboxRows {
        rows: Vec<serde_json::Value>,
        pin: String,
    },
    /// Sole writer of doorbell.statuses. Working pins queue; the
    /// working→idle transition drains the queue into Prompt.
    PlaneStatus {
        pin: String,
        status: String,
    },
    /// bd-9mjla: the pane refused a Prompt as busy. The doorbell slice keeps
    /// the wake (one per pin) and rings it once at the next at-prompt status;
    /// the interpreter only reports the refusal, it never retries.
    PromptRefused {
        pin: String,
        line: String,
        event_id: Option<String>,
    },
    /// A mail hint from the toron events source (U8). The bus event id is
    /// the shared dedupe key across wake.sock and events.
    EventsHint {
        event_id: String,
        pin: String,
        line: String,
    },
    /// The events source lost track: one digest wake, never a prompt storm.
    EventsGap {
        pin: String,
    },
    /// Captain/operator injection. Prompts the captain-role pin iff
    /// prompt_allowed, else falls through to the doorbell slice.
    Fact {
        summary: String,
    },
    /// Clock payload for the ack-SLA slice. Carries time, never reads it.
    AckSlaTick {
        now: u64,
    },
    /// ADR-0010 D1 (bd-w98c residual): the ack-SLA scan's CHANGE hit list
    /// (overdue corr ids, bd-adms delta fire) re-enters as a Msg. Tick emits
    /// one captain-relevant audit + FetchInbox — rows re-enter as InboxRows,
    /// where ack-overdue corr rows register on the re-ring ladder (the
    /// "registration rides the send path" residual closes here: a corr seen
    /// overdue on the wire is tracked even without a same-process send).
    /// Writes: doorbell slice (via the doorbell arm), audit trace. Emits:
    /// Audit + FetchInbox. Never a direct Prompt (the ladder owns re-rings).
    SlaFired {
        posts: Vec<String>,
    },
    /// Transport death. Slice restarts the watch; model queues survive.
    WatchDied,
    /// Delegates to the swarm slice; guarantees a Cmd::Reclaim when the
    /// slice emits none (reason defaults to "reclaim").
    Reclaim {
        bead_id: String,
        holder_pin: String,
        claimant_pin: String,
        is_live: bool,
        #[serde(default)]
        reason: String,
    },
    Heal {
        name: String,
        status: String,
        exited: bool,
    },
    ClaimNudge {
        name: String,
        bead_id: String,
    },
    CloseNudge {
        name: String,
        bead_id: String,
        title: String,
    },
    /// Clock for plane-P fetch: effect runs FetchInbox; rows re-enter as InboxRows.
    /// Emits exactly one FetchInbox signal; never performs the fetch itself.
    /// Carries `now` so the same beat expires bounded waits (bd-zfnx.6).
    Sweep {
        now: u64,
    },
    /// A toron pull-cursor read re-enters here (pipe pattern) and folds
    /// straight into the doorbell slice (bd-yc6jf). Decides parked mail by
    /// max(host cursor, toron cursor); unanswered toron rings anyway.
    ToronCursor {
        pin: String,
        cursor_us: Option<i64>,
        read_at_secs: u64,
    },
    /// Write-through of one durable receipt: inserts the assignment cache
    /// AND emits SqliteUpsert for the same corr (never one without the other).
    Receipt {
        corr: String,
        receipt: Receipt,
    },
    /// Upserts the roles map (role → pin). Last writer wins; no Cmd emitted.
    RoleBound {
        role: String,
        pin: String,
    },
    /// Trusted internal presence edge (dispatcher jsonl path). Inserts
    /// Presence::from_str(state); unknown strings become Unknown. See E5.
    Presence {
        pin: String,
        state: String,
    },
    /// Closes at most one open question per corr. Unknown or duplicate corr
    /// emits LoudMiss; exactly one Audit per close.
    HumanDecision {
        corr: String,
        verdict: String,
    },
    /// Clock discipline: wall-clock enters tick ONLY as this payload (E1).
    /// Drives the swarm slice; deterministic in (model, now).
    Tick {
        now: u64,
    },
    /// Diagnose I/O failed: revert optimistic awaiting_clear inside tick.
    /// Clears the flag, emits nothing.
    DiagnoseFailed {
        name: String,
    },
    /// Hc pane event. Enters as Msg (not a world-dump into pending_events).
    /// Delegates to the swarm slice.
    Hc {
        event: crate::durable::PollEvent,
    },
    /// GS-048 / GS-047 §E.1: material context change (AUTHORITATIVE objective,
    /// explicit topology ask, new work-unit class, classifier miss, stage-chain
    /// exhaustion). Tick emits caller-applied `Cmd::Reclassify` — never runs
    /// decide I/O inside the pure reducer. Routine Channel/Job wakes stay
    /// WatchNdjson → Prompt-only and do NOT use this arm.
    MaterialContext {
        goal: String,
        /// Operator-explicit topology (`open`/`loop`/`swarm`/`join`). `None`
        /// preserves sticky-open (do not flip open→loop/swarm/join/none).
        explicit_topology: Option<String>,
        /// Why this is material (audit + announce).
        reason: String,
    },
    /// GS-048: effect-layer result of `Cmd::Reclassify` (decide/briefing ran
    /// outside tick). Re-enters the Elm plane; tick applies sticky-open and
    /// emits the vehicle announcement Prompt.
    DecisionReady {
        goal: String,
        primitive: String,
        topology: String,
        argv: Vec<String>,
        decide_reason: String,
        /// Passthrough from MaterialContext (sticky predicate).
        explicit_topology: Option<String>,
    },
    /// bd-context-handoff-064d: records one auditable handoff row keyed
    /// (project, pin, bead). Last writer wins; emits nothing.
    HandoffRecorded {
        row: HandoffRow,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cmd {
    /// Inject iff prompt_allowed (E5 gate). Line always starts "[doorbell]".
    Prompt {
        pin: String,
        line: String,
        event_id: Option<String>,
    },
    /// Effect runs the fetch; rows re-enter tick as InboxRows.
    /// Signal only (bd-z5ou): per-pin cursors live in the brain.
    FetchInbox,
    /// Effect runs `toron inbox cursor`; the answer re-enters tick as
    /// ToronCursor (bd-yc6jf). Signal only, like FetchInbox.
    ReadToronCursor { pin: String },
    /// Caller-applied dispatcher effects (no tick-side content).
    CheckAckSla,
    /// Caller-applied dispatcher effects (no tick-side content).
    RestartWatch,
    /// Structured route-miss: why carries "route-miss kind=<k> tags=[...]".
    /// Journal-observable; the miss rate is the audit signal.
    FactMiss { why: String },
    /// Observation only (kind/pin/detail). Never gates dispatch.
    Audit {
        kind: String,
        pin: String,
        detail: String,
    },
    /// ADR-0010 D5 (bd-65he): afk digest batch — drained captain-relevant
    /// entries for the effect layer to deliver (steer channel post).
    /// Rendered with render_digest; never holds prompt content beyond
    /// the bounded DigestEntry details.
    Digest { entries: Vec<DigestEntry> },

    /// Swarm send. Name is the hc pane, text the full prompt.
    Send { name: String, text: String },
    /// Swarm send. Records bead→pin binding for the spawn registry.
    PinStamp { bead_id: String, pin: String },
    /// Swarm send. Immediate dispatch bypassing the queue.
    DispatchImmediate { name: String, text: String },
    /// Swarm send. Marks the pane blocked (await-signal, not dead).
    Blocked { name: String },
    /// Swarm send. Claim/close nudge for one bead.
    Nudge { name: String },
    /// Swarm send. Diagnose with reason; may precede Respawn.
    Diagnose { name: String, reason: String },
    /// Swarm send. Respawn with reason; budgeted by the executor.
    Respawn { name: String, reason: String },
    /// Swarm send. Reclaim carries bead + pin + terminal name + reason.
    Reclaim {
        bead_id: String,
        pin: String,
        terminal_name: String,
        reason: String,
    },
    /// Swarm send. Cooperative yield; no content.
    Wait,

    /// Write-through of the assignment cache. Corr is always non-empty;
    /// receipt mirrors the Model row exactly.
    SqliteUpsert { corr: String, receipt: Receipt },
    /// Rumor-14 shape: kind is always 14, content non-empty, corr is the
    /// assignment key the answer returns (E4: no uncorrelated Publish),
    /// result_path rides where known (executor tags both onto the rumor).
    Publish {
        kind: u64,
        content: String,
        corr: String,
        result_path: Option<String>,
    },
    /// Registers interest in one receipt. Corr is always non-empty
    /// (empty corr is standing-empty silence, never a Wait). Bounding
    /// (TTL + cap) is the executor's job (.6); tick registers, never drops.
    WaitCorr { corr: String },
    /// Playbook invocation vehicle (zfnx.5 rewired per GS-016): effect-layer
    /// door to `engine.invoke` + `VerifySink` through the durable Run
    /// machinery. The gate is inside `cmd_workflow` — playbook lint first,
    /// unknown workflow fails loud naming it. Caller-applied (like Prompt):
    /// tick never emits Invoke; the composing caller does. `workflow` is a
    /// built-in or catalog name; `input` must be a JSON object (`{{inputs.x}}`);
    /// `corr` is always non-empty and attributes the run.
    Invoke {
        workflow: String,
        input: serde_json::Value,
        corr: String,
    },
    /// Tick's hard-error signal. Why is always non-empty and names the
    /// contract broken (forge, malformed tags, unknown corr).
    LoudMiss { why: String },

    /// GS-043: journaled in-flight profile/model rebind (ADR-0032 Spec 7).
    /// Pin/npub immutable; profile_name/model replaceable. Not a mailbox
    /// receipt — effect layer journals via agent_ref before applying.
    RebindBinding {
        pin: String,
        profile_name: Option<String>,
        model: Option<String>,
        harness: Option<String>,
        effect_key: String,
        note: String,
    },
    /// GS-048 / GS-050: caller-applied mid-session reclassify. Effect runs
    /// pure `decide` (+ briefing assemble when a World is available) and
    /// re-enters as `Msg::DecisionReady`. Tick never performs I/O here.
    /// `effect_key` is the stable journal identity for one logical
    /// reclassify (ADR-0032 I8a) — retries keep the same key.
    Reclassify {
        goal: String,
        pin: String,
        explicit_topology: Option<String>,
        reason: String,
        effect_key: String,
    },
}

impl Cmd {
    /// bd-c9jw: `pub(crate)` so the `why` journal replay uses this ONE
    /// mapping instead of re-stating it — the duplicate drifted the moment
    /// a new action variant landed (bd-bk9t's `Digest`).
    pub(crate) fn from_doorbell(a: crate::doorbell_tick::DoorbellAction) -> Self {
        match a {
            crate::doorbell_tick::DoorbellAction::Prompt {
                pin,
                line,
                event_id,
            } => Self::Prompt {
                pin,
                line,
                event_id,
            },
            crate::doorbell_tick::DoorbellAction::FetchInbox => Self::FetchInbox,
            crate::doorbell_tick::DoorbellAction::ReadToronCursor { pin } => {
                Self::ReadToronCursor { pin }
            }
            crate::doorbell_tick::DoorbellAction::CheckAckSla => Self::CheckAckSla,
            crate::doorbell_tick::DoorbellAction::RestartWatch => Self::RestartWatch,
            crate::doorbell_tick::DoorbellAction::FactMiss { why } => Self::FactMiss { why },
            crate::doorbell_tick::DoorbellAction::Audit { kind, pin, detail } => {
                Self::Audit { kind, pin, detail }
            }
            crate::doorbell_tick::DoorbellAction::Digest { pin, line, why } => Self::Audit {
                kind: "digest_bound".into(),
                pin,
                detail: format!("{why}: {line}"),
            },
        }
    }

    fn from_loop(a: crate::durable::LoopAction) -> Self {
        match a {
            crate::durable::LoopAction::Send { name, text } => Self::Send { name, text },
            crate::durable::LoopAction::PinStamp { bead_id, pin } => {
                Self::PinStamp { bead_id, pin }
            }
            crate::durable::LoopAction::DispatchImmediate { name, text } => {
                Self::DispatchImmediate { name, text }
            }
            crate::durable::LoopAction::Blocked { name } => Self::Blocked { name },
            crate::durable::LoopAction::Nudge { name } => Self::Nudge { name },
            crate::durable::LoopAction::Diagnose { name, reason } => {
                Self::Diagnose { name, reason }
            }
            crate::durable::LoopAction::Respawn { name, reason } => Self::Respawn { name, reason },
            crate::durable::LoopAction::Reclaim {
                bead_id,
                pin,
                terminal_name,
                reason,
            } => Self::Reclaim {
                bead_id,
                pin,
                terminal_name,
                reason,
            },
            crate::durable::LoopAction::PausedResurface {
                name,
                bead_id,
                owner,
                age_secs,
            } => {
                let detail =
                    crate::durable::pause_resurface_line(&name, &bead_id, &owner, age_secs);
                Self::Audit {
                    kind: "pause_resurface".into(),
                    pin: owner,
                    detail,
                }
            }
            crate::durable::LoopAction::Wait => Self::Wait,
        }
    }

    /// Swarm executor still drains LoopAction; doorbell cmds stay None.
    pub fn to_loop_action(&self) -> Option<crate::durable::LoopAction> {
        match self {
            Self::Send { name, text } => Some(crate::durable::LoopAction::Send {
                name: name.clone(),
                text: text.clone(),
            }),
            Self::PinStamp { bead_id, pin } => Some(crate::durable::LoopAction::PinStamp {
                bead_id: bead_id.clone(),
                pin: pin.clone(),
            }),
            Self::DispatchImmediate { name, text } => {
                Some(crate::durable::LoopAction::DispatchImmediate {
                    name: name.clone(),
                    text: text.clone(),
                })
            }
            Self::Blocked { name } => {
                Some(crate::durable::LoopAction::Blocked { name: name.clone() })
            }
            Self::Nudge { name } => Some(crate::durable::LoopAction::Nudge { name: name.clone() }),
            Self::Diagnose { name, reason } => Some(crate::durable::LoopAction::Diagnose {
                name: name.clone(),
                reason: reason.clone(),
            }),
            Self::Respawn { name, reason } => Some(crate::durable::LoopAction::Respawn {
                name: name.clone(),
                reason: reason.clone(),
            }),
            Self::Reclaim {
                bead_id,
                pin,
                terminal_name,
                reason,
            } => Some(crate::durable::LoopAction::Reclaim {
                bead_id: bead_id.clone(),
                pin: pin.clone(),
                terminal_name: terminal_name.clone(),
                reason: reason.clone(),
            }),
            Self::Wait => Some(crate::durable::LoopAction::Wait),
            _ => None,
        }
    }
}

/// ADR-0028 E5 inject gate as amended by ADR-0010 D3: only an exact
/// Idle verdict prompts. Busy never prompts; Dead is Lane-1 drain
/// territory; Unknown (missing key or unparseable state) never prompts
/// and never absorbs a wake — it surfaces for inspection via status
/// (Unknown→digest is the re-ring ladder's read, not this gate's).
/// DEVIATION (bd-s8bo): pre-D3, blocked done idle all prompted via
/// should_inject; now blocked parses to Busy and does not prompt — a
/// blocked pin needs a human decision via digest, not a wake.
fn prompt_allowed(model: &WorkspaceModel, pin: &str) -> bool {
    match model.presence.get(pin) {
        Some(Presence::Idle { .. }) => true,
        Some(Presence::Busy { .. })
        | Some(Presence::Dead { .. })
        | Some(Presence::Unknown)
        | None => false,
    }
}

/// GS-048 sticky-open: preserve open unless loop/swarm/join/none is
/// **explicit** on the MaterialContext. Catalog miss (`none`) must not wipe
/// `session_topology` open→none invent-flip. Also hold loop against swarm
/// without an explicit request (matches loop.md.tpl speech).
pub fn sticky_open_topology(
    current: Option<&str>,
    classified: &str,
    explicit: Option<&str>,
) -> (String, bool) {
    if let Some(e) = explicit {
        let e = e.trim().to_ascii_lowercase();
        if !e.is_empty() {
            return (e, false);
        }
    }
    if current == Some("open") && matches!(classified, "loop" | "swarm" | "join" | "none") {
        return ("open".into(), true);
    }
    if current == Some("loop") && classified == "swarm" {
        return ("loop".into(), true);
    }
    (classified.to_string(), false)
}

/// GS-048: parse an operator-explicit topology request from free text.
/// Returns `Some` only when the human named open/loop/swarm/join (slash-mode
/// or explicit words) — never invent from classifier output.
pub fn parse_explicit_topology(text: &str) -> Option<String> {
    let t = text.to_ascii_lowercase();
    if t.contains("/swarm-mode") || t.contains("topology=swarm") || t.contains("topology swarm") {
        return Some("swarm".into());
    }
    if t.contains("/loop-mode") || t.contains("topology=loop") || t.contains("topology loop") {
        return Some("loop".into());
    }
    if t.contains("/open-mode") || t.contains("topology=open") || t.contains("topology open") {
        return Some("open".into());
    }
    if t.contains("/join") || t.contains("topology=join") || t.contains("topology join") {
        return Some("join".into());
    }
    for topo in ["swarm", "loop", "open", "join"] {
        let needles = [
            format!("switch to {topo}"),
            format!("use {topo} topology"),
            format!("explicit {topo}"),
            format!("want {topo}"),
        ];
        if needles.iter().any(|n| t.contains(n)) {
            return Some(topo.into());
        }
    }
    None
}

/// GS-048: classify whether a Fact / steer / operator summary is **material**
/// (must enter `Msg::MaterialContext`) vs routine Prompt-only wake.
/// Returns `Some(reason)` when material.
pub fn material_context_reason(summary: &str) -> Option<&'static str> {
    let s = summary;
    let lower = summary.to_ascii_lowercase();
    if s.contains("AUTHORITATIVE") || lower.contains("authoritative operator objective") {
        return Some("authoritative-objective");
    }
    if parse_explicit_topology(summary).is_some() {
        return Some("explicit-topology-request");
    }
    if lower.contains("stage-chain") && (lower.contains("exhaust") || lower.contains("complete")) {
        return Some("stage-chain-exhaustion");
    }
    if lower.contains("classifier miss") || lower.contains("catalog miss") {
        return Some("classifier-miss");
    }
    if lower.contains("new work unit")
        || lower.contains("new work-unit")
        || lower.contains("new bead")
        || lower.contains("new wave")
    {
        return Some("new-work-unit");
    }
    if lower.contains("reclassify") || lower.contains("material context") {
        return Some("material-reclassify");
    }
    if lower.contains("new objective")
        || lower.contains("objective change")
        || lower.contains("changed objective")
        || lower.starts_with("goal:")
        || lower.starts_with("objective:")
    {
        return Some("operator-goal-change");
    }
    None
}

/// Build a MaterialContext Msg from a material summary (Fact / steer / CLI).
pub fn material_context_msg(goal: &str, reason: &str) -> Msg {
    Msg::MaterialContext {
        goal: goal.to_string(),
        explicit_topology: parse_explicit_topology(goal),
        reason: reason.to_string(),
    }
}

/// GS-048 vehicle announcement line body (without the `[reclassify]` head).
pub fn vehicle_announcement(primitive: &str, topology: &str) -> String {
    if primitive.is_empty() || primitive == "none" {
        "none: raw".into()
    } else {
        format!("vehicle: {primitive} (topology={topology})")
    }
}

/// GS-049 sticky-open airtight: when sticky-held, do **not** name the invent
/// swarm/join/compose primitive under the held topology. Open → `none: raw`;
/// loop held against invent swarm → loop-work vehicle only.
pub fn sticky_held_vehicle_announcement(invent_primitive: &str, held_topology: &str) -> String {
    match held_topology {
        "open" => "none: raw".into(),
        "loop" => {
            let invent = invent_primitive.trim();
            if invent == "wave-swarm"
                || invent == "join"
                || invent == "repair-until-verified"
                || invent == "beads-lifecycle"
            {
                vehicle_announcement("loop-work", "loop")
            } else {
                vehicle_announcement(invent_primitive, held_topology)
            }
        }
        _ => vehicle_announcement(invent_primitive, held_topology),
    }
}

/// Full Prompt line for a DecisionReady announce — urgent by `[reclassify]` head.
pub fn reclassify_announce_line(vehicle: &str, decide_reason: &str, sticky_held: bool) -> String {
    let sticky = if sticky_held { " sticky-open held" } else { "" };
    format!("[reclassify] {vehicle} — {decide_reason}{sticky}")
}

/// GS-050 / ADR-0032 I8a: stable journal identity for one logical
/// reclassify. Excludes attempt number — same goal/reason/explicit/pin
/// always yields the same key (retries keep it). Length-prefixed / tagged
/// encode via [`crate::agent_ref::ReclassifyIntent::derive_effect_key`] so
/// field boundaries cannot collide and `None` ≠ `Some("")`.
pub fn reclassify_effect_key(
    goal: &str,
    reason: &str,
    explicit_topology: Option<&str>,
    pin: &str,
) -> String {
    crate::agent_ref::ReclassifyIntent::derive_effect_key(goal, reason, explicit_topology, pin)
}

/// GS-048 / GS-050 effect helper: replace each `Cmd::Reclassify` with the
/// cmds from folding `Msg::DecisionReady` after a pure `decide` call.
/// The classify leaf's answer: `(topology, src, argv, reason)`.
type ReclassifyDecision = (String, String, Vec<String>, String);

/// Journals Prepared → InFlight → Settled|Failed under the Cmd's
/// `effect_key` (ADR-0032 I8a) via insert-or-load — retries attach to the
/// same journal primary key. Callers in the effect/host path run this
/// **before** mapping Prompt→delivery; decide stays outside pure `tick`.
pub fn expand_reclassify_cmds(
    model: &mut WorkspaceModel,
    cmds: Vec<Cmd>,
    project: &str,
    authority: &crate::authority::RuntimeAuthority,
    decide: &dyn Fn(&str) -> ReclassifyDecision,
) -> Vec<Cmd> {
    let mut out = Vec::with_capacity(cmds.len());
    for cmd in cmds {
        match cmd {
            Cmd::Reclassify {
                goal,
                pin,
                explicit_topology,
                reason,
                effect_key,
            } => {
                let expected =
                    reclassify_effect_key(&goal, &reason, explicit_topology.as_deref(), &pin);
                if effect_key != expected {
                    out.push(Cmd::LoudMiss {
                        why: format!(
                            "reclassify effect_key drift: supplied={effect_key} derived={expected}"
                        ),
                    });
                    continue;
                }
                let prepared = crate::agent_ref::ReclassifyIntent::from_cmd_key(
                    effect_key.clone(),
                    goal.clone(),
                    reason.clone(),
                    explicit_topology.clone(),
                    pin.clone(),
                    0,
                );
                let intent = match crate::agent_ref::insert_or_load_reclassify(project, prepared) {
                    Ok(i) => i,
                    Err(e) => {
                        out.push(Cmd::LoudMiss {
                            why: format!(
                                "reclassify journal insert failed; refusing provider effect: {e:#}"
                            ),
                        });
                        continue;
                    }
                };
                let (primitive, topology, argv, decide_reason) = decide(&intent.goal);
                let plan = crate::judgment::prepare_post_with_authority(
                    crate::judgment::ExpandArgs {
                        project: project.to_string(),
                        goal: intent.goal.clone(),
                        pin: pin.clone(),
                        reason: reason.clone(),
                        explicit_topology: intent.explicit_topology.clone(),
                        parent_effect_key: intent.effect_key.clone(),
                        sticky_topology: model.session_topology.clone(),
                        table_primitive: primitive,
                        table_topology: topology,
                        table_argv: argv,
                        table_reason: decide_reason,
                    },
                    authority,
                );
                let posted = match &plan.stage {
                    crate::judgment::PostStage::Ready {
                        slice,
                        pack,
                        transport,
                        screen: _,
                    } => {
                        let adapter = crate::judgment::JudgmentAdapter {
                            pack: pack.clone(),
                            transport: transport.clone(),
                        };
                        Some(crate::judgment::run_post(
                            adapter,
                            slice,
                            crate::judgment::Surface::Honesty,
                        ))
                    }
                    _ => None,
                };
                let screen_posted = match &plan.stage {
                    crate::judgment::PostStage::Ready {
                        screen: Some(ask), ..
                    } => {
                        let adapter = crate::judgment::JudgmentAdapter {
                            pack: ask.pack.clone(),
                            transport: match &plan.stage {
                                crate::judgment::PostStage::Ready { transport, .. } => {
                                    transport.clone()
                                }
                                _ => unreachable!("screen implies Ready"),
                            },
                        };
                        Some(crate::judgment::run_post(
                            adapter,
                            &ask.slice,
                            crate::judgment::Surface::Honesty,
                        ))
                    }
                    _ => None,
                };
                let enriched = crate::judgment::finish_plan(plan, posted, screen_posted);
                let ready = Msg::DecisionReady {
                    goal: intent.goal.clone(),
                    primitive: enriched.primitive,
                    topology: enriched.topology,
                    argv: enriched.argv,
                    decide_reason: enriched.reason,
                    explicit_topology: intent.explicit_topology.clone(),
                };
                let more = match crate::agent_ref::commit_reclassify(project, intent, |intent| {
                    let _ = &intent.effect_key;
                    let (next, mut more) = tick(model, &ready);
                    *model = next;
                    if let Some((audit_pin, detail)) = enriched.audit {
                        more.push(Cmd::Audit {
                            kind: "judgment".to_string(),
                            pin: audit_pin,
                            detail: detail.to_string(),
                        });
                    }
                    if let Some((audit_pin, detail)) = enriched.screen_audit {
                        more.push(Cmd::Audit {
                            kind: "judgment".to_string(),
                            pin: audit_pin,
                            detail: detail.to_string(),
                        });
                    }
                    if let Err(e) = crate::judgment::miss_append(&crate::judgment::MissRow {
                        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                        effect_key: intent.effect_key.clone(),
                        goal: crate::judgment::cap_str(
                            &intent.goal,
                            crate::judgment::MISS_GOAL_CAP,
                        ),
                        table_primitive: enriched.table_primitive.clone(),
                        table_topology: enriched.table_topology.clone(),
                        escalated: enriched.escalated,
                        screen: enriched.screen.clone(),
                        resolution: None,
                        resolved_ts: None,
                        note: None,
                    }) {
                        eprintln!("flywheel: miss ledger append failed: {e}");
                    }
                    Ok(more)
                }) {
                    Ok(more) => more,
                    Err(e) => {
                        vec![Cmd::LoudMiss {
                            why: format!("reclassify commit failed; refusing DecisionReady: {e:#}"),
                        }]
                    }
                };
                out.extend(more);
            }
            other => out.push(other),
        }
    }
    out
}

/// Which prompts may be typed now, and which must be HELD.
///
/// A held prompt is not a dropped one (bd-0uzrq.8, the 01:12 stall): it used to
/// become an audit row and vanish, so the wake rang only when the doorbell's own
/// status map next transitioned — minutes later, or never. The caller re-enters
/// the held lines through the typed deferral ([`Msg::PromptRefused`]), which
/// A wake held for a busy pane: pin, rendered line, typed event id.
type HeldWake = (String, String, Option<String>);

/// queues them and journals the merge.
fn filter_prompts(model: &WorkspaceModel, cmds: Vec<Cmd>) -> (Vec<Cmd>, Vec<HeldWake>) {
    let mut keep = Vec::new();
    let mut held = Vec::new();
    for c in cmds {
        match c {
            Cmd::Prompt {
                pin,
                line,
                event_id,
            } => {
                if prompt_allowed(model, &pin)
                    || (is_act_now_line(&line)
                        && matches!(model.presence.get(&pin), Some(Presence::Busy { .. })))
                {
                    keep.push(Cmd::Prompt {
                        pin,
                        line,
                        event_id,
                    });
                } else {
                    held.push((pin, line, event_id));
                }
            }
            other => keep.push(other),
        }
    }
    (keep, held)
}

/// Hold every prompt the workspace cannot type now as a typed deferral, so the
/// doorbell's queue owns it (one class entry, journaled) instead of it becoming
/// an audit row nobody rings for.
fn hold_filtered(model: &WorkspaceModel, cmds: Vec<Cmd>) -> (WorkspaceModel, Vec<Cmd>) {
    let (keep, held) = filter_prompts(model, cmds);
    let mut next = model.clone();
    let mut out = keep;
    for (pin, line, event_id) in held {
        let (dm, c) = doorbell_slice(
            &next.doorbell,
            crate::doorbell_tick::DoorbellMsg::PromptRefused {
                pin: pin.clone(),
                line: line.clone(),
                event_id: event_id.clone(),
            },
        );
        next.doorbell = dm;
        for cmd in c {
            match cmd {
                Cmd::Prompt { .. } => {}
                other => out.push(other),
            }
        }
        out.push(Cmd::Audit {
            kind: "held".into(),
            pin,
            detail: format!("non-injectable presence held wake in the queue: {line}"),
        });
    }
    (next, out)
}

fn parse_verdict(raw: &str) -> Option<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "approve" | "approved" | "yes" | "ack" => Some("approve".into()),
        "deny" | "denied" | "no" | "reject" | "rejected" => Some("deny".into()),
        _ => None,
    }
}

fn apply_human_decision(next: &mut WorkspaceModel, cmds: &mut Vec<Cmd>, corr: &str, verdict: &str) {
    if next.open_human.remove(corr).is_some() {
        cmds.push(Cmd::Audit {
            kind: "human_decision".into(),
            pin: corr.to_string(),
            detail: verdict.to_string(),
        });
    } else {
        cmds.push(Cmd::LoudMiss {
            why: format!("HumanDecision for unknown corr {corr}: {verdict}"),
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_assignment(
    next: &mut WorkspaceModel,
    cmds: &mut Vec<Cmd>,
    corr: String,
    wake: String,
    body: &str,
    audience: String,
    priority: String,
    broadcast: bool,
    result_path: Option<String>,
    verdict: Option<String>,
) {
    if corr.is_empty() {
        return;
    }
    if !matches!(wake.as_str(), "tasked" | "standing")
        || !matches!(audience.as_str(), "human" | "agents")
        || !matches!(priority.as_str(), "queue" | "interrupt")
    {
        cmds.push(Cmd::LoudMiss {
            why: format!(
                "malformed assignment tags for corr {corr}: wake={wake} audience={audience} priority={priority}"
            ),
        });
        return;
    }
    let from_tag = verdict.as_deref().and_then(parse_verdict);
    let from_body = if next.open_human.contains_key(&corr) {
        parse_verdict(body)
    } else {
        None
    };
    if let Some(v) = from_tag.or(from_body) {
        apply_human_decision(next, cmds, &corr, &v);
        return;
    }
    if audience == "human" && broadcast {
        cmds.push(Cmd::LoudMiss {
            why: format!("broadcast to human is hard error for corr {corr}"),
        });
        return;
    }
    if priority == "interrupt" && audience == "human" {
        cmds.push(Cmd::LoudMiss {
            why: format!("interrupt never onto human pin for corr {corr}"),
        });
        return;
    }
    if wake == "standing" && body.is_empty() {
        return;
    }
    if audience == "human" && !body.is_empty() {
        mint_human_question(next, cmds, &corr, body, result_path.clone());
    }
    let state = if wake == "tasked" && body.is_empty() {
        "resulted"
    } else {
        "owed"
    };
    let receipt = Receipt {
        state: state.to_string(),
        wake: wake.clone(),
        due_at: None,
        result_path,
    };
    next.assignment.insert(corr.clone(), receipt.clone());
    cmds.push(Cmd::SqliteUpsert {
        corr: corr.clone(),
        receipt,
    });
    if next.last_now > 0 {
        next.wake.waits.insert(
            corr.clone(),
            next.last_now.saturating_add(PENDING_WAIT_TTL_SECS),
        );
    }
    cmds.push(Cmd::WaitCorr { corr });
}

fn mint_human_question(
    next: &mut WorkspaceModel,
    cmds: &mut Vec<Cmd>,
    corr: &str,
    body: &str,
    result_path: Option<String>,
) {
    if next.open_human.contains_key(corr) {
        cmds.push(Cmd::LoudMiss {
            why: format!("question already open for corr {corr}"),
        });
        return;
    }
    next.open_human.insert(
        corr.to_string(),
        Question {
            corr: corr.to_string(),
            body: body.to_string(),
            created_at: 0,
        },
    );
    cmds.push(Cmd::Publish {
        kind: 14,
        content: body.to_string(),
        corr: corr.to_string(),
        result_path,
    });
}

fn doorbell_slice(
    model: &crate::doorbell_tick::DoorbellModel,
    msg: crate::doorbell_tick::DoorbellMsg,
) -> (crate::doorbell_tick::DoorbellModel, Vec<Cmd>) {
    let (next, cmds) = crate::elm::inbox::InboxProgram::fold(model, &msg);
    let cmds = cmds
        .into_iter()
        .map(|c| Cmd::from_doorbell(c.action))
        .collect();
    (next, cmds)
}

fn swarm_slice(
    model: &crate::durable::LoopModel,
    msg: crate::durable::LoopMsg,
) -> (crate::durable::LoopModel, Vec<Cmd>) {
    let (next, actions) = crate::durable::tick(model, &msg);
    let cmds = actions.into_iter().map(Cmd::from_loop).collect();
    (next, cmds)
}

/// Shared reclaim fold (lane A): the swarm fold plus the synthesized reclaim
/// action when the fold emits none. One copy serves both `tick` (workspace-
/// owned model) and `tick_from_loop_msg` (loop-owned model).
fn reclaim_fold(
    swarm: &crate::durable::LoopModel,
    bead_id: &str,
    holder_pin: &str,
    claimant_pin: &str,
    is_live: bool,
    reason: &str,
) -> (crate::durable::LoopModel, Vec<Cmd>) {
    let (next, mut cmds) = swarm_slice(
        swarm,
        crate::durable::LoopMsg::Reclaim {
            bead_id: bead_id.to_string(),
            holder_pin: holder_pin.to_string(),
            claimant_pin: claimant_pin.to_string(),
            is_live,
        },
    );
    if !cmds.iter().any(|cmd| matches!(cmd, Cmd::Reclaim { .. })) {
        cmds.push(Cmd::Reclaim {
            bead_id: bead_id.to_string(),
            pin: holder_pin.to_string(),
            terminal_name: claimant_pin.to_string(),
            reason: if reason.is_empty() {
                "reclaim".to_string()
            } else {
                reason.to_string()
            },
        });
    }
    (next, cmds)
}

/// Replay / live-loop helper: loop messages fold the owned `LoopModel`
/// directly (bd-lane-a-kernel-honesty-ub2gd). The workspace `tick` arms for
/// these messages only read and write the swarm slice, so re-embedding the
/// model in a default `WorkspaceModel` per message was a clone with no
/// behavioral effect. The fold stays pure: no I/O, clock, rand, env, or
/// locks inside.
pub fn tick_from_loop_msg(
    model: &crate::durable::LoopModel,
    msg: &crate::durable::LoopMsg,
) -> (crate::durable::LoopModel, Vec<crate::durable::LoopAction>) {
    // The reclaim synthesis lives in `reclaim_fold` (`tick` builds it with
    // an empty reason, which renders as "reclaim").
    if let crate::durable::LoopMsg::Reclaim {
        bead_id,
        holder_pin,
        claimant_pin,
        is_live,
        ..
    } = msg
    {
        let (next, cmds) = reclaim_fold(model, bead_id, holder_pin, claimant_pin, *is_live, "");
        let acts = cmds.iter().filter_map(Cmd::to_loop_action).collect();
        return (next, acts);
    }
    // Every other loop message folds the swarm slice only: `tick`'s arms
    // for Tick/Heal/ClaimNudge/CloseNudge delegate to `swarm_slice` with
    // identical input, and the trailing `tick` bookkeeping (seq, digests,
    // counters, prompt filtering) touches only discarded workspace state —
    // `Prompt`/`Digest` cmds never survive `to_loop_action`.
    let (next, cmds) = swarm_slice(model, msg.clone());
    let acts = cmds.iter().filter_map(Cmd::to_loop_action).collect();
    (next, acts)
}

/// One entry: pure, total, deterministic, no I/O, no &mut.
/// Clock enters ONLY as Msg payload (Tick{now} / AckSlaTick{now}).
pub fn tick(model: &WorkspaceModel, msg: &Msg) -> (WorkspaceModel, Vec<Cmd>) {
    let mut next = model.clone();
    next.seq = model.seq.wrapping_add(1);
    let mut cmds: Vec<Cmd> = Vec::new();

    match msg {
        Msg::WatchNdjson { line } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::WatchNdjson { line: line.clone() },
            );
            next.doorbell = dm;
            let (m2, c) = hold_filtered(&next, c);
            next = m2;
            cmds.extend(c);
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if v.get("kind").and_then(serde_json::Value::as_u64) == Some(20001)
                    && let Some(content) = v.get("content").and_then(|c| c.as_str())
                {
                    let state = content.to_string();
                    let author = v
                        .get("pubkey")
                        .and_then(|p| p.as_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if let Some(tags) = v.get("tags").and_then(|t| t.as_array()) {
                        for tag in tags {
                            if let Some(arr) = tag.as_array()
                                && arr.len() >= 2
                                && arr[0].as_str() == Some("p")
                            {
                                let pin = arr[1].as_str().unwrap_or("").to_string();
                                if !pin.is_empty() {
                                    let expected =
                                        next.doorbell.index.npub(&pin).unwrap_or("").to_lowercase();
                                    if !expected.is_empty()
                                        && !author.is_empty()
                                        && author != expected
                                    {
                                        cmds.push(Cmd::LoudMiss {
                                            why: format!(
                                                "forged 20001 for {pin}: author is not the pin"
                                            ),
                                        });
                                        cmds.push(Cmd::Audit {
                                            kind: "presence_forge".into(),
                                            pin: pin.clone(),
                                            detail: "20001 author != pin npub".into(),
                                        });
                                        continue;
                                    }
                                    next.presence
                                        .insert(pin.clone(), Presence::from_str(&state));
                                }
                            }
                        }
                    }
                }
                if v.get("kind").and_then(serde_json::Value::as_u64) == Some(30013)
                    && let Some(tags) = v.get("tags").and_then(|t| t.as_array())
                {
                    let mut pin: Option<String> = None;
                    let mut roles: Vec<String> = Vec::new();
                    for tag in tags {
                        if let Some(arr) = tag.as_array()
                            && arr.len() >= 2
                        {
                            let k = arr[0].as_str().unwrap_or("");
                            let val = arr[1].as_str().unwrap_or("").to_string();
                            if k == "d" {
                                pin = Some(val.split(':').next_back().unwrap_or(&val).to_string());
                            } else if k == "role" {
                                roles.push(val);
                            }
                        }
                    }
                    if let Some(p) = pin {
                        for role in roles {
                            next.roles.insert(role, p.clone());
                        }
                    }
                }
                if let Some(tags) = v.get("tags").and_then(|t| t.as_array()) {
                    let mut corr: Option<String> = None;
                    let mut wake: Option<String> = None;
                    let mut audience: Option<String> = None;
                    let mut result_path: Option<String> = None;
                    let mut priority: Option<String> = None;
                    let mut verdict: Option<String> = None;
                    for tag in tags {
                        if let Some(arr) = tag.as_array()
                            && arr.len() >= 2
                        {
                            let k = arr[0].as_str().unwrap_or("");
                            let val = arr[1].as_str().unwrap_or("").to_string();
                            match k {
                                "corr" => corr = Some(val),
                                "wake" => wake = Some(val),
                                "audience" => audience = Some(val),
                                "result_path" => result_path = Some(val),
                                "priority" => priority = Some(val),
                                "verdict" => verdict = Some(val),
                                _ => {}
                            }
                        }
                    }
                    let kind = v.get("kind").and_then(serde_json::Value::as_u64);
                    if kind == Some(14)
                        && let Some(c) = corr
                    {
                        let body = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        apply_assignment(
                            &mut next,
                            &mut cmds,
                            c,
                            wake.unwrap_or_else(|| "tasked".to_string()),
                            body,
                            audience.unwrap_or_else(|| "agents".to_string()),
                            priority.unwrap_or_else(|| "queue".to_string()),
                            false,
                            result_path,
                            verdict,
                        );
                    }
                }
            }
        }
        Msg::InboxItems { raw, pin } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::InboxItems {
                    raw: raw.clone(),
                    pin: pin.clone(),
                },
            );
            next.doorbell = dm;
            let (m2, c) = hold_filtered(&next, c);
            next = m2;
            cmds.extend(c);
        }
        Msg::InboxRows { rows, pin } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::InboxRows {
                    rows: rows.clone(),
                    pin: pin.clone(),
                },
            );
            next.doorbell = dm;
            let (m2, c) = hold_filtered(&next, c);
            next = m2;
            cmds.extend(c);
            for row in rows {
                let corr = row
                    .get("corr")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let body = row
                    .get("body")
                    .or_else(|| row.get("body_md"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let verdict = row
                    .get("verdict")
                    .and_then(|v| v.as_str())
                    .map(std::string::ToString::to_string);
                apply_assignment(
                    &mut next,
                    &mut cmds,
                    corr,
                    row.get("wake")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tasked")
                        .to_string(),
                    body,
                    row.get("audience")
                        .and_then(|v| v.as_str())
                        .unwrap_or("agents")
                        .to_string(),
                    row.get("priority")
                        .and_then(|v| v.as_str())
                        .unwrap_or("queue")
                        .to_string(),
                    row.get("broadcast")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    row.get("result_path")
                        .and_then(|v| v.as_str())
                        .map(std::string::ToString::to_string),
                    verdict,
                );
            }
            for row in rows {
                let Some(corr) = row
                    .get("corr")
                    .and_then(|v| v.as_str())
                    .filter(|c| !c.is_empty())
                    .map(std::string::ToString::to_string)
                else {
                    continue;
                };
                if row.get("read_ts_us").is_some_and(|v| !v.is_null()) {
                    if let Some(e) = next.wake.ladders.get_mut(&corr) {
                        e.acked = true;
                    }
                } else if inbox_row_ack_overdue(row) {
                    let e = next
                        .wake
                        .ladders
                        .entry(corr)
                        .or_insert_with(|| ReringEntry {
                            pin: pin.clone(),
                            attempts: 0,
                            window_start_unix: next.last_now,
                            acked: false,
                            fire_and_forget: false,
                            raw_without_id: false,
                        });
                    e.acked = false;
                }
            }
            let _ = pin;
        }
        Msg::PlaneStatus { pin, status } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::PlaneStatus {
                    pin: pin.clone(),
                    status: status.clone(),
                    now_us: crate::doorbell::now_micros(),
                },
            );
            next.doorbell = dm;
            // The plane status IS a presence fact: seed the prompt gate from
            // it (done/idle -> Idle, working/blocked -> Busy, anything else
            // -> Unknown). Without this a rex host saw every wake as
            // "non-injectable presence" and filtered every prompt to Audit.
            // A confirmed Dead pane is never resurrected by a shell status:
            // the pane can still report idle at a prompt while its agent is
            // gone, and a prompt into that shell is a lost wake.
            if !matches!(next.presence.get(pin), Some(Presence::Dead { .. })) {
                next.presence
                    .insert(pin.clone(), Presence::from_str(status));
            }
            let (m2, c) = hold_filtered(&next, c);
            next = m2;
            cmds.extend(c);
        }
        Msg::PromptRefused {
            pin,
            line,
            event_id,
        } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::PromptRefused {
                    pin: pin.clone(),
                    line: line.clone(),
                    event_id: event_id.clone(),
                },
            );
            next.doorbell = dm;
            cmds.extend(c);
        }
        Msg::EventsHint {
            event_id,
            pin,
            line,
        } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::EventsHint {
                    event_id: event_id.clone(),
                    pin: pin.clone(),
                    line: line.clone(),
                },
            );
            next.doorbell = dm;
            cmds.extend(c);
        }
        Msg::EventsGap { pin } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::EventsGap { pin: pin.clone() },
            );
            next.doorbell = dm;
            cmds.extend(c);
        }
        Msg::Fact { summary } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::Fact {
                    summary: summary.clone(),
                },
            );
            next.doorbell = dm;
            if let Some(pin) = doorbell_core::fact_captain_pin(&next.roles) {
                if prompt_allowed(&next, &pin) {
                    cmds.push(Cmd::Prompt {
                        pin,
                        line: doorbell_core::fact_prompt_line(summary),
                        event_id: None,
                    });
                } else {
                    cmds.extend(c);
                }
            } else {
                cmds.extend(c);
            }
        }
        Msg::ToronCursor {
            pin,
            cursor_us,
            read_at_secs,
        } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::ToronCursor {
                    pin: pin.clone(),
                    cursor_us: *cursor_us,
                    read_at_secs: *read_at_secs,
                },
            );
            next.doorbell = dm;
            cmds.extend(c);
        }
        Msg::AckSlaTick { now } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::AckSlaTick { now: *now },
            );
            next.doorbell = dm;
            cmds.extend(c);
            let corrs: Vec<String> = next.wake.ladders.keys().cloned().collect();
            for corr in corrs {
                let Some(entry) = next.wake.ladders.get(&corr).cloned() else {
                    continue;
                };
                if entry.acked || entry.fire_and_forget {
                    next.wake.ladders.remove(&corr);
                    continue;
                }
                let verdict = match next.presence.get(&entry.pin) {
                    Some(Presence::Dead { .. }) => "Dead",
                    Some(Presence::Unknown) => "Unknown",
                    _ => "other",
                };
                let (grace, max) = (
                    next.policy.doorbell_grace_secs,
                    next.policy.doorbell_ring_max,
                );
                match next.wake.ladder_step(&corr, *now, verdict, grace, max) {
                    ReringDecision::Ring { attempt } => {
                        cmds.push(Cmd::Prompt {
                            pin: entry.pin.clone(),
                            line: doorbell_mail_line(1, &entry.pin, &corr),
                            event_id: None,
                        });
                        if let Some(e) = next.wake.ladders.get_mut(&corr) {
                            e.attempts = attempt + 1;
                            e.window_start_unix = *now;
                            if e.raw_without_id {
                                next.wake.ladders.remove(&corr);
                            }
                        }
                    }
                    ReringDecision::RecoverDead => {
                        cmds.push(Cmd::Respawn {
                            name: entry.pin.clone(),
                            reason: format!(
                                "doorbell re-ring exhausted pin={} corr={corr} attempts={}",
                                entry.pin, entry.attempts
                            ),
                        });
                        next.wake.ladders.remove(&corr);
                    }
                    ReringDecision::DigestUnknown => {
                        cmds.push(Cmd::Audit {
                            kind: "doorbell-unanswered-digest".into(),
                            pin: entry.pin.clone(),
                            detail: format!(
                                "doorbell: unanswered pin={} corr={corr} attempts={}",
                                entry.pin, entry.attempts
                            ),
                        });
                        next.wake.ladders.remove(&corr);
                    }
                    ReringDecision::Pending | ReringDecision::Skip => {}
                }
            }
        }
        Msg::SlaFired { posts } => {
            cmds.push(Cmd::Audit {
                kind: "ack-sla-hit".into(),
                pin: "-".into(),
                detail: format!("overdue corrs: {}", posts.join(",")),
            });
            cmds.push(Cmd::FetchInbox);
        }
        Msg::Sweep { now } => {
            cmds.push(Cmd::FetchInbox);
            let mut gone: Vec<(String, &'static str)> = Vec::new();
            for (corr, deadline) in &next.wake.waits {
                if *deadline <= *now {
                    gone.push((corr.clone(), "expired"));
                }
            }
            for (corr, _) in &gone {
                next.wake.waits.remove(corr);
            }
            while next.wake.waits.len() > PENDING_WAIT_CAP {
                if let Some((corr, ())) = next
                    .wake
                    .waits
                    .iter()
                    .min_by_key(|(_, d)| *d)
                    .map(|(c, _)| (c.clone(), ()))
                {
                    next.wake.waits.remove(&corr);
                    gone.push((corr, "over-cap"));
                } else {
                    break;
                }
            }
            for (corr, why) in gone {
                if !next.open_human.contains_key(&corr) {
                    next.open_human.insert(
                        corr.clone(),
                        Question {
                            corr: corr.clone(),
                            body: format!(
                                "pending-wait {why} without receipt — re-surfaced, never dropped"
                            ),
                            created_at: *now,
                        },
                    );
                }
                cmds.push(Cmd::Audit {
                    kind: "wait-timeout".into(),
                    pin: corr,
                    detail: format!("waitcorr {why}"),
                });
            }
        }
        Msg::WatchDied => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::WatchDied,
            );
            next.doorbell = dm;
            cmds.extend(c);
        }
        Msg::Reclaim {
            bead_id,
            holder_pin,
            claimant_pin,
            is_live,
            reason,
        } => {
            let (sm, c) = reclaim_fold(
                &model.swarm,
                bead_id,
                holder_pin,
                claimant_pin,
                *is_live,
                reason,
            );
            next.swarm = sm;
            cmds.extend(c);
        }
        Msg::Heal {
            name,
            status,
            exited,
        } => {
            let (sm, c) = swarm_slice(
                &model.swarm,
                crate::durable::LoopMsg::Heal {
                    name: name.clone(),
                    status: status.clone(),
                    exited: *exited,
                },
            );
            next.swarm = sm;
            cmds.extend(c);
        }
        Msg::ClaimNudge { name, bead_id } => {
            let (sm, c) = swarm_slice(
                &model.swarm,
                crate::durable::LoopMsg::ClaimNudge {
                    name: name.clone(),
                    bead_id: bead_id.clone(),
                },
            );
            next.swarm = sm;
            cmds.extend(c);
        }
        Msg::CloseNudge {
            name,
            bead_id,
            title,
        } => {
            let (sm, c) = swarm_slice(
                &model.swarm,
                crate::durable::LoopMsg::CloseNudge {
                    name: name.clone(),
                    bead_id: bead_id.clone(),
                    title: title.clone(),
                },
            );
            next.swarm = sm;
            cmds.extend(c);
        }
        Msg::Receipt { corr, receipt } => {
            if let Err(error) = crate::kernel::Corr::parse(corr.clone()) {
                cmds.push(Cmd::LoudMiss {
                    why: format!("receipt corr invalid: {error}"),
                });
            } else {
                let stale = matches!(
                    (
                        next.assignment
                            .get(corr)
                            .and_then(|current| receipt_progress(&current.state)),
                        receipt_progress(&receipt.state),
                    ),
                    (Some(current), Some(incoming)) if incoming < current
                );
                next.wake.waits.remove(corr);
                if !stale {
                    next.assignment.insert(corr.clone(), receipt.clone());
                    cmds.push(Cmd::SqliteUpsert {
                        corr: corr.clone(),
                        receipt: receipt.clone(),
                    });
                }
            }
        }
        Msg::RoleBound { role, pin } => {
            next.roles.insert(role.clone(), pin.clone());
        }
        Msg::HandoffRecorded { row } => {
            let key = handoff_key(&row.project, &row.pin, &row.bead);
            next.handoffs.insert(key, row.clone());
        }
        Msg::Presence { pin, state } => {
            if let Err(error) = crate::kernel::AgentPin::parse(pin.clone()) {
                cmds.push(Cmd::LoudMiss {
                    why: format!("presence pin invalid: {error}"),
                });
            } else {
                next.presence.insert(pin.clone(), Presence::from_str(state));
            }
        }
        Msg::HumanDecision { corr, verdict } => {
            apply_human_decision(&mut next, &mut cmds, corr, verdict);
        }
        Msg::Tick { now } => {
            let (sm, c) = swarm_slice(&model.swarm, crate::durable::LoopMsg::Tick { now: *now });
            next.swarm = sm;
            cmds.extend(c);
        }
        Msg::DiagnoseFailed { name } => {
            if let Some(hs) = next.swarm.heal_states.get_mut(name) {
                hs.awaiting_clear = false;
            }
        }
        Msg::Hc { event } => {
            let (sm, c) = swarm_slice(&model.swarm, crate::durable::LoopMsg::Hc(event.clone()));
            next.swarm = sm;
            cmds.extend(c);
        }
        Msg::MaterialContext {
            goal,
            explicit_topology,
            reason,
        } => {
            let pin = doorbell_core::fact_captain_pin(&next.roles).unwrap_or_default();
            let effect_key =
                reclassify_effect_key(goal, reason, explicit_topology.as_deref(), &pin);
            cmds.push(Cmd::Reclassify {
                goal: goal.clone(),
                pin,
                explicit_topology: explicit_topology.clone(),
                reason: reason.clone(),
                effect_key: effect_key.clone(),
            });
            cmds.push(Cmd::Audit {
                kind: "reclassify_requested".into(),
                pin: doorbell_core::fact_captain_pin(&next.roles).unwrap_or_default(),
                detail: format!(
                    "material={reason}; goal_len={}; effect_key={effect_key}",
                    goal.len()
                ),
            });
        }
        Msg::DecisionReady {
            goal: _,
            primitive,
            topology,
            argv,
            decide_reason,
            explicit_topology,
        } => {
            let (final_topo, sticky_held) = sticky_open_topology(
                next.session_topology.as_deref(),
                topology,
                explicit_topology.as_deref(),
            );
            next.session_topology = Some(final_topo.clone());
            let vehicle = if sticky_held {
                sticky_held_vehicle_announcement(primitive, &final_topo)
            } else {
                vehicle_announcement(primitive, &final_topo)
            };
            next.last_vehicle = Some(vehicle.clone());
            let announce_reason = if sticky_held {
                "session topology held (invent vehicle suppressed)"
            } else {
                decide_reason.as_str()
            };
            let line = reclassify_announce_line(&vehicle, announce_reason, sticky_held);
            if let Some(pin) = doorbell_core::fact_captain_pin(&next.roles) {
                let busy = matches!(next.presence.get(&pin), Some(Presence::Busy { .. }));
                if prompt_allowed(&next, &pin) || busy {
                    cmds.push(Cmd::Prompt {
                        pin: pin.clone(),
                        line: line.clone(),
                        event_id: None,
                    });
                } else {
                    cmds.push(Cmd::Audit {
                        kind: "prompt_filtered".into(),
                        pin,
                        detail: format!("non-injectable presence held reclassify: {line}"),
                    });
                }
            }
            cmds.push(Cmd::Audit {
                kind: "reclassify_ready".into(),
                pin: doorbell_core::fact_captain_pin(&next.roles).unwrap_or_default(),
                detail: format!(
                    "primitive={primitive} topology={final_topo} sticky={sticky_held} argv_len={} vehicle={vehicle}",
                    argv.len()
                ),
            });
        }
    }

    let mut replays: Vec<Cmd> = Vec::new();
    for cmd in &cmds {
        if let Cmd::Prompt { pin, line, .. } = cmd {
            if !is_urgent_line(line) {
                continue;
            }
            let watermark = next.wake_acked.get(pin).copied().unwrap_or(0);
            if let Some(entries) = next.wake.shown.get(pin) {
                for e in entries {
                    if e.seq > watermark
                        && e.line != *line
                        && !cmds.iter().any(|c| matches!(c, Cmd::Prompt { pin: p, line: l, .. } if p == pin && l == &e.line))
                        && !replays.iter().any(|c| matches!(c, Cmd::Prompt { pin: p, line: l, .. } if p == pin && l == &e.line))
                    {
                        let line = Cmd::Prompt {
                            pin: pin.clone(),
                            line: e.line.clone(),
                            event_id: None,
                        };
                        if prompt_allowed(&next, pin) {
                            replays.push(line);
                        }
                    }
                }
            }
        }
    }
    cmds.splice(0..0, replays);
    for cmd in &cmds {
        if let Cmd::Prompt { pin, line, .. } = cmd {
            if !is_urgent_line(line) {
                continue;
            }
            let slot = next.wake.shown.entry(pin.clone()).or_default();
            if let Some(found) = slot.iter_mut().find(|e| e.line == *line) {
                found.seq = next.seq;
            } else {
                slot.push(PresentedWake {
                    seq: next.seq,
                    line: line.clone(),
                });
            }
        }
    }

    if let Some(now) = match msg {
        Msg::AckSlaTick { now } | Msg::Sweep { now, .. } | Msg::Tick { now } => Some(*now),
        _ => None,
    } {
        next.last_now = now;
    }
    let mut tick_class = WakeClass::Benign;
    for cmd in &cmds {
        if matches!(cmd, Cmd::Digest { .. }) {
            continue;
        }
        tick_class = tick_class.max(classify_cmd(cmd));
        let entry = match cmd {
            Cmd::Prompt { pin, line, .. } if is_urgent_line(line) => Some(DigestEntry {
                seq: next.seq,
                at_unix: next.last_now,
                kind: "urgent-prompt".into(),
                pin: pin.clone(),
                detail: digest_detail_truncate(line),
            }),
            Cmd::Prompt { line, .. } if is_routine_wake_line(line) => None,
            Cmd::Prompt { pin, line, .. } => {
                let short = digest_detail_truncate(line);
                let queued = next
                    .wake
                    .digests
                    .iter()
                    .any(|e| e.pin == *pin && e.detail == short);
                if queued {
                    None
                } else {
                    Some(DigestEntry {
                        seq: next.seq,
                        at_unix: next.last_now,
                        kind: "prompt".into(),
                        pin: pin.clone(),
                        detail: short,
                    })
                }
            }
            Cmd::Audit { kind, pin, detail } => Some(DigestEntry {
                seq: next.seq,
                at_unix: next.last_now,
                kind: format!("audit:{kind}"),
                pin: pin.clone(),
                detail: digest_detail_truncate(detail),
            }),
            Cmd::FactMiss { why } => Some(DigestEntry {
                seq: next.seq,
                at_unix: next.last_now,
                kind: "fact-miss".into(),
                pin: "-".into(),
                detail: digest_detail_truncate(why),
            }),
            Cmd::LoudMiss { why } => Some(DigestEntry {
                seq: next.seq,
                at_unix: next.last_now,
                kind: "loud-miss".into(),
                pin: "-".into(),
                detail: digest_detail_truncate(why),
            }),
            Cmd::Respawn { name, reason } => Some(DigestEntry {
                seq: next.seq,
                at_unix: next.last_now,
                kind: "respawn".into(),
                pin: name.clone(),
                detail: digest_detail_truncate(reason),
            }),
            Cmd::Diagnose { name, reason } => Some(DigestEntry {
                seq: next.seq,
                at_unix: next.last_now,
                kind: "diagnose".into(),
                pin: name.clone(),
                detail: digest_detail_truncate(reason),
            }),
            _ => None,
        };
        if let Some(e) = entry {
            next.wake.digests.push(e);
        }
    }
    *next
        .wake_counters
        .entry(tick_class.label().to_string())
        .or_insert(0) += 1;
    // bd-l72k batched non-urgent channel traffic into the digest; bd-346jl
    // showed the batch also swallowed MAIL wakes, so a normal-importance done
    // mail never rang the captain or a crew. A mail wake always reaches its
    // pane; routine channel chatter keeps the afk batch.
    cmds.retain(|c| {
        !matches!(c, Cmd::Prompt { line, .. }
            if !is_urgent_line(line)
                && !doorbell_core::is_mail_wake(line)
                && !doorbell_core::is_p2p_wake(line))
    });
    if let Some(now) = match msg {
        Msg::AckSlaTick { now } | Msg::Sweep { now, .. } | Msg::Tick { now } => Some(*now),
        _ => None,
    } && !next.wake.digests.is_empty()
        && (next.wake.digests.len() >= next.policy.digest_threshold
            || now.saturating_sub(next.last_digest_unix) >= next.policy.digest_interval_secs)
    {
        let entries = std::mem::take(&mut next.wake.digests);
        next.last_digest_unix = now;
        cmds.push(Cmd::Digest { entries });
    }

    (next, cmds)
}

/// OPEN DECISIONS from a journal-msg sequence (status/helm consumer
/// path): fold ticks from Default over the msgs and return the open
/// fold. Same replay rule as cmd_doorbell_why; commands are discarded —
/// only the open_human cache matters here.
#[cfg(test)]
pub fn open_decisions_from_msgs(msgs: &[Msg]) -> Vec<OpenDecision> {
    let mut model = WorkspaceModel::default();
    for m in msgs {
        let (next, _) = tick(&model, m);
        model = next;
    }
    open_decisions(&model)
}

/// bd-2fxx: holds whose needed-by has passed — the wedge-pattern
/// escalation signal (mirrors digest_overdue: fail loud, never silently
/// batch). Non-empty means escalate now.
#[cfg(test)]
pub fn overdue_holds(model: &WorkspaceModel, now: u64) -> Vec<(String, u64)> {
    sorted_holds(model)
        .into_iter()
        .filter(|(_, d)| *d <= now)
        .collect()
}

/// Status/helm/digest rendering of the fold. Always prints the section
/// (even empty — "none" is a fact); entries are never cut off.
#[cfg(test)]
pub fn render_open_decisions(model: &WorkspaceModel) -> String {
    let rows = open_decisions(model);
    let mut out = String::from("OPEN DECISIONS");
    if rows.is_empty() {
        out.push_str(": none");
        return out;
    }
    for r in &rows {
        let _ = write!(out, "\n- [{}] {}", r.corr, r.body);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_model() -> WorkspaceModel {
        WorkspaceModel::default()
    }

    /// bd-lane-a-kernel-honesty-ub2gd: one message log, one fold. The same
    /// LoopMsg log yields the same (model, cmds), and loop messages fold
    /// the owned LoopModel directly — `tick_from_loop_msg` never embeds
    /// the model in a default WorkspaceModel per message.
    #[test]
    fn tick_from_loop_msg_folds_swarm_slice_in_place() {
        use crate::durable::{LoopModel, LoopMsg};
        let model = LoopModel::default();
        let log = vec![
            LoopMsg::Tick { now: 7 },
            LoopMsg::ClaimNudge {
                name: "a".into(),
                bead_id: "bd-1".into(),
            },
            LoopMsg::CloseNudge {
                name: "a".into(),
                bead_id: "bd-1".into(),
                title: "t".into(),
            },
            LoopMsg::Heal {
                name: "a".into(),
                status: "idle".into(),
                exited: false,
            },
            LoopMsg::Reclaim {
                bead_id: "bd-1".into(),
                holder_pin: "P".into(),
                claimant_pin: "Q".into(),
                is_live: true,
            },
        ];
        let fold = |start: &LoopModel| {
            let mut m = start.clone();
            let mut out = Vec::new();
            for msg in &log {
                let (next, acts) = tick_from_loop_msg(&m, msg);
                m = next;
                out.extend(acts);
            }
            (m, out)
        };
        let (first, first_acts) = fold(&model);
        let (second, second_acts) = fold(&model);
        assert_eq!(first, second, "same log must yield same model");
        assert_eq!(first_acts, second_acts, "same log must yield same cmds");
        // Loop-only messages delegate to the swarm slice with identical input.
        for msg in &log[..4] {
            let (via_helper, helper_acts) = tick_from_loop_msg(&model, msg);
            let (via_slice, slice_cmds) = swarm_slice(&model, msg.clone());
            assert_eq!(
                via_helper, via_slice,
                "fold must match swarm slice for {msg:?}"
            );
            let slice_acts: Vec<_> = slice_cmds.iter().filter_map(Cmd::to_loop_action).collect();
            assert_eq!(helper_acts, slice_acts, "cmds must match for {msg:?}");
        }
        // Reclaim still synthesizes the reclaim action when the fold emits none.
        let (_, reclaim_acts) = tick_from_loop_msg(&model, &log[4]);
        assert!(
            reclaim_acts.iter().any(|a| matches!(
                a,
                crate::durable::LoopAction::Reclaim { reason, .. } if reason == "reclaim"
            )),
            "reclaim must synthesize its action, got {reclaim_acts:?}"
        );
    }

    /// bd-context-handoff-064d: the resume delta covers only unseen runs.
    #[test]
    fn handoff_delta_covers_only_unseen_runs() {
        let runs = vec![
            HandoffRun {
                id: "r1".into(),
                seq: 1,
            },
            HandoffRun {
                id: "r2".into(),
                seq: 2,
            },
            HandoffRun {
                id: "r3".into(),
                seq: 3,
            },
            HandoffRun {
                id: "r4".into(),
                seq: 4,
            },
        ];
        let row = record_handoff("p", "BlueRidge", "bd-1", "r2", "r4", "/tmp/h.json");
        let (unseen, strategy, reason) = handoff_delta(Some(&row), &runs);
        assert_eq!(strategy, HandoffStrategy::DeltaSinceLastSeen);
        assert_eq!(
            unseen.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["r3", "r4"],
            "{reason}"
        );
        let (model, cmds) = tick(&empty_model(), &Msg::HandoffRecorded { row });
        assert!(cmds.is_empty(), "record emits nothing");
        let (via_model, _, _) = resume_runs_for(&model, "p", "BlueRidge", "bd-1", &runs);
        assert_eq!(
            via_model.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec!["r3", "r4"]
        );
        let (brief, strategy) = resume_brief(model.handoffs.values().next(), &runs);
        assert_eq!(strategy, HandoffStrategy::DeltaSinceLastSeen);
        assert!(brief.contains("r3") && brief.contains("r4"), "{brief}");
        assert!(!brief.contains("r1,") && !brief.contains("r2,"), "{brief}");
        let row = record_handoff("p", "BlueRidge", "bd-1", "r4", "r4", "/tmp/h.json");
        let (unseen, strategy, _) = handoff_delta(Some(&row), &runs);
        assert!(unseen.is_empty());
        assert_eq!(strategy, HandoffStrategy::DeltaSinceLastSeen);
    }

    #[test]
    fn handoff_missing_row_falls_back_to_full_brief() {
        let runs = vec![
            HandoffRun {
                id: "r1".into(),
                seq: 1,
            },
            HandoffRun {
                id: "r2".into(),
                seq: 2,
            },
        ];
        let (unseen, strategy, reason) = handoff_delta(None, &runs);
        assert_eq!(strategy, HandoffStrategy::FullBrief);
        assert_eq!(unseen.len(), 2, "{reason}");
        let (brief, strategy) = resume_brief(None, &runs);
        assert_eq!(strategy, HandoffStrategy::FullBrief);
        assert!(brief.contains("full brief"), "{brief}");
    }

    #[test]
    fn handoff_stale_row_falls_back_to_full_brief() {
        let runs = vec![HandoffRun {
            id: "r9".into(),
            seq: 9,
        }];
        let row = record_handoff("p", "BlueRidge", "bd-1", "r1", "r4", "/tmp/h.json");
        let (unseen, strategy, _) = handoff_delta(Some(&row), &runs);
        assert_eq!(strategy, HandoffStrategy::FullBrief);
        assert_eq!(unseen.len(), 1);
    }

    #[test]
    fn ws_diagnose_failed_clears_awaiting_clear() {
        let mut m = empty_model();
        m.swarm.heal_states.insert(
            "oc".into(),
            crate::durable::HealState {
                awaiting_clear: true,
                ..Default::default()
            },
        );
        let (next, cmds) = tick(&m, &Msg::DiagnoseFailed { name: "oc".into() });
        assert!(
            !next
                .swarm
                .heal_states
                .get("oc")
                .expect("kept")
                .awaiting_clear
        );
        assert_eq!(cmds, [] as [Cmd; 0]);
    }

    #[test]
    fn ws_model_tick_is_pure_and_deterministic() {
        let m = empty_model();
        let msg = Msg::WatchDied;
        let (m1, a1) = tick(&m, &msg);
        let (m2, a2) = tick(&m, &msg);
        assert_eq!(a1, a2);
        assert_eq!(m1.seq, m2.seq);
    }

    #[test]
    fn ws_model_seq_increments_on_every_msg() {
        let m = empty_model();
        assert_eq!(m.seq, 0);
        let (m1, _) = tick(&m, &Msg::WatchDied);
        assert_eq!(m1.seq, 1);
        let (m2, _) = tick(&m1, &Msg::Tick { now: 42 });
        assert_eq!(m2.seq, 2);
    }

    #[test]
    fn ws_model_plane_status_is_inject_truth() {
        let mut m = empty_model();
        m.doorbell.index.insert_pin("BlueRidge".into(), None);
        m.presence
            .insert("BlueRidge".into(), Presence::from_str("idle"));
        let (m, _) = tick(
            &m,
            &Msg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "working".into(),
            },
        );
        assert_eq!(
            m.doorbell.statuses.get("BlueRidge").map(String::as_str),
            Some("working"),
            "PlaneStatus tick is the only status write"
        );
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#;
        let (m, cmds) = tick(&m, &Msg::WatchNdjson { line: line.into() });
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "inject reads Model.doorbell.statuses, not a side map"
        );
        assert_eq!(m.doorbell.queues.get("BlueRidge").map(Vec::len), Some(1));
        let (m2, cmds2) = tick(
            &m,
            &Msg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert!(
            !cmds2.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "drained routine wakes spend zero prompts: {cmds2:?}"
        );
        assert!(
            m2.doorbell
                .queues
                .get("BlueRidge")
                .is_none_or(std::vec::Vec::is_empty)
        );
    }

    /// bd-346jl: the afk batch must never swallow a MAIL wake. A normal
    /// (non-urgent) mail-delivered ring reaches its pane exactly once; the
    /// same watch line without the mail tag keeps the batch.
    #[test]
    fn a_normal_importance_mail_wake_rings_its_pane() {
        let mut m = empty_model();
        m.doorbell.index.insert_pin("SlateHarbor".into(), None);
        m.presence
            .insert("SlateHarbor".into(), Presence::from_str("idle"));
        m.doorbell
            .statuses
            .insert("SlateHarbor".into(), "idle".into());
        let mail = r#"{"kind":9,"id":"aa","tags":[["p","SlateHarbor"],["toron-kind","mail-delivered"],["h","t-1"],["mid","msg-1"]],"content":"sealed"}"#;
        let (_m, cmds) = tick(
            &m,
            &Msg::WatchNdjson {
                line: mail.to_string(),
            },
        );
        let prompts: Vec<&Cmd> = cmds
            .iter()
            .filter(|c| matches!(c, Cmd::Prompt { .. }))
            .collect();
        assert_eq!(
            prompts.len(),
            1,
            "a normal done mail must ring exactly once: {cmds:?}"
        );
        let Cmd::Prompt { pin, line, .. } = prompts[0] else {
            unreachable!()
        };
        assert_eq!(pin, "SlateHarbor");
        assert!(
            doorbell_core::is_mail_wake(line),
            "the ring is a mail wake: {line}"
        );
        assert!(
            !doorbell_core::is_urgent_line(line),
            "and it is routine, so only the mail rule keeps it: {line}"
        );

        let channel = r#"{"kind":9,"id":"ab","tags":[["p","SlateHarbor"],["h","ch-1"]],"content":"@SlateHarbor hi"}"#;
        let (_m2, cmds2) = tick(
            &m,
            &Msg::WatchNdjson {
                line: channel.to_string(),
            },
        );
        assert!(
            !cmds2.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "routine channel traffic still batches: {cmds2:?}"
        );
    }

    #[test]
    fn ws_model_tick_composes_doorbell_watchndjson() {
        let mut m = empty_model();
        m.doorbell.index.insert_pin("BlueRidge".into(), None);
        m.presence
            .insert("BlueRidge".into(), Presence::from_str("idle"));
        m.doorbell
            .statuses
            .insert("BlueRidge".into(), "idle".into());
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#.to_string();
        let (next, cmds) = tick(&m, &Msg::WatchNdjson { line });
        assert!(
            cmds.iter().all(|c| !matches!(c, Cmd::Prompt { .. })),
            "routine wakes spend zero prompts: {cmds:?}"
        );
        assert_eq!(
            next.wake_counters.get("routine"),
            Some(&1),
            "routine tick counted: {:?}",
            next.wake_counters
        );
        assert_eq!(next.seq, 1);
    }

    #[test]
    fn ws_model_tick_clock_only_via_msg_payload() {
        let m = empty_model();
        let (m1, a1) = tick(&m, &Msg::Tick { now: 1_000_000 });
        let (m2, a2) = tick(&m, &Msg::Tick { now: 1_000_000 });
        assert_eq!(a1, a2);
        assert_eq!(m1.seq, m2.seq);
        let (_, a3) = tick(&m, &Msg::Tick { now: 2_000_000 });
        assert_eq!(a1.len(), a3.len());
    }

    #[test]
    fn ws_model_tick_no_instant_inside_source() {
        let src = include_str!("workspace.rs");
        let a = ["Instant", "::now"].concat();
        let b = ["SystemTime", "::now"].concat();
        assert!(!src.contains(&a), "tick must not call Instant now");
        assert!(!src.contains(&b), "tick must not call SystemTime now");
    }

    #[test]
    fn ws_model_presence_missing_is_unknown() {
        let m = empty_model();
        assert!(!m.presence.contains_key("GhostPin"));
        let (next, _) = tick(
            &m,
            &Msg::Presence {
                pin: "GhostPin".into(),
                state: "idle".into(),
            },
        );
        assert_eq!(
            next.presence.get("GhostPin"),
            Some(&Presence::from_str("idle"))
        );
        let (next2, _) = tick(
            &next,
            &Msg::Presence {
                pin: "GhostPin".into(),
                state: "working".into(),
            },
        );
        assert_eq!(
            next2.presence.get("GhostPin"),
            Some(&Presence::from_str("working"))
        );
        let (next3, _) = tick(
            &next2,
            &Msg::Presence {
                pin: "GhostPin".into(),
                state: "weird".into(),
            },
        );
        assert_eq!(next3.presence.get("GhostPin"), Some(&Presence::Unknown));
    }

    #[test]
    fn ws_model_receipt_inserts_assignment_and_emits_sqlite_upsert() {
        let m = empty_model();
        let receipt = Receipt {
            state: "owed".into(),
            wake: "tasked".into(),
            due_at: Some(999),
            result_path: None,
        };
        let (next, cmds) = tick(
            &m,
            &Msg::Receipt {
                corr: "FM-GS-012".into(),
                receipt: receipt.clone(),
            },
        );
        assert_eq!(next.assignment.get("FM-GS-012"), Some(&receipt));
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Cmd::SqliteUpsert { corr, receipt: r } => {
                assert_eq!(corr, "FM-GS-012");
                assert_eq!(r, &receipt);
            }
            other => panic!("expected SqliteUpsert got {:?}", other),
        }
    }

    #[test]
    fn ws_model_receipt_does_not_regress_a_resulted_assignment() {
        let mut m = empty_model();
        let resulted = Receipt {
            state: "resulted".into(),
            wake: "tasked".into(),
            due_at: None,
            result_path: Some("/tmp/result".into()),
        };
        m.assignment.insert("c".into(), resulted.clone());
        let (next, cmds) = tick(
            &m,
            &Msg::Receipt {
                corr: "c".into(),
                receipt: Receipt {
                    state: "owed".into(),
                    ..Receipt::default()
                },
            },
        );
        assert_eq!(
            next.assignment.get("c"),
            Some(&resulted),
            "a late owed receipt must not overwrite a resulted assignment"
        );
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::SqliteUpsert { .. })),
            "a stale receipt must not write a regressed row: {cmds:?}"
        );
    }

    #[test]
    fn ws_model_receipt_unparks_the_wait() {
        let mut m = empty_model();
        m.wake.waits.insert("c".into(), 999);
        let (next, _) = tick(
            &m,
            &Msg::Receipt {
                corr: "c".into(),
                receipt: Receipt::default(),
            },
        );
        assert!(!next.wake.waits.contains_key("c"));
        assert!(next.assignment.contains_key("c"));
    }

    #[test]
    fn ws_model_human_decision_one_per_corr() {
        let mut m = empty_model();
        m.open_human.insert(
            "FM-GS-012".into(),
            Question {
                corr: "FM-GS-012".into(),
                body: "approve?".into(),
                created_at: 1,
            },
        );
        let (next, cmds) = tick(
            &m,
            &Msg::HumanDecision {
                corr: "FM-GS-012".into(),
                verdict: "approve".into(),
            },
        );
        assert!(!next.open_human.contains_key("FM-GS-012"));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Audit { kind, .. } if kind == "human_decision"))
        );
        let (next2, cmds2) = tick(
            &next,
            &Msg::HumanDecision {
                corr: "FM-GS-012".into(),
                verdict: "approve".into(),
            },
        );
        assert!(cmds2.iter().any(|c| matches!(c, Cmd::LoudMiss { .. })));
        assert!(next2.open_human.is_empty());
    }

    #[test]
    fn ws_model_heal_exited_emits_diagnose() {
        let m = empty_model();
        let (_, cmds) = tick(
            &m,
            &Msg::Heal {
                name: "AgentA".into(),
                status: "blocked".into(),
                exited: true,
            },
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Diagnose { name, .. } if name == "AgentA"))
        );
        let (_, cmds2) = tick(
            &m,
            &Msg::Heal {
                name: "AgentA".into(),
                status: "blocked".into(),
                exited: false,
            },
        );
        assert_eq!(cmds2, [] as [Cmd; 0]);
    }

    #[test]
    fn ws_model_reclaim_delegates_to_swarm_slice() {
        let m = empty_model();
        let (next, _) = tick(
            &m,
            &Msg::Reclaim {
                bead_id: "br-1".into(),
                holder_pin: "A".into(),
                claimant_pin: "B".into(),
                is_live: false,
                reason: "pane_exited".into(),
            },
        );
        assert_eq!(next.seq, 1);
    }

    #[test]
    fn tick_clock_drives_swarm_queue() {
        let mut m = empty_model();
        m.swarm.queue.push("bd-1".into());
        let (parked, no_cmds) = tick(&m, &Msg::Tick { now: 1 });
        assert!(
            parked.swarm.queue == vec!["bd-1".to_string()],
            "parked tick keeps the queue: {:?}",
            parked.swarm.queue
        );
        assert!(
            !no_cmds.iter().any(|c| matches!(c, Cmd::Send { .. })),
            "parked tick sends nothing: {no_cmds:?}"
        );
        m.swarm.states.push(crate::durable::AgentState {
            name: "oc".to_string(),
            status: "idle".to_string(),
            ..Default::default()
        });
        let (next, cmds) = tick(&m, &Msg::Tick { now: 1 });
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Send { name, .. } if name == "oc")),
            "Msg::Tick must run swarm_slice, not no-op: {cmds:?}"
        );
        assert_eq!(next.swarm.queue, [] as [String; 0]);
        assert!(next.swarm.dispatch_pending.contains("oc"));
    }

    #[test]
    fn tick_heal_runs_swarm_slice() {
        let m = empty_model();
        let (next, cmds) = tick(
            &m,
            &Msg::Heal {
                name: "oc".into(),
                status: "idle".into(),
                exited: true,
            },
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Diagnose { name, .. } if name == "oc"))
        );
        assert!(
            next.swarm.heal_states.contains_key("oc"),
            "Msg::Heal must call swarm_slice so heal_states land in Model"
        );
    }

    #[test]
    fn tick_reclaim_always_emits_cmd() {
        let m = empty_model();
        let (_, cmds) = tick(
            &m,
            &Msg::Reclaim {
                bead_id: "bd-9".into(),
                holder_pin: "Holder".into(),
                claimant_pin: "Holder".into(),
                is_live: false,
                reason: "pane_exited".into(),
            },
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Reclaim { bead_id, .. } if bead_id == "bd-9")),
            "Reclaim Msg must emit Cmd even when LoopPhase transition is illegal"
        );
    }

    #[test]
    fn kind9_is_ring_not_assignment() {
        let line = r#"{"kind":9,"tags":[["corr","C9"],["wake","tasked"]],"content":"hi"}"#;
        let (next, cmds) = tick(&empty_model(), &Msg::WatchNdjson { line: line.into() });
        assert!(
            !next.assignment.contains_key("C9"),
            "kind-9 mail-delivered is the ring; assignment enters from InboxRows / rumor 14"
        );
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::SqliteUpsert { .. })));
    }

    #[test]
    fn human_decision_mints_at_most_one_question() {
        let row = serde_json::json!({
            "corr": "FM-H",
            "wake": "tasked",
            "body": "approve this?",
            "audience": "human"
        });
        let (m1, cmds1) = tick(
            &empty_model(),
            &Msg::InboxRows {
                rows: vec![row.clone()],
                pin: "Tester".into(),
            },
        );
        assert!(m1.open_human.contains_key("FM-H"));
        assert!(
            cmds1
                .iter()
                .any(|c| matches!(c, Cmd::Publish { kind, .. } if *kind == 14))
        );
        let (m2, cmds2) = tick(
            &m1,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        assert_eq!(m2.open_human.len(), 1);
        assert!(
            cmds2
                .iter()
                .any(|c| matches!(c, Cmd::LoudMiss { why } if why.contains("already open")))
        );
    }

    #[test]
    fn inject_skips_prompt_when_presence_working() {
        let mut m = empty_model();
        m.doorbell.index.insert_pin("BlueRidge".into(), None);
        m.doorbell
            .statuses
            .insert("BlueRidge".into(), "idle".into());
        m.presence
            .insert("BlueRidge".into(), Presence::from_str("working"));
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#;
        let (_, cmds) = tick(&m, &Msg::WatchNdjson { line: line.into() });
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "inject reads Model.presence; working pin is not prompted"
        );
    }

    #[test]
    fn ws_model_tick_is_total_for_all_variants() {
        let m = empty_model();
        let msgs = vec![
            Msg::WatchNdjson { line: "{}".into() },
            Msg::InboxItems {
                raw: "{}".into(),
                pin: "P".into(),
            },
            Msg::InboxRows {
                rows: vec![],
                pin: "P".into(),
            },
            Msg::PlaneStatus {
                pin: "P".into(),
                status: "idle".into(),
            },
            Msg::Fact {
                summary: "test".into(),
            },
            Msg::AckSlaTick { now: 0 },
            Msg::WatchDied,
            Msg::Reclaim {
                bead_id: "b".into(),
                holder_pin: "h".into(),
                claimant_pin: "c".into(),
                is_live: false,
                reason: String::new(),
            },
            Msg::Heal {
                name: "n".into(),
                status: "s".into(),
                exited: false,
            },
            Msg::ClaimNudge {
                name: "n".into(),
                bead_id: "b".into(),
            },
            Msg::CloseNudge {
                name: "n".into(),
                bead_id: "b".into(),
                title: "t".into(),
            },
            Msg::Sweep { now: 0 },
            Msg::Receipt {
                corr: "c".into(),
                receipt: Receipt::default(),
            },
            Msg::RoleBound {
                role: "r".into(),
                pin: "p".into(),
            },
            Msg::Presence {
                pin: "p".into(),
                state: "idle".into(),
            },
            Msg::HumanDecision {
                corr: "c".into(),
                verdict: "v".into(),
            },
            Msg::Tick { now: 123 },
            Msg::DiagnoseFailed { name: "n".into() },
        ];
        for msg in msgs {
            let (next, _) = tick(&m, &msg);
            assert_eq!(next.seq, 1, "seq must increment for {:?}", msg);
        }
    }

    #[test]
    fn declared_pause_resurface_rides_the_captain_relevant_audit() {
        let cmd = Cmd::from_loop(crate::durable::LoopAction::PausedResurface {
            name: "oc".into(),
            bead_id: "bd-1".into(),
            owner: "CooperSparrow".into(),
            age_secs: 3_600,
        });
        match &cmd {
            Cmd::Audit { kind, pin, detail } => {
                assert_eq!(kind, "pause_resurface");
                assert_eq!(pin, "CooperSparrow", "the row is addressed to the owner");
                assert!(
                    detail.contains("bd-1") && detail.contains("3600"),
                    "{detail}"
                );
            }
            other => panic!("expected a captain-relevant Audit, got {other:?}"),
        }
        assert_eq!(classify_cmd(&cmd), WakeClass::CaptainRelevant);
        assert!(cmd.to_loop_action().is_none());
    }

    #[test]
    fn wait_deadline_stamped_from_tick_clock_not_wall_clock() {
        let row = || {
            serde_json::json!({
                "corr": "C-W",
                "wake": "tasked",
                "audience": "agents",
                "priority": "queue",
                "body": "do x",
            })
        };
        let mut m = empty_model();
        m.last_now = 1_000_000;
        let (next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row()],
                pin: "p".into(),
            },
        );
        assert_eq!(
            next.wake.waits.get("C-W"),
            Some(&(1_000_000 + PENDING_WAIT_TTL_SECS)),
            "deadline = tick clock + TTL"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::WaitCorr { corr } if corr == "C-W")),
            "WaitCorr Cmd still emits: {cmds:?}"
        );
        let (bare, bare_cmds) = tick(
            &empty_model(),
            &Msg::InboxRows {
                rows: vec![row()],
                pin: "p".into(),
            },
        );
        assert!(
            !bare.wake.waits.contains_key("C-W"),
            "pre-clock model registers nothing"
        );
        assert!(
            bare_cmds
                .iter()
                .any(|c| matches!(c, Cmd::WaitCorr { corr } if corr == "C-W")),
            "pre-clock Cmd still emits: {bare_cmds:?}"
        );
    }

    #[test]
    fn wait_sweep_expires_and_caps_with_timeout_record() {
        let mut m = empty_model();
        let now = 1_000_000u64;
        m.wake.waits.insert("expired".into(), now - 1);
        m.wake.waits.insert("live".into(), now + 100_000);
        for i in 0..PENDING_WAIT_CAP + 2 {
            m.wake
                .waits
                .insert(format!("filler-{i:04}"), now + 20_000 + i as u64);
        }
        let (next, cmds) = tick(&m, &Msg::Sweep { now });
        assert!(
            !next.wake.waits.contains_key("expired"),
            "expired waits drain on Sweep"
        );
        assert!(
            next.wake.waits.contains_key("live"),
            "live waits survive Sweep"
        );
        assert!(
            next.wake.waits.len() <= PENDING_WAIT_CAP,
            "cap holds: {}",
            next.wake.waits.len()
        );
        let timeouts: Vec<&str> = cmds
            .iter()
            .filter_map(|c| match c {
                Cmd::Audit { kind, pin, .. } if kind == "wait-timeout" => Some(pin.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            timeouts.contains(&"expired"),
            "expired corr gets a timeout record: {cmds:?}"
        );
        assert!(
            !timeouts.contains(&"live"),
            "live corr gets no timeout record"
        );
        assert!(
            timeouts.len() > 2,
            "over-cap evictions recorded too ({} timeouts)",
            timeouts.len()
        );
    }

    #[test]
    fn deadline_holds_sorted_digest_and_overdue_escalation() {
        let now = 1_000_000u64;
        let mut m = empty_model();
        m.wake.waits.insert("late".into(), now - 1);
        m.wake.waits.insert("soon".into(), now + 10);
        m.wake.waits.insert("later".into(), now + 100);
        let order: Vec<String> = sorted_holds(&m).iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(
            order,
            vec!["late".to_string(), "soon".to_string(), "later".to_string()],
            "holds sort earliest needed-by first: {order:?}"
        );
        let overdue: Vec<String> = overdue_holds(&m, now)
            .iter()
            .map(|(c, _)| c.clone())
            .collect();
        assert_eq!(
            overdue,
            vec!["late".to_string()],
            "only past needed-by escalates: {overdue:?}"
        );
        let (next, cmds) = tick(&m, &Msg::Sweep { now });
        assert!(
            next.open_human.contains_key("late"),
            "overdue hold re-surfaces"
        );
        assert!(
            cmds.iter().any(
                |c| matches!(c, Cmd::Audit { kind, pin, .. } if kind == "wait-timeout" && pin == "late")
            ),
            "overdue hold gets a timeout record: {cmds:?}"
        );
        let text = render_digest_with_holds(&[], &sorted_holds(&m), now);
        let late_pos = text.find("late").expect("digest names overdue hold");
        let soon_pos = text.find("soon").expect("digest names holds");
        let later_pos = text.find("later").expect("digest names holds");
        assert!(
            late_pos < soon_pos && soon_pos < later_pos,
            "earliest deadline first:\n{text}"
        );
        assert!(text.contains("ESCALATE"), "overdue hold flagged:\n{text}");
    }
}

#[cfg(test)]
mod bell_fold_tests {
    #[test]
    fn bell_fold_no_dead_secret_or_subscribe_arms() {
        let src = include_str!("workspace.rs");
        let wn = ["WriteNcryptsec", " {"].concat();
        let sub = ["Cmd::Sub", "scribe"].concat();
        let cw = ["Msg::Claim", "Watch"].concat();
        assert!(
            !src.contains(&wn),
            "RED (zfnx.4): WriteNcryptsec arm still present — delete it, flywheel never writes ncryptsec"
        );
        assert!(
            !src.contains(&sub),
            "RED (55nx.7): the dead Subscribe Cmd still present — delete it, swarm owns claim-watch"
        );
        assert!(
            !src.contains(&cw),
            "RED (zfnx.4): the producer-less ClaimWatch Msg still present — delete it"
        );
        assert!(
            src.contains("Invoke {"),
            "the wired Invoke vehicle must exist (GS-016 rewire)"
        );
        let main = include_str!("main.rs");
        assert!(
            !main.contains(&["::Cmd::Sub", "scribe"].concat()),
            "RED (55nx.7): main.rs still matches the dead Subscribe Cmd — delete the no-op arm"
        );
        assert!(
            !main.contains(&wn),
            "RED (zfnx.4): main.rs still matches WriteNcryptsec — delete the exfil-path arm"
        );
    }

    #[test]
    fn bell_fold_no_double_bell_model() {
        let src = include_str!("main.rs");
        let needle = "let mut bell_model = doorbell_tick::DoorbellModel::default()";
        let count = src.matches(needle).count();
        assert!(
            count <= 1,
            "RED (bd-6b1y #6): main.rs has {count} bell_model = Default (7747/7819) — must be ONE model for dispatcher lifetime"
        );
    }

    #[test]
    fn bell_fold_no_status_preinsert_before_plane_tick() {
        let src = include_str!("main.rs");
        let needle = "bell_model.statuses.insert(pin.clone(), to.clone())";
        let count = src.matches(needle).count();
        assert!(
            count <= 1,
            "RED (bd-6b1y #7): main.rs still pre-inserts statuses before PlaneStatus tick"
        );
        assert!(
            !src.contains("status_by_pin.insert"),
            "RED: dispatcher still writes a side map before PlaneStatus; tick owns Model.doorbell.statuses"
        );
    }

    #[test]
    fn bell_fold_no_status_by_pin_side_map() {
        let src = include_str!("main.rs");
        assert!(
            !src.contains("fn doorbell_refresh_status("),
            "migration incomplete: refresh helper still present"
        );
        assert!(
            !src.contains("fn doorbell_sweep_inbox("),
            "migration incomplete: sweep helper still present"
        );
        assert!(
            !src.contains("status_by_pin"),
            "RED: a status_by_pin side map survives; tick PlaneStatus writes Model"
        );
        let host = include_str!("doorbell_host.rs");
        assert!(
            !host.contains("status_by_pin") && !host.contains(".statuses.insert"),
            "RED: host writes a status side map; Model.doorbell.statuses is the only map"
        );
        let brain = include_str!("../crates/doorbell-rt/src/brain.rs");
        assert!(
            !brain.contains("statuses"),
            "RED: brain owns status state; perception lives in the ticked model"
        );
    }

    #[test]
    fn bell_fold_watchdied_keeps_model() {
        // Scoped to the bell fold: the WatchDied arm must keep the returned
        // model. A crate-wide `!contains` of the old discard line self-matches
        // the test's own needle now that tests live in-corpus.
        let src = crate::test_source::crate_source();
        let start =
            crate::test_source::find_close(&src, "pub fn tick(model: &WorkspaceModel, msg: &Msg)");
        let rest = &src[start..];
        let end = rest
            .find("\npub fn ")
            .map(|i| start + i)
            .unwrap_or(src.len());
        let tick_body = &src[start..end];
        let arm = tick_body
            .find("Msg::WatchDied =>")
            .expect("WatchDied arm exists");
        let arm_end = tick_body[arm..]
            .find("\n        Msg::")
            .map(|i| arm + i)
            .unwrap_or(tick_body.len());
        let arm_body = &tick_body[arm..arm_end];
        assert!(
            !arm_body.contains("let (_, "),
            "RED (bd-6b1y #8): WatchDied must keep the returned model, never `let (_, ...)`-discard it"
        );
        assert!(
            arm_body.contains("next.doorbell = dm"),
            "WatchDied must write the ticked model back"
        );
    }

    #[test]
    fn bell_fold_once_calls_tick_not_second_reducer() {
        let main = include_str!("main.rs");
        let doorbell = include_str!("doorbell.rs");
        assert!(
            main.contains("workspace::tick"),
            "RED (bd-6b1y #9): main.rs must wire through workspace::tick"
        );
        assert!(
            doorbell.contains("workspace::tick"),
            "RED (bd-6b1y #9): doorbell.rs once_json must call workspace::tick, not apply_watch_line"
        );
        assert!(
            !doorbell.contains("fn apply_watch_line"),
            "RED (bd-3wzl): doorbell.rs still has apply_watch_line second reducer — delete it"
        );
        assert!(
            !doorbell.contains("fn apply_status_change"),
            "RED (bd-3wzl): doorbell.rs still has apply_status_change second reducer — delete it"
        );
    }

    #[test]
    fn bell_fold_fact_path_live() {
        let src = include_str!("main.rs");
        let dt = include_str!("../crates/doorbell-core/src/lib.rs");
        assert!(
            dt.contains("Fact"),
            "RED (bd-6b1y #10): DoorbellMsg::Fact missing in doorbell-core"
        );
        let host = include_str!("doorbell_host.rs");
        assert!(
            host.contains("BrainMsg::Fact") && host.contains("workspace::Msg::Fact"),
            "RED (bd-6b1y #10): Fact not on the live host path"
        );
        assert!(
            !host.contains("WorkspaceModel::default(), &msg"),
            "Fact must tick the live workspace_model, not Default"
        );
        let brain = include_str!("../crates/doorbell-rt/src/brain.rs");
        assert!(
            brain.contains("drain_facts") && brain.contains("on_fact"),
            "dispatcher must drain live facts every sweep"
        );
        assert!(
            brain.contains("take_allowed"),
            "Fact injects must honor 5/30/3 — Watch/Status/Sweep already do"
        );
        assert!(
            src.contains("drain_pending_facts") && src.contains("append_pending_fact"),
            "ingress queues facts; dispatcher ticks them on the live model"
        );
        let fact_fn = src
            .find("pub fn captain_fact_inject")
            .expect("captain_fact_inject");
        let slice = &src[fact_fn..src.len().min(fact_fn + 1800)];
        assert!(
            slice.contains("append_pending_fact"),
            "captain_fact_inject must queue for workspace::tick"
        );
        assert!(
            !slice.contains("agent_prompt") && !slice.contains("Client::connect"),
            "captain_fact_inject must not hc-inject (Tick Law)"
        );
        assert!(
            !src.contains("if let Err(e) = captain_fact_inject(&proj, ev)"),
            "RED (bd-6b1y #10): adapter still swallows Err into () at main.rs 10853-10860 — must propagate fact into Msg"
        );
    }

    #[test]
    fn bell_fold_fetch_inbox_emitted() {
        let dt = include_str!("../crates/doorbell-core/src/lib.rs");
        let main = include_str!("main.rs");
        assert!(
            dt.contains("FetchInbox"),
            "RED (bd-6b1y #13): FetchInbox variant missing"
        );
        let has_emit = main.contains("FetchInbox") || dt.contains("FetchInbox");
        assert!(
            has_emit,
            "RED (bd-6b1y #13): FetchInbox never emitted — must be a real Cmd whose rows re-enter as InboxRows"
        );
        let ws = include_str!("workspace.rs");
        assert!(ws.contains("InboxRows"), "workspace must handle InboxRows");
    }

    #[test]
    fn bell_fold_why_replay() {
        let doorbell = include_str!("doorbell.rs");
        // bd-wave5-main-eviction: `cmd_doorbell_why` moved to cmd/doorbell.rs.
        let why_src = include_str!("cmd/doorbell.rs");
        assert!(
            doorbell.contains("doorbell-journal-") && doorbell.contains("journals"),
            "RED (bd-6b1y #11): doorbell.rs journal path must be state_dir/journals/doorbell-journal"
        );
        assert!(
            why_src.contains("fn cmd_doorbell_why"),
            "RED (bd-6b1y #11): fn cmd_doorbell_why not found"
        );
        let pos = why_src.find("fn cmd_doorbell_why").unwrap();
        let slice = &why_src[pos..std::cmp::min(why_src.len(), pos + 8000)];
        assert!(
            slice.contains("fold") || slice.contains("replay") || slice.contains("tick("),
            "RED (bd-6b1y #11): doorbell why must fold tick from Default over journal and assert same Cmd sequence — currently greps pin substring, not replay"
        );
        assert!(
            slice.contains("corr") && slice.contains("seq"),
            "RED (bd-6b1y #11): why must gain --corr / --seq flags — why_fn missing corr/seq"
        );
        assert!(
            !slice.contains("raw.contains(&format!") && !slice.contains("raw.contains(&needle)"),
            "why display must filter from fold (cmds/model), not pin-substring grep"
        );
    }

    #[test]
    fn handle_workspace_cmds_waitcorr_ticks_receipt() {
        // bd-wave5-main-eviction: handler moved to cmd/doorbell.rs.
        let src = include_str!("cmd/doorbell.rs");
        let pos = src
            .find("fn handle_workspace_cmds(")
            .expect("handle_workspace_cmds");
        let slice = &src[pos..src.len().min(pos + 2500)];
        assert!(
            src.contains("fn try_tick_receipt")
                && src.contains("Msg::Receipt")
                && src.contains("wake.waits"),
            "WaitCorr must re-enter Receipt through tick (pending re-peek), not discard the store row"
        );
        assert!(
            !slice.contains("let _ = r"),
            "WaitCorr must not peek-and-drop the receipt"
        );
        assert!(
            !src.contains("subscribe cmd {filter}"),
            "Subscribe must not eprintln; live bus already subscribed"
        );
    }

    #[test]
    fn bell_fold_no_live_doorbell_tick_in_main() {
        let src = include_str!("main.rs");
        let count = src.matches("doorbell_tick::tick").count();
        assert_eq!(
            count, 0,
            "RED (bd-3wzl): main.rs still has {count} direct doorbell_tick::tick call(s) on live paths — must be 0, route through workspace::tick"
        );
        assert!(
            src.contains("workspace::tick") || src.contains("WorkspaceModel"),
            "RED (bd-3wzl): main.rs must hold WorkspaceModel and route via workspace::tick"
        );
    }
}

#[cfg(test)]
mod loop_fold_tests {
    #[test]
    fn loop_fold_live_entry_is_workspace_tick() {
        let durable = include_str!("durable.rs");
        assert!(
            durable.contains("crate::workspace::tick"),
            "swarm_loop must call workspace::tick, not durable::tick as the live entry"
        );
        let main = include_str!("main.rs");
        assert!(
            !main.contains("crate::durable::tick(&model, &msg)"),
            "loop replay must fold workspace::tick, not durable::tick"
        );
        assert!(
            !durable.contains("poll_actions.extend(reclaim_actions)"),
            "reclaim must not bypass tick"
        );
    }

    #[test]
    fn loop_fold_nudges_via_tick_msgs() {
        let durable = include_str!("durable.rs");
        assert!(
            durable.contains("LoopMsg::ClaimNudge") && durable.contains("LoopMsg::CloseNudge"),
            "RED (bd-gs8b #2): claim/close nudges must be tick Msgs, not loop-side Sends"
        );
        assert!(
            durable.contains("extra_msgs.append(&mut nudge_msgs)")
                && durable.contains("crate::workspace::tick"),
            "nudge Msgs must be folded through workspace::tick after the poll tick"
        );
        assert!(
            !durable.contains("let _ = watch_sends"),
            "nudge Sends must never be computed then discarded"
        );
    }

    #[test]
    fn loop_fold_no_heal_pre_seed() {
        let durable = include_str!("durable.rs");
        assert!(
            !durable.contains("poll_actions.extend(reclaim_actions)"),
            "RED (bd-gs8b #3): reclaim still bypasses tick via poll_actions.extend(reclaim_actions)"
        );
        assert!(
            durable.contains("crate::workspace::Msg::Reclaim") || durable.contains("Msg::Reclaim"),
            "live producer must construct Msg::Reclaim"
        );
        let has_preseed =
            durable.contains("let mut heal_actions") && durable.contains("actions = tick");
        assert!(
            !has_preseed,
            "RED (bd-gs8b #3): durable.rs has heal_actions pre-seed before tick"
        );
    }

    #[test]
    fn loop_fold_no_diagnose_filter() {
        let durable = include_str!("durable.rs");
        let lines: Vec<&str> = durable.lines().collect();
        let mut violators: Vec<usize> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if line.contains(".filter(") {
                let end = (i + 8).min(lines.len());
                if lines[i..end].join("\n").contains("Diagnose") {
                    violators.push(i + 1);
                }
            }
        }
        assert!(
            violators.is_empty(),
            "RED (bd-gs8b #4): durable.rs filters Diagnose post-tick near line(s) {:?} — DELETE the filter",
            violators
        );
    }

    #[test]
    fn loop_fold_single_event_apply() {
        let durable = include_str!("durable.rs");
        let tick_call = "apply_events_to_states(&mut next.states, &next.pending_events)";
        let loop_call = "apply_events_to_states(&mut states, &event_batch.events)";
        let tick_count = durable.matches(tick_call).count();
        let loop_count = durable.matches(loop_call).count();
        assert_eq!(
            tick_count, 1,
            "event application must happen exactly once inside Tick arm (found {tick_count} tick calls)"
        );
        assert_eq!(
            loop_count, 0,
            "loop-side apply must be gone (found {loop_count} old loop calls)"
        );
    }

    #[test]
    fn loop_fold_heal_states_on_activeloop() {
        let state = include_str!("state.rs");
        assert!(
            state.contains("heal_states"),
            "RED (bd-gs8b #14): state.rs ActiveLoop missing heal_states — move onto ActiveLoop (crash-safe)"
        );
        let durable = include_str!("durable.rs");
        assert!(
            !durable.contains("apply_diagnose_outcome") || state.contains("heal_states"),
            "RED (bd-gs8b #14): apply_diagnose_outcome loop-local mutate still present"
        );
        assert!(
            durable.contains("Msg::DiagnoseFailed"),
            "diagnose I/O failure must re-enter tick as Msg::DiagnoseFailed"
        );
        assert!(
            !durable.contains("tick(&ws.swarm, other)"),
            "fold other must not call durable::tick; map LoopMsg or skip"
        );
        assert!(
            !durable.contains("let mut heal_states: std::collections::HashMap"),
            "loop-local heal_states HashMap must be gone; LoopModel.heal_states is the only map"
        );
        assert!(
            !durable.contains("pending_events: event_batch.events.clone()"),
            "do not world-dump poll events into the model before Tick; fold Msg::Hc"
        );
        assert!(
            !durable.contains("swarm: model.clone()"),
            "swarm_loop must not wipe WorkspaceModel from Default each poll"
        );
        assert!(
            !durable.contains("swarm: next_model.clone()"),
            "DiagnoseFailed must tick the live WorkspaceModel, not a Default-wrapped copy"
        );
    }

    #[test]
    fn loop_fold_tick_is_clock_not_world_dump() {
        let durable = include_str!("durable.rs");
        let pos = durable.find("LoopMsg::Tick").unwrap_or(0);
        let slice = &durable[pos..std::cmp::min(durable.len(), pos + 800)];
        let has_world_dump =
            slice.contains("states:") && slice.contains("digests:") && slice.contains("events:");
        assert!(
            !has_world_dump,
            "RED (bd-gs8b): LoopMsg::Tick still world-dumps states/digests/events — must be Tick now clock + poll as Cmd"
        );
        assert!(
            slice.contains("now: u64") || slice.contains("now"),
            "LoopMsg::Tick should be clock payload"
        );
    }
}

#[cfg(test)]
mod assignment_tick_tests {
    use super::*;

    fn empty_model() -> WorkspaceModel {
        WorkspaceModel::default()
    }

    fn inbox_row(
        corr: &str,
        wake: &str,
        body: &str,
        audience: &str,
        result_path: Option<&str>,
        priority: Option<&str>,
    ) -> serde_json::Value {
        let mut v = serde_json::json!({
            "corr": corr,
            "wake": wake,
            "body": body,
            "audience": audience,
        });
        if let Some(rp) = result_path {
            v["result_path"] = serde_json::Value::String(rp.to_string());
        }
        if let Some(p) = priority {
            v["priority"] = serde_json::Value::String(p.to_string());
        }
        v
    }

    #[test]
    fn assignment_tick_standing_empty_is_silence() {
        let m = empty_model();
        let row = inbox_row("FM-GS-012", "standing", "", "agents", None, None);
        let (next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        let has_human_prompt = cmds
            .iter()
            .any(|c| matches!(c, Cmd::Prompt{pin,..} if pin=="human" || pin=="captain"));
        let has_result = cmds
            .iter()
            .any(|c| matches!(c, Cmd::SqliteUpsert{corr, ..} if corr=="FM-GS-012"));
        assert!(
            !has_human_prompt,
            "standing empty must be silence, no human pin Cmd"
        );
        assert!(!has_result, "standing empty must not write result");
        assert!(
            !next.assignment.contains_key("FM-GS-012"),
            "standing empty must not create assignment"
        );
    }

    #[test]
    fn assignment_tick_tasked_empty_writes_result() {
        let m = empty_model();
        let row = inbox_row(
            "FM-GS-012",
            "tasked",
            "",
            "agents",
            Some("/tmp/adr27-result.txt"),
            None,
        );
        let (next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        assert!(
            next.assignment.contains_key("FM-GS-012"),
            "tasked empty must create assignment"
        );
        let receipt = next.assignment.get("FM-GS-012").unwrap();
        assert_eq!(receipt.wake, "tasked");
        assert_eq!(
            receipt.result_path.as_deref(),
            Some("/tmp/adr27-result.txt")
        );
        let has_upsert = cmds.iter().any(|c| matches!(c, Cmd::SqliteUpsert{corr, receipt} if corr=="FM-GS-012" && receipt.result_path.is_some()));
        assert!(
            has_upsert,
            "tasked empty must emit SqliteUpsert with result_path"
        );
        let has_wait = cmds
            .iter()
            .any(|c| matches!(c, Cmd::WaitCorr{corr} if corr=="FM-GS-012"));
        assert!(has_wait, "tasked should also emit WaitCorr");
    }

    #[test]
    fn assignment_tick_default_tasked_priority_is_queue() {
        let m = empty_model();
        let row = inbox_row("C1", "tasked", "hello", "agents", None, None);
        let (next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        let r = next.assignment.get("C1").expect("should create assignment");
        assert_ne!(r.wake, "interrupt");
        let has_interrupt_err = cmds
            .iter()
            .any(|c| matches!(c, Cmd::LoudMiss{why} if why.contains("interrupt")));
        assert!(
            !has_interrupt_err,
            "default tasked priority must be queue, not interrupt"
        );
    }

    #[test]
    fn assignment_tick_interrupt_never_human() {
        let m = empty_model();
        let row = inbox_row("C2", "tasked", "hi", "human", None, Some("interrupt"));
        let (_next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        let has_err = cmds.iter().any(|c| matches!(c, Cmd::LoudMiss{why} if why.contains("interrupt") && why.contains("human")));
        assert!(
            has_err,
            "interrupt never onto human pin must be hard error (LoudMiss)"
        );
    }

    #[test]
    fn assignment_tick_audience_human_one_pin_only() {
        let m = empty_model();
        let mut row = inbox_row("C3", "tasked", "hello", "human", None, None);
        row["broadcast"] = serde_json::Value::Bool(true);
        let (_next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        let has_err = cmds.iter().any(|c| matches!(c, Cmd::LoudMiss{why} if why.contains("broadcast") || why.contains("human")));
        assert!(has_err, "audience=human broadcast must be hard error");
    }

    #[test]
    fn assignment_tick_receipt_is_durable() {
        let m = empty_model();
        let receipt = Receipt {
            state: "owed".into(),
            wake: "tasked".into(),
            due_at: Some(999),
            result_path: None,
        };
        let (next, cmds) = tick(
            &m,
            &Msg::Receipt {
                corr: "FM-GS-012".into(),
                receipt: receipt.clone(),
            },
        );
        assert_eq!(next.assignment.get("FM-GS-012"), Some(&receipt));
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::SqliteUpsert{corr, ..} if corr=="FM-GS-012"))
        );
    }

    #[test]
    fn assignment_tick_rejects_malformed_receipt_corr_without_writing() {
        let m = empty_model();
        let receipt = Receipt {
            state: "owed".into(),
            wake: "tasked".into(),
            due_at: None,
            result_path: None,
        };
        let (next, cmds) = tick(
            &m,
            &Msg::Receipt {
                corr: "bad\ncorrelation".into(),
                receipt,
            },
        );
        assert!(next.assignment.is_empty());
        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::LoudMiss { why } if why.contains("receipt corr invalid")
        )));
        assert!(
            !cmds
                .iter()
                .any(|cmd| matches!(cmd, Cmd::SqliteUpsert { .. }))
        );
    }

    #[test]
    fn assignment_tick_wait_corr_wired() {
        let m = empty_model();
        let row = inbox_row("C4", "tasked", "work", "agents", None, None);
        let (_next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::WaitCorr{corr} if corr=="C4")),
            "tasked should emit WaitCorr"
        );
    }

    #[test]
    fn assignment_tick_s1_tasked_survives() {
        let m = empty_model();
        let receipt = Receipt {
            state: "owed".into(),
            wake: "tasked".into(),
            due_at: Some(123),
            result_path: Some("/tmp/x".into()),
        };
        let (next, cmds) = tick(
            &m,
            &Msg::Receipt {
                corr: "FM-GS-012".into(),
                receipt: receipt.clone(),
            },
        );
        assert!(next.assignment.contains_key("FM-GS-012"));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::SqliteUpsert { .. })));
        let rebuilt = next.assignment.clone();
        assert_eq!(rebuilt.get("FM-GS-012"), Some(&receipt));
    }
}

#[cfg(test)]
mod role_model_tests {
    use super::*;

    fn empty_model() -> WorkspaceModel {
        WorkspaceModel::default()
    }

    #[test]
    fn role_model_role_bound_updates_roles_map() {
        let m = empty_model();
        let (next, cmds) = tick(
            &m,
            &Msg::RoleBound {
                role: "wheelhouse".into(),
                pin: "DustySparrow".into(),
            },
        );
        assert_eq!(cmds, [] as [Cmd; 0]);
        assert_eq!(
            next.roles.get("wheelhouse").map(String::as_str),
            Some("DustySparrow")
        );
        let (next2, _) = tick(
            &next,
            &Msg::RoleBound {
                role: "wheelhouse".into(),
                pin: "NewPin".into(),
            },
        );
        assert_eq!(
            next2.roles.get("wheelhouse").map(String::as_str),
            Some("NewPin")
        );
    }

    #[test]
    fn role_model_corr_inheritance_on_rebind() {
        let m = empty_model();
        let (m1, _) = tick(
            &m,
            &Msg::RoleBound {
                role: "wheelhouse".into(),
                pin: "OldPin".into(),
            },
        );
        let receipt = Receipt {
            state: "owed".into(),
            wake: "tasked".into(),
            due_at: None,
            result_path: None,
        };
        let (m2, _) = tick(
            &m1,
            &Msg::Receipt {
                corr: "FM-1".into(),
                receipt: receipt.clone(),
            },
        );
        let (m3, _) = tick(
            &m2,
            &Msg::RoleBound {
                role: "wheelhouse".into(),
                pin: "NewPin".into(),
            },
        );
        assert_eq!(m3.assignment.get("FM-1"), Some(&receipt));
        assert_eq!(
            m3.roles.get("wheelhouse").map(String::as_str),
            Some("NewPin")
        );
        assert!(m3.assignment.contains_key("FM-1"));
    }

    #[test]
    fn role_model_mail_to_role_resolves_live_pin() {
        let m = empty_model();
        let (m1, _) = tick(
            &m,
            &Msg::RoleBound {
                role: "wheelhouse".into(),
                pin: "LivePin".into(),
            },
        );
        assert_eq!(
            m1.roles.get("wheelhouse").map(String::as_str),
            Some("LivePin")
        );
        let src = include_str!("workspace.rs");
        let bad = ["toron_kind(300", "15)"].concat();
        assert!(
            !src.contains(&bad),
            "NO 30015 — role binding via 30013 role= tags only"
        );
    }

    #[test]
    fn role_model_no_30015_and_no_auto_spawn() {
        let src = include_str!("workspace.rs");
        let bad = ["toron_kind(300", "15)"].concat();
        let bad2 = ["kind(300", "15)"].concat();
        assert!(
            !src.contains(&bad) && !src.contains(&bad2),
            "NO 30015 kind — role binding via 30013 only"
        );
        assert!(
            !src.contains("auto_spawn") || src.contains("auto-spawn on role miss forbidden"),
            "auto-spawn on role miss forbidden"
        );
        let m = empty_model();
        let row = serde_json::json!({"corr": "C1", "wake": "tasked", "body": "hi", "audience": "agents", "role": "wheelhouse"});
        let (next, _cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        assert!(!next.roles.contains_key("wheelhouse"));
    }

    #[test]
    fn role_model_30013_role_tags_parsed() {
        let line = r#"{"kind":30013,"tags":[["d","toron:TestAgent"],["role","wheelhouse"]],"content":"{}"}"#;
        let (next, _) = tick(&empty_model(), &Msg::WatchNdjson { line: line.into() });
        assert_eq!(
            next.roles.get("wheelhouse").map(String::as_str),
            Some("TestAgent")
        );
        assert!(!next.roles.contains_key("30015"));
    }

    #[test]
    fn role_model_binding_lives_in_model_not_agent_names() {
        let src = include_str!("workspace.rs");
        assert!(
            src.contains("roles: BTreeMap"),
            "roles must be in WorkspaceModel"
        );
        assert!(
            src.contains("Model.roles") || src.contains("next.roles"),
            "binding must live in Model"
        );
    }
}

#[cfg(test)]
mod presence_model_tests {
    use super::*;

    fn empty_model() -> WorkspaceModel {
        WorkspaceModel::default()
    }

    #[test]
    fn presence_model_unknown_stays_unknown() {
        let m = empty_model();
        assert!(!m.presence.contains_key("Ghost"));
        let (next, _) = tick(
            &m,
            &Msg::Presence {
                pin: "Ghost".into(),
                state: "idle".into(),
            },
        );
        assert_eq!(
            next.presence.get("Ghost"),
            Some(&Presence::from_str("idle"))
        );
        let (next2, _) = tick(
            &next,
            &Msg::Presence {
                pin: "Ghost".into(),
                state: "unknown".into(),
            },
        );
        assert_eq!(next2.presence.get("Ghost"), Some(&Presence::Unknown));
        let has_idle = next2.presence.get("Ghost") == Some(&Presence::from_str("idle"));
        assert!(!has_idle, "unknown must stay unknown, never silently idle");
    }

    #[test]
    fn presence_model_working_queues() {
        let mut m = empty_model();
        m.presence.insert("P".into(), Presence::from_str("working"));
        assert_eq!(m.presence.get("P"), Some(&Presence::from_str("working")));
        let (next, _) = tick(
            &m,
            &Msg::Presence {
                pin: "P".into(),
                state: "idle".into(),
            },
        );
        assert_eq!(next.presence.get("P"), Some(&Presence::from_str("idle")));
    }

    /// bd-9mjla through the live reducer: a refused prompt defers, two
    /// deferred mails share one queue entry, and the next at-prompt status
    /// emits exactly one Prompt for that pin.
    #[test]
    fn prompt_refused_defers_through_the_workspace_tick() {
        let mut m = empty_model();
        m.doorbell.index.insert_pin("BlueRidge".into(), None);
        m.presence
            .insert("BlueRidge".into(), Presence::from_str("idle"));
        let (m1, c1) = tick(
            &m,
            &Msg::PromptRefused {
                pin: "BlueRidge".into(),
                line: "⟦toron⟧ URGENT steer (wm 1) — wake A".into(),
                event_id: None,
            },
        );
        assert!(
            c1.iter()
                .any(|c| matches!(c, Cmd::Audit { kind, .. } if kind == "deferred")),
            "deferral is audit-visible: {c1:?}"
        );
        assert_eq!(m1.doorbell.queues.get("BlueRidge").map(Vec::len), Some(1));
        let (m2, c2) = tick(
            &m1,
            &Msg::PromptRefused {
                pin: "BlueRidge".into(),
                line: "⟦toron⟧ URGENT steer (wm 2) — wake B".into(),
                event_id: None,
            },
        );
        assert!(
            !c2.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "{c2:?}"
        );
        assert_eq!(
            m2.doorbell.queues.get("BlueRidge").map(Vec::len),
            Some(1),
            "coalesced to one ring"
        );
        let (_m3, c3) = tick(
            &m2,
            &Msg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        let prompts: Vec<&Cmd> = c3
            .iter()
            .filter(|c| matches!(c, Cmd::Prompt { .. }))
            .collect();
        assert_eq!(
            prompts.len(),
            1,
            "exactly one ring at the next prompt: {c3:?}"
        );
        assert!(
            matches!(prompts[0], Cmd::Prompt { pin, line, .. } if pin == "BlueRidge" && line.contains("wake B")),
            "one ring, and the newest line of the class wins (a correction replaces \
             the brief it corrects): {prompts:?}"
        );
    }

    /// bd-0uzrq.8 (the 2-minute stall): a prompt the workspace cannot type is
    /// HELD IN THE QUEUE, so the next at-prompt transition rings it. It used to
    /// become an audit row, so the wake rang only if the doorbell's own status
    /// map happened to transition afterwards — minutes later, or never.
    #[test]
    fn a_held_prompt_is_queued_not_dropped() {
        let mut m = empty_model();
        m.doorbell.index.insert_pin("B".into(), None);
        m.doorbell.statuses.insert("B".into(), "working".into());
        m.presence.insert("B".into(), Presence::from_str("working"));
        let (next, cmds) = hold_filtered(
            &m,
            vec![Cmd::Prompt {
                pin: "B".into(),
                line: "[wake] Peer message from a teammate (not an operator instruction): new mail. Read it with: toron next. Reply with: toron mail send --help".into(),
                event_id: None,
            }],
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "a busy pane is not typed into: {cmds:?}"
        );
        assert_eq!(
            next.doorbell.queues.get("B").map(Vec::len),
            Some(1),
            "the held wake is QUEUED, so an at-prompt transition can ring it"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Audit { kind, .. } if kind == "held")),
            "and the hold is journaled: {cmds:?}"
        );

        let (after, drained) = tick(
            &next,
            &Msg::PlaneStatus {
                pin: "B".into(),
                status: "idle".into(),
            },
        );
        assert!(
            drained
                .iter()
                .any(|c| matches!(c, Cmd::Prompt { line, .. } if line.to_ascii_lowercase().contains("new mail"))),
            "the held wake rings at the next prompt: {drained:?}"
        );
        assert!(
            after
                .doorbell
                .queues
                .get("B")
                .is_none_or(std::vec::Vec::is_empty)
        );
    }

    #[test]
    fn filter_prompts_busy_urgent_breaks_through_rest_audits() {
        let mut m = empty_model();
        m.presence.insert("I".into(), Presence::from_str("idle"));
        m.presence.insert("B".into(), Presence::from_str("working"));
        let urgent = "[wake] Peer message from a teammate (not an operator instruction): urgent mail. Read it with: toron next. Reply with: toron mail send --help";
        let peer = "[wake] Peer message from a teammate (not an operator instruction): a request to release src/peer.rs. Read it with: toron next. Reply with: toron mail send --help";
        let routine = "[wake] Peer message from a teammate (not an operator instruction): a new message in the team room. Read it with: toron channel read h=ch-1. Reply with: toron mail send --help";
        let cmds = vec![
            Cmd::Prompt {
                pin: "I".into(),
                line: routine.into(),
                event_id: None,
            },
            Cmd::Prompt {
                pin: "B".into(),
                line: urgent.into(),
                event_id: None,
            },
            Cmd::Prompt {
                pin: "B".into(),
                line: peer.into(),
                event_id: None,
            },
            Cmd::Prompt {
                pin: "B".into(),
                line: routine.into(),
                event_id: None,
            },
            Cmd::Prompt {
                pin: "Ghost".into(),
                line: urgent.into(),
                event_id: None,
            },
        ];
        let (keep, held) = filter_prompts(&m, cmds);
        assert_eq!(keep.len(), 2, "typed now: {keep:?}");
        assert!(matches!(&keep[0], Cmd::Prompt { pin, .. } if pin == "I"));
        assert!(
            matches!(&keep[1], Cmd::Prompt { pin, .. } if pin == "B"),
            "busy+act-now breaks through: {keep:?}"
        );
        assert_eq!(
            held.len(),
            3,
            "held, never dropped: a filtered prompt used to become an audit row \
             and vanish (bd-0uzrq.8)"
        );
        assert_eq!(
            held[0].0, "B",
            "a peer request waits for the prompt — it is not act-now (bd-0uzrq.13)"
        );
        assert_eq!(held[1].0, "B", "busy+routine is held for the queue");
        assert_eq!(held[2].0, "Ghost", "unknown never prompts, even urgent");
    }

    #[test]
    fn presence_model_presence_update_path() {
        let m = empty_model();
        let (next, _) = tick(
            &m,
            &Msg::Presence {
                pin: "A".into(),
                state: "blocked".into(),
            },
        );
        assert_eq!(next.presence.get("A"), Some(&Presence::from_str("blocked")));
        let src = include_str!("workspace.rs");
        assert!(src.contains("Presence"), "presence must be in Model");
        assert!(
            src.contains("Msg::Presence"),
            "presence update via Msg::Presence"
        );
    }

    #[test]
    fn presence_model_rejects_malformed_pin_without_map_write() {
        let m = empty_model();
        let (next, cmds) = tick(
            &m,
            &Msg::Presence {
                pin: "bad\npin".into(),
                state: "idle".into(),
            },
        );
        assert!(next.presence.is_empty());
        assert!(cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::LoudMiss { why } if why.contains("presence pin invalid")
        )));
    }

    #[test]
    fn presence_model_inject_reads_model_not_plane() {
        let src = include_str!("workspace.rs");
        assert!(
            src.contains("presence") && src.contains("Presence"),
            "inject must read presence in Model"
        );
        let doorbell = include_str!("doorbell.rs");
        assert!(
            doorbell.contains("should_inject") || doorbell.contains("idle"),
            "should_inject stays idle|done|blocked"
        );
    }

    #[test]
    fn presence_model_watch_filter_has_20001() {
        let src = include_str!("cmd/doorbell.rs");
        assert!(
            src.contains("30013") && src.contains("20001"),
            "watch filter must include 30013 and 20001"
        );
    }

    #[test]
    fn presence_20001_forge_fails_loud() {
        let mut m = empty_model();
        let npub_self = "a".repeat(64);
        let npub_forge = "b".repeat(64);
        m.doorbell
            .index
            .insert_pin("BlueRidge".into(), Some(npub_self));
        let line = format!(
            r#"{{"kind":20001,"id":"e1","pubkey":"{npub_forge}","tags":[["p","BlueRidge"]],"content":"idle"}}"#
        );
        let (next, cmds) = tick(&m, &Msg::WatchNdjson { line });
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::LoudMiss { .. })),
            "forged 20001 must fail loud, got {cmds:?}"
        );
        assert!(
            !next.presence.contains_key("BlueRidge"),
            "forged 20001 must not write presence"
        );
    }

    #[test]
    fn presence_20001_self_report_accepted() {
        let mut m = empty_model();
        let npub_self = "a".repeat(64);
        m.doorbell
            .index
            .insert_pin("BlueRidge".into(), Some(npub_self.clone()));
        let line = format!(
            r#"{{"kind":20001,"id":"e2","pubkey":"{npub_self}","tags":[["p","BlueRidge"]],"content":"idle"}}"#
        );
        let (next, cmds) = tick(&m, &Msg::WatchNdjson { line });
        assert_eq!(
            next.presence.get("BlueRidge"),
            Some(&Presence::from_str("idle"))
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::LoudMiss { .. })),
            "self-reported 20001 must not LoudMiss, got {cmds:?}"
        );
    }

    #[test]
    fn presence_unknown_pin_gets_no_prompt() {
        let mut m = empty_model();
        m.doorbell.index.insert_pin("GhostPin".into(), None);
        let line = r#"{"kind":9,"id":"ab","tags":[["p","GhostPin"],["h","ch-1"]],"content":"@GhostPin hi"}"#;
        let (_, cmds) = tick(&m, &Msg::WatchNdjson { line: line.into() });
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "Unknown pin must get no Prompt, got {cmds:?}"
        );
    }

    /// D3 property: no consumer branch is reachable from Unknown.
    /// verdict_for never yields Idle from Unknown/missing presence;
    /// hc busy yields evidence-labeled Busy, never authority.
    #[test]
    fn d3_verdict_never_promotes_unknown() {
        let busy_evidence = Presence::Busy {
            source: "hc-evidence".into(),
        };
        let dead_endpoint = Presence::Dead {
            source: "endpoint".into(),
        };
        for hc in [
            "working", "busy", "idle", "done", "blocked", "unknown", "weird",
        ] {
            let v = verdict_for(None, hc, true);
            let expected = match hc {
                "working" | "busy" => busy_evidence.clone(),
                _ => Presence::Unknown,
            };
            assert_eq!(v, expected, "hc={hc}");
            assert_eq!(verdict_for(None, hc, false), dead_endpoint, "hc={hc}");
            assert_eq!(
                verdict_for(Some(&Presence::Unknown), hc, true),
                v,
                "Unknown behaves as missing: hc={hc}"
            );
        }
        let idle = Presence::Idle {
            source: "presence".into(),
        };
        assert_eq!(verdict_for(Some(&idle), "idle", true), idle);
        assert_eq!(
            Presence::from_str("blocked"),
            Presence::Busy {
                source: "presence-blocked: no typing target".into()
            }
        );
        assert_eq!(Presence::from_str("done"), idle);
        assert_eq!(Presence::from_str("weird"), Presence::Unknown);
        let legacy: Presence = serde_json::from_str("\"Working\"").unwrap();
        assert_eq!(
            legacy,
            Presence::Busy {
                source: "legacy".into()
            }
        );
        assert_eq!(busy_evidence.status_token(), "Busy(hc-evidence)");
        assert_eq!(idle.status_token(), "Idle(presence)");
        assert_eq!(Presence::Unknown.status_token(), "Unknown");
        let mut m = empty_model();
        for (pin, presence, prompts) in [
            (
                "i",
                Presence::Idle {
                    source: "presence".into(),
                },
                true,
            ),
            (
                "b",
                Presence::Busy {
                    source: "presence".into(),
                },
                false,
            ),
            (
                "d",
                Presence::Dead {
                    source: "endpoint".into(),
                },
                false,
            ),
            ("u", Presence::Unknown, false),
        ] {
            m.presence.insert(pin.into(), presence);
            assert_eq!(prompt_allowed(&m, pin), prompts, "pin={pin}");
        }
        assert!(!prompt_allowed(&m, "missing"), "missing key never prompts");
    }
}

#[cfg(test)]
mod human_decision_tests {
    use super::*;

    fn empty_model() -> WorkspaceModel {
        WorkspaceModel::default()
    }

    #[test]
    fn human_decision_one_per_corr() {
        let mut m = empty_model();
        m.open_human.insert(
            "FM-1".into(),
            Question {
                corr: "FM-1".into(),
                body: "q".into(),
                created_at: 1,
            },
        );
        let (m1, cmds1) = tick(
            &m,
            &Msg::HumanDecision {
                corr: "FM-1".into(),
                verdict: "approve".into(),
            },
        );
        assert!(!m1.open_human.contains_key("FM-1"));
        assert!(
            cmds1
                .iter()
                .any(|c| matches!(c, Cmd::Audit{kind, ..} if kind=="human_decision"))
        );
        let (m2, cmds2) = tick(
            &m1,
            &Msg::HumanDecision {
                corr: "FM-1".into(),
                verdict: "approve".into(),
            },
        );
        assert!(cmds2.iter().any(|c| matches!(c, Cmd::LoudMiss { .. })));
        assert!(m2.open_human.is_empty());
        let mut m3 = empty_model();
        m3.open_human.insert(
            "FM-1".into(),
            Question {
                corr: "FM-1".into(),
                body: "q".into(),
                created_at: 1,
            },
        );
    }

    #[test]
    fn human_decision_verdict_closes() {
        let mut m = empty_model();
        m.open_human.insert(
            "C1".into(),
            Question {
                corr: "C1".into(),
                body: "ask?".into(),
                created_at: 1,
            },
        );
        let (next, _) = tick(
            &m,
            &Msg::HumanDecision {
                corr: "C1".into(),
                verdict: "deny".into(),
            },
        );
        assert!(!next.open_human.contains_key("C1"));
    }

    #[test]
    fn human_decision_publish_carries_corr() {
        let m = empty_model();
        let row = serde_json::json!({
            "corr": "C-PUB", "wake": "tasked", "audience": "human",
            "priority": "queue", "body": "approve?",
            "result_path": "beads/br-1",
        });
        let (next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Recv".into(),
            },
        );
        assert!(next.open_human.contains_key("C-PUB"));
        let (kind, corr, rp) = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::Publish {
                    kind,
                    content,
                    corr,
                    result_path,
                } if !content.is_empty() => Some((*kind, corr.clone(), result_path.clone())),
                _ => None,
            })
            .expect("human audience must mint Publish, got {cmds:?}");
        assert_eq!(kind, 14, "Publish is rumor-14");
        assert_eq!(corr, "C-PUB", "Publish carries the corr");
        assert_eq!(rp.as_deref(), Some("beads/br-1"));
    }

    #[test]
    fn human_decision_live_path_no_preseed() {
        let m = empty_model();
        let mint = serde_json::json!({
            "corr": "C-LIVE", "wake": "tasked", "audience": "human",
            "priority": "queue", "body": "ship it?",
        });
        let (m1, _) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![mint],
                pin: "Recv".into(),
            },
        );
        assert!(
            m1.open_human.contains_key("C-LIVE"),
            "mint opens the question"
        );
        let answer = serde_json::json!({
            "corr": "C-LIVE", "wake": "tasked", "audience": "agents",
            "priority": "queue", "body": "", "verdict": "approve",
        });
        let (m2, cmds) = tick(
            &m1,
            &Msg::InboxRows {
                rows: vec![answer],
                pin: "Recv".into(),
            },
        );
        assert!(
            !m2.open_human.contains_key("C-LIVE"),
            "tagged answer closes it"
        );
        assert!(
            cmds.iter().any(
                |c| matches!(c, Cmd::Audit { kind, detail, .. } if kind == "human_decision" && detail == "approve")
            ),
            "exactly one close Audit, got {cmds:?}"
        );
        let (m3, cmds3) = tick(
            &m2,
            &Msg::InboxRows {
                rows: vec![serde_json::json!({
                    "corr": "C-LIVE", "wake": "tasked", "audience": "agents",
                    "priority": "queue", "body": "", "verdict": "approve",
                })],
                pin: "Recv".into(),
            },
        );
        assert!(
            cmds3.iter().any(|c| matches!(c, Cmd::LoudMiss { .. })),
            "duplicate answer fails loud, got {cmds3:?}"
        );
        assert!(m3.open_human.is_empty());
    }

    #[test]
    fn human_decision_body_fallback_while_open() {
        let m = empty_model();
        let mint = serde_json::json!({
            "corr": "C-FB", "wake": "tasked", "audience": "human",
            "priority": "queue", "body": "go?",
        });
        let (m1, _) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![mint],
                pin: "Recv".into(),
            },
        );
        let answer = serde_json::json!({
            "corr": "C-FB", "wake": "tasked", "audience": "agents",
            "priority": "queue", "body": "deny",
        });
        let (m2, cmds) = tick(
            &m1,
            &Msg::InboxRows {
                rows: vec![answer],
                pin: "Recv".into(),
            },
        );
        assert!(
            !m2.open_human.contains_key("C-FB"),
            "body verdict closes it"
        );
        assert!(
            cmds.iter().any(
                |c| matches!(c, Cmd::Audit { kind, detail, .. } if kind == "human_decision" && detail == "deny")
            ),
            "got {cmds:?}"
        );
    }

    #[test]
    fn human_decision_question_publish() {
        let mut m = empty_model();
        m.open_human.insert(
            "C2".into(),
            Question {
                corr: "C2".into(),
                body: "approve?".into(),
                created_at: 1,
            },
        );
        assert!(m.open_human.contains_key("C2"));
        assert_eq!(
            m.open_human.get("C2").map(|q| q.body.as_str()),
            Some("approve?")
        );
    }

    #[test]
    fn human_decision_second_for_same_corr_rejected_loudly() {
        let mut m = empty_model();
        m.open_human.insert(
            "FM-1".into(),
            Question {
                corr: "FM-1".into(),
                body: "q".into(),
                created_at: 1,
            },
        );
        let (m1, _) = tick(
            &m,
            &Msg::HumanDecision {
                corr: "FM-1".into(),
                verdict: "approve".into(),
            },
        );
        let (_, cmds2) = tick(
            &m1,
            &Msg::HumanDecision {
                corr: "FM-1".into(),
                verdict: "approve".into(),
            },
        );
        assert!(
            cmds2
                .iter()
                .any(|c| matches!(c, Cmd::LoudMiss{why} if why.contains("unknown corr")))
        );
    }

    #[test]
    fn human_decision_no_ui_token() {
        let src = include_str!("workspace.rs");
        let bad = ["wid", "get"].concat();
        let bad2 = ["UI ", "card"].concat();
        assert!(
            !src.contains(&bad) && !src.contains(&bad2),
            "mailbox mints no UI token/card"
        );
    }

    #[test]
    fn human_decision_from_inbox_verdict_closes_open_question() {
        let m = empty_model();
        let ask = serde_json::json!({
            "corr": "C-LIVE",
            "body_md": "ship?",
            "wake": "standing",
            "audience": "human",
        });
        let (m, _mint_cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![ask],
                pin: "Captain".into(),
            },
        );
        assert!(
            m.open_human.contains_key("C-LIVE"),
            "audience=human ask must mint the open question"
        );
        let row = serde_json::json!({
            "corr": "C-LIVE",
            "body_md": "approve",
            "wake": "tasked",
            "audience": "human",
        });
        let (next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Captain".into(),
            },
        );
        assert!(
            !next.open_human.contains_key("C-LIVE"),
            "mailbox verdict must close the open question"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Audit { kind, detail, .. } if kind == "human_decision" && detail == "approve")),
            "live producer is InboxRows, not a UI token"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Publish { .. })),
            "a verdict must not mint another question"
        );
    }

    #[test]
    fn assignment_reads_body_md_from_live_inbox_json() {
        let m = empty_model();
        let row = serde_json::json!({
            "corr": "FM-LIVE",
            "body_md": "do the work",
            "wake": "tasked",
            "audience": "agents",
        });
        let (next, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Recv".into(),
            },
        );
        assert!(
            next.assignment.contains_key("FM-LIVE"),
            "live fetch_inbox JSON uses body_md, not body"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::WaitCorr { corr } if corr == "FM-LIVE"))
        );
    }
}

#[cfg(test)]
mod d6_open_decisions_tests {
    use super::*;
    use crate::workspace_reporting::Divergence;

    fn empty_model() -> WorkspaceModel {
        WorkspaceModel::default()
    }

    #[test]
    fn d6_buried_decision_renders_after_routine_rows() {
        let mut m = empty_model();
        for i in 0..20 {
            m.assignment.insert(
                format!("R-{i:02}"),
                Receipt {
                    state: "owed".into(),
                    wake: "tasked".into(),
                    due_at: None,
                    result_path: None,
                },
            );
        }
        m.open_human.insert(
            "FM-BURIED".into(),
            Question {
                corr: "FM-BURIED".into(),
                body: "captain call?".into(),
                created_at: 7,
            },
        );
        let text = render_open_decisions(&m);
        assert!(text.starts_with("OPEN DECISIONS"), "section header: {text}");
        assert!(
            text.contains("FM-BURIED"),
            "buried decision renders: {text}"
        );
        assert!(text.contains("captain call?"), "body renders: {text}");
        assert_eq!(
            render_open_decisions(&empty_model()),
            "OPEN DECISIONS: none"
        );
    }

    #[test]
    fn d6_wait_expiry_promotes_never_drops() {
        let mut m = empty_model();
        m.wake.waits.insert("FM-W".into(), 100);
        let (next, cmds) = tick(&m, &Msg::Sweep { now: 200 });
        assert!(
            !next.wake.waits.contains_key("FM-W"),
            "expired entry leaves the wait cache"
        );
        let q = next
            .open_human
            .get("FM-W")
            .expect("expiry promotes into open_human");
        assert!(
            q.body.contains("re-surfaced"),
            "promotion body marks the source: {}",
            q.body
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Audit { kind, .. } if kind == "wait-timeout")),
            "wait-timeout audit kept: {cmds:?}"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::Publish { corr, .. } if corr == "FM-W")),
            "promotion is silent: {cmds:?}"
        );
        let (next2, _) = tick(
            &next,
            &Msg::HumanDecision {
                corr: "FM-W".into(),
                verdict: "approve".into(),
            },
        );
        assert!(!next2.open_human.contains_key("FM-W"), "answer clears");
    }

    #[test]
    fn d6_journal_fold_matches_live_tick() {
        let msgs = vec![
            Msg::Presence {
                pin: "P".into(),
                state: "idle".into(),
            },
            Msg::Sweep { now: 200 },
        ];
        let mut live = empty_model();
        for m in &msgs {
            let (next, _) = tick(&live, m);
            live = next;
        }
        assert_eq!(open_decisions_from_msgs(&msgs), open_decisions(&live));
    }

    #[test]
    fn d6_divergence_reports_all_three_without_closing() {
        let spec = DivergenceSpec {
            open_corrs: &[("C-1".to_string(), "bd-1".to_string())],
            closed_beads: &["bd-1".to_string()],
            answered_corrs: &[("C-2".to_string(), "bd-2".to_string())],
            held_beads: &["bd-2".to_string()],
            done_without_close: &["bd-3".to_string()],
        };
        let rows = record_divergence(&spec);
        assert_eq!(rows.len(), 3, "all three rules fire: {rows:?}");
        assert!(rows.contains(&Divergence::ClosedBeadOpenCorr {
            bead: "bd-1".into(),
            corr: "C-1".into()
        }));
        assert!(rows.contains(&Divergence::AnsweredCorrHeldBead {
            corr: "C-2".into(),
            bead: "bd-2".into()
        }));
        assert!(rows.contains(&Divergence::DoneWithoutLegalClose {
            bead: "bd-3".into()
        }));
        let text = render_divergence(&rows);
        assert!(text.contains("closed-bead/open-corr"), "{text}");
        assert!(text.contains("answered-corr/held-bead"), "{text}");
        assert!(text.contains("done-without-legal-close"), "{text}");
        let empty = DivergenceSpec {
            open_corrs: &[],
            closed_beads: &[],
            answered_corrs: &[],
            held_beads: &[],
            done_without_close: &[],
        };
        assert_eq!(record_divergence(&empty), [] as [Divergence; 0]);
        assert_eq!(render_divergence(&[]), "RECORD DIVERGENCE: none");
    }
}

#[cfg(test)]
mod cmd_schema_tests {
    use super::*;

    fn empty_model() -> WorkspaceModel {
        WorkspaceModel::default()
    }

    fn row(
        corr: &str,
        wake: &str,
        audience: &str,
        priority: &str,
        body: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "corr": corr, "wake": wake, "audience": audience,
            "priority": priority, "body": body,
        })
    }

    #[test]
    fn schema_publish_shape_kind14_nonempty_content() {
        let m = empty_model();
        let (_, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row("C-PUB", "tasked", "human", "queue", "approve?")],
                pin: "Recv".into(),
            },
        );
        let pub_ = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::Publish {
                    kind,
                    content,
                    corr,
                    result_path,
                } => Some((*kind, content.clone(), corr.clone(), result_path.clone())),
                _ => None,
            })
            .expect("human audience must mint Publish, got {cmds:?}");
        assert_eq!(pub_.0, 14, "Publish is rumor-14, never kind-9");
        assert!(!pub_.1.is_empty(), "Publish content carries the question");
        assert_eq!(pub_.2, "C-PUB", "Publish carries the corr (E4)");
        assert!(pub_.3.is_none(), "no result_path on this row");
    }

    #[test]
    fn schema_invoke_is_caller_applied_tick_never_emits() {
        let m = empty_model();
        let (_, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row("C-INV", "tasked", "agents", "queue", "run bug-fix")],
                pin: "Recv".into(),
            },
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Invoke { .. })),
            "tick must never emit Invoke (caller-applied), got {cmds:?}"
        );
    }

    #[test]
    fn schema_waitcorr_registers_only_nonempty_corr() {
        let m = empty_model();
        let (_, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row("", "tasked", "agents", "queue", "work")],
                pin: "Recv".into(),
            },
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::WaitCorr { .. })),
            "empty corr registers no WaitCorr, got {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SqliteUpsert { .. })),
            "empty corr writes no receipt, got {cmds:?}"
        );
        let (_, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row("C-W", "tasked", "agents", "queue", "work")],
                pin: "Recv".into(),
            },
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::WaitCorr { corr } if corr == "C-W")),
            "non-empty corr registers WaitCorr with the same corr"
        );
    }

    #[test]
    fn schema_malformed_vocab_fails_loud() {
        for (wake, audience, priority) in [
            ("bogus", "agents", "queue"),
            ("tasked", "everyone", "queue"),
            ("tasked", "agents", "later"),
        ] {
            let m = empty_model();
            let (next, cmds) = tick(
                &m,
                &Msg::InboxRows {
                    rows: vec![row("C-M", wake, audience, priority, "work")],
                    pin: "Recv".into(),
                },
            );
            assert!(
                cmds.iter().any(|c| matches!(c, Cmd::LoudMiss { .. })),
                "unknown vocab ({wake}/{audience}/{priority}) must fail loud"
            );
            assert!(
                !next.assignment.contains_key("C-M"),
                "malformed tags must not write assignment"
            );
        }
    }
}

#[cfg(test)]
mod adr0010_d1_tests {
    // governed-by: ADR-0010 D1 (bd-w98c) — re-ring ladder behavior.
    use super::*;

    fn entry(pin: &str) -> ReringEntry {
        ReringEntry {
            pin: pin.into(),
            attempts: 0,
            window_start_unix: 0,
            acked: false,
            fire_and_forget: false,
            raw_without_id: false,
        }
    }

    /// Test-local ladder shorthand (bd-fuql): folds one entry through a
    /// scratch table with the file defaults (production goes through
    /// `model.wake.ladder_step` with live policy values).
    fn ladder_due(e: &ReringEntry, now: u64, verdict: &str) -> ReringDecision {
        let mut wake = doorbell_core::wake_core::WakeModel::default();
        wake.ladders.insert("t".to_string(), e.clone());
        wake.ladder_step(
            "t",
            now,
            verdict,
            DOORBELL_GRACE_DEFAULT_SECS,
            DOORBELL_RING_MAX_DEFAULT,
        )
    }

    #[test]
    fn adr0010_d1_line_shape_is_shrunk_doorbell() {
        assert_eq!(
            doorbell_mail_line(3, "EmeraldSparrow", "c-9"),
            "[wake] Urgent mail: 3 unacked message(s) for EmeraldSparrow in project flywheel. Read and reply with: toron next --project flywheel --as EmeraldSparrow"
        );
    }

    #[test]
    fn adr0010_d1_duplicate_ring_is_zero_duplicate_work() {
        let e = entry("P");
        let a = ladder_due(&e, DOORBELL_GRACE_DEFAULT_SECS + 1, "other");
        let b = ladder_due(&e, DOORBELL_GRACE_DEFAULT_SECS + 1, "other");
        assert_eq!(a, b, "same state + same beat must decide identically");
        assert_eq!(
            doorbell_mail_line(1, "P", "c"),
            doorbell_mail_line(1, "P", "c"),
            "re-ring line is a pure function of (n, pin, corr)"
        );
    }

    #[test]
    fn adr0010_d1_one_ring_per_grace_window_then_escalate() {
        let grace = doorbell_grace_secs();
        let max = doorbell_ring_max();
        let mut e = entry("P");
        for attempt in 0..max {
            assert_eq!(
                ladder_due(&e, e.window_start_unix + grace - 1, "other"),
                ReringDecision::Pending
            );
            assert_eq!(
                ladder_due(&e, e.window_start_unix + grace, "other"),
                ReringDecision::Ring { attempt }
            );
            e.attempts = attempt + 1;
            e.window_start_unix += grace;
        }
        assert_eq!(
            ladder_due(&e, e.window_start_unix + grace, "Dead"),
            ReringDecision::RecoverDead,
            "past max rings a Dead pane takes the respawn path"
        );
        assert_eq!(
            ladder_due(&e, e.window_start_unix + grace, "Unknown"),
            ReringDecision::DigestUnknown,
            "past max rings an Unknown pane takes the digest path"
        );
    }

    #[test]
    fn adr0010_d1_skips_never_climb() {
        let mut acked = entry("P");
        acked.acked = true;
        assert_eq!(ladder_due(&acked, u64::MAX, "Dead"), ReringDecision::Skip);
        let mut faf = entry("P");
        faf.fire_and_forget = true;
        assert_eq!(ladder_due(&faf, u64::MAX, "Dead"), ReringDecision::Skip);
    }

    #[test]
    fn adr0010_d1_raw_without_id_rings_once_never_escalates() {
        let mut e = entry("P");
        e.raw_without_id = true;
        assert_eq!(
            ladder_due(&e, 0, "other"),
            ReringDecision::Ring { attempt: 0 }
        );
        e.attempts = 1;
        assert_eq!(
            ladder_due(&e, u64::MAX, "Dead"),
            ReringDecision::Skip,
            "raw without id must never escalate, even past max on a Dead pane"
        );
    }

    #[test]
    fn adr0010_d1_tick_advances_ladder_on_ack_sla_beat() {
        let mut m = WorkspaceModel::default();
        m.presence.insert(
            "P".into(),
            Presence::Busy {
                source: "test".into(),
            },
        );
        m.wake.ladders.insert("c-1".into(), entry("P"));
        let (_, cmds) = tick(&m, &Msg::AckSlaTick { now: 1 });
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "fresh entry must not ring inside its window: {cmds:?}"
        );
        let (next, cmds) = tick(
            &m,
            &Msg::AckSlaTick {
                now: doorbell_grace_secs() + 1,
            },
        );
        assert_eq!(
            cmds.iter()
                .filter(|c| matches!(c, Cmd::Prompt { .. }))
                .count(),
            1,
            "one re-ring prompt per grace window: {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::Prompt { pin, line, .. }
                if pin == "P"
                    && line == "[wake] Urgent mail: 1 unacked message(s) for P in project flywheel. Read and reply with: toron next --project flywheel --as P")),
            "re-ring must prompt the shrunk doorbell line, not the payload: {cmds:?}"
        );
        assert_eq!(next.wake.ladders["c-1"].attempts, 1);
    }

    #[test]
    fn adr0010_d1_tick_escalates_dead_and_digests_unknown() {
        let mut m = WorkspaceModel::default();
        m.presence.insert(
            "D".into(),
            Presence::Dead {
                source: "test".into(),
            },
        );
        m.presence.insert("U".into(), Presence::Unknown);
        let max = doorbell_ring_max();
        let mut ed = entry("D");
        ed.attempts = max;
        ed.window_start_unix = 0;
        let mut eu = entry("U");
        eu.attempts = max;
        eu.window_start_unix = 0;
        m.wake.ladders.insert("c-d".into(), ed);
        m.wake.ladders.insert("c-u".into(), eu);
        let (next, cmds) = tick(
            &m,
            &Msg::AckSlaTick {
                now: doorbell_grace_secs() + 1,
            },
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Respawn { name, reason }
                if name == "D" && reason.contains("c-d"))),
            "exhausted Dead entry must take the respawn path: {cmds:?}"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Audit { kind, pin, detail }
                if kind == "doorbell-unanswered-digest" && pin == "U" && detail.contains("c-u"))),
            "exhausted Unknown entry must digest naming pin+corr+attempts: {cmds:?}"
        );
        assert!(
            next.wake.ladders.is_empty(),
            "terminal entries leave the ladder"
        );
    }

    fn overdue_row(corr: &str, id: &str, read: Option<i64>) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "corr": corr,
            "ack_required": true,
            "importance": "high",
            "read_ts_us": read,
            "created_ts_us": 1_000,
        })
    }

    #[test]
    fn adr0010_d1_slafired_audits_and_pulls_the_inbox() {
        let (next, cmds) = tick(
            &WorkspaceModel::default(),
            &Msg::SlaFired {
                posts: vec!["corr-1".into(), "corr-2".into()],
            },
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Audit { kind, detail, .. }
                if kind == "ack-sla-hit" && detail.contains("corr-1") && detail.contains("corr-2"))),
            "the hit list must be captain-visible as data: {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::FetchInbox)),
            "the registrar pipe must pull the inbox: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "the hit list never rings directly — the ladder owns re-rings: {cmds:?}"
        );
        assert!(next.wake.ladders.is_empty(), "no row, no registration");
    }

    #[test]
    fn adr0010_d1_inbox_rows_register_overdue_corrs_on_the_ladder() {
        let m = WorkspaceModel {
            last_now: 5_000,
            ..Default::default()
        };
        let (next, _) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![
                    overdue_row("c-over", "id-1", None),
                    overdue_row("c-read", "id-2", Some(1_100)),
                    serde_json::json!({
                        "id": "id-3", "corr": "c-normal", "ack_required": true,
                        "importance": "normal", "read_ts_us": serde_json::Value::Null,
                    }),
                    serde_json::json!({
                        "id": "id-4", "ack_required": true,
                        "importance": "urgent", "read_ts_us": serde_json::Value::Null,
                    }),
                ],
                pin: "P".into(),
            },
        );
        assert_eq!(
            next.wake.ladders.get("c-over").map(|e| e.pin.as_str()),
            Some("P"),
            "an unacked overdue corr registers: {:?}",
            next.wake.ladders
        );
        let e = &next.wake.ladders["c-over"];
        assert_eq!(
            e.window_start_unix, 5_000,
            "window stamps from the tick clock (bd-bdpx)"
        );
        assert_eq!(e.attempts, 0, "registration is not a ring");
        assert!(
            !next.wake.ladders.contains_key("c-normal"),
            "outside the scan scope"
        );
        assert_eq!(next.wake.ladders.len(), 1, "no corr, no entry");
        assert!(
            !next.wake.ladders.contains_key("c-read"),
            "a read never registers"
        );
        let (next2, _) = tick(
            &next,
            &Msg::InboxRows {
                rows: vec![overdue_row("c-over", "id-1", Some(5_100))],
                pin: "P".into(),
            },
        );
        assert!(
            next2.wake.ladders["c-over"].acked,
            "the per-recipient read is the handling fact"
        );
    }

    #[test]
    fn adr0010_d1_registration_then_ladder_beat_rings_without_a_send() {
        let m = WorkspaceModel {
            last_now: 100,
            ..Default::default()
        };
        let (m, _) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![overdue_row("c-wire", "id-9", None)],
                pin: "WirePin".into(),
            },
        );
        let (next, cmds) = tick(
            &m,
            &Msg::AckSlaTick {
                now: 100 + doorbell_grace_secs(),
            },
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Prompt { pin, line, .. }
                if pin == "WirePin" && line.contains("unacked message(s) for WirePin"))),
            "wire-registered corr re-rings through the shrunk doorbell line: {cmds:?}"
        );
        assert_eq!(next.wake.ladders["c-wire"].attempts, 1);
    }

    #[test]
    fn adr0010_d1_registration_is_idempotent_and_refreshes_window() {
        let m = WorkspaceModel {
            last_now: 100,
            ..Default::default()
        };
        let (m, _) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![overdue_row("c-i", "id-i", None)],
                pin: "P".into(),
            },
        );
        assert_eq!(m.wake.ladders["c-i"].attempts, 0);
        let (m, _) = tick(
            &m,
            &Msg::AckSlaTick {
                now: 100 + doorbell_grace_secs(),
            },
        );
        assert_eq!(m.wake.ladders["c-i"].attempts, 1, "one ring");
        let (m2, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![overdue_row("c-i", "id-i", None)],
                pin: "P".into(),
            },
        );
        assert_eq!(
            m2.wake.ladders["c-i"].attempts, 1,
            "redelivery never double-rings"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "redelivery is quiet: {cmds:?}"
        );
    }

    #[test]
    fn adr0010_d1_slafired_same_log_same_effects() {
        let build = || {
            let m = WorkspaceModel {
                last_now: 42,
                ..Default::default()
            };
            let logs: Vec<Vec<Msg>> = vec![
                vec![
                    Msg::SlaFired {
                        posts: vec!["c-a".into()],
                    },
                    Msg::InboxRows {
                        rows: vec![overdue_row("c-a", "id-a", None)],
                        pin: "P".into(),
                    },
                    Msg::AckSlaTick { now: 1_000 },
                ],
                vec![
                    Msg::SlaFired { posts: vec![] },
                    Msg::InboxRows {
                        rows: vec![],
                        pin: "P".into(),
                    },
                    Msg::AckSlaTick { now: 1_000 },
                ],
            ];
            (m, logs)
        };
        let (m0, logs) = build();
        let mut a = m0.clone();
        let mut a_effects: Vec<Vec<String>> = Vec::new();
        for log in &logs {
            for msg in log {
                let (n, c) = tick(&a, msg);
                a = n;
                a_effects.push(c.iter().map(|c| format!("{c:?}")).collect());
            }
        }
        let (m0, logs) = build();
        let mut b = m0;
        let mut b_effects: Vec<Vec<String>> = Vec::new();
        for log in &logs {
            for msg in log {
                let (n, c) = tick(&b, msg);
                b = n;
                b_effects.push(c.iter().map(|c| format!("{c:?}")).collect());
            }
        }
        assert_eq!(a, b, "same log folds to the same model");
        assert_eq!(a_effects, b_effects, "same log emits the same effects");
    }
}

#[cfg(test)]
mod adr0010_d2_tests {
    // governed-by: ADR-0010 D2 (bd-8jlv) — WakeAck generation-bound watermark.
    use super::*;

    fn fact(summary: &str) -> Msg {
        Msg::Fact {
            summary: format!("plane-P urgent {summary}"),
        }
    }

    #[test]
    fn adr0010_d2_represent_respects_prompt_gate() {
        let mut m = WorkspaceModel::default();
        m.presence.insert(
            "Q".into(),
            Presence::Busy {
                source: "test".into(),
            },
        );
        m.roles.insert("captain".into(), "Q".into());
        m.wake.shown.insert(
            "Q".into(),
            vec![PresentedWake {
                seq: 1,
                line: "[doorbell] old".into(),
            }],
        );
        let (_, cmds) = tick(&m, &fact("new"));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "gated pin prompts nothing, re-present included: {cmds:?}"
        );
    }
}

#[cfg(test)]
mod adr0010_d5_tests {
    // governed-by: ADR-0010 D5 (bd-65he) — wake classification + afk digest.
    use super::*;

    fn urgent_row() -> serde_json::Value {
        serde_json::json!({"importance": "urgent", "subject": "wake"})
    }

    fn idle_captain(pin: &str) -> WorkspaceModel {
        let mut m = WorkspaceModel::default();
        m.presence.insert(
            pin.into(),
            Presence::Idle {
                source: "test".into(),
            },
        );
        m.doorbell.statuses.insert(pin.into(), "idle".into());
        m.roles.insert("captain".into(), pin.into());
        m
    }

    #[test]
    fn adr0010_d5_fixture_replay_afk_classes() {
        let mut m = idle_captain("P");
        let chatter = r#"{"kind":9,"id":"z","tags":[["p","Nobody"],["h","ch"]],"content":"hi"}"#;
        let (m1, c1) = tick(
            &m,
            &Msg::WatchNdjson {
                line: chatter.into(),
            },
        );
        m = m1;
        assert!(
            !c1.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "benign chatter spends zero prompts: {c1:?}"
        );
        let (m2, c2) = tick(&m, &Msg::AckSlaTick { now: 10 });
        m = m2;
        assert!(
            !c2.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "routine beats spend zero prompts: {c2:?}"
        );
        let mut m3 = WorkspaceModel::default();
        m3.presence.insert(
            "P".into(),
            Presence::Idle {
                source: "test".into(),
            },
        );
        let (m4, c4) = tick(
            &m3,
            &Msg::Fact {
                summary: "orphan".into(),
            },
        );
        assert!(
            c4.iter().any(|c| matches!(c, Cmd::FactMiss { .. })),
            "alias-less fact must miss loud: {c4:?}"
        );
        assert!(
            !c4.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "misses spend zero prompts: {c4:?}"
        );
        assert_eq!(m4.wake.digests.len(), 1, "miss queues for digest");
        assert_eq!(m4.wake.digests[0].kind, "fact-miss");
        let (m5, c5) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![urgent_row()],
                pin: "P".into(),
            },
        );
        assert!(
            c5.iter().any(|c| matches!(c, Cmd::Prompt { pin, line, .. }
                if pin == "P" && is_urgent_line(line))),
            "urgent bypasses batching with an immediate prompt: {c5:?}"
        );
        let _ = m5;
    }

    #[test]
    fn adr0010_d5_digest_batch_interval_and_threshold() {
        let mut m = WorkspaceModel::default();
        for i in 0..2 {
            let (next, cmds) = tick(
                &m,
                &Msg::Fact {
                    summary: format!("orphan-{i}"),
                },
            );
            assert!(
                !cmds.iter().any(|c| matches!(c, Cmd::Digest { .. })),
                "below threshold/interval no batch emits yet"
            );
            m = next;
        }
        assert_eq!(m.wake.digests.len(), 2);
        let (next, cmds) = tick(
            &m,
            &Msg::AckSlaTick {
                now: digest_interval_secs() + 1,
            },
        );
        let digests: Vec<&Vec<DigestEntry>> = cmds
            .iter()
            .filter_map(|c| match c {
                Cmd::Digest { entries } => Some(entries),
                _ => None,
            })
            .collect();
        assert_eq!(digests.len(), 1, "one batch per due beat: {cmds:?}");
        assert_eq!(digests[0].len(), 2, "digest is exactly the queued set");
        assert!(
            digests[0].iter().all(|e| e.seq > 0 && e.at_unix == 0),
            "entries carry generation + clock (0 = pre-clock fixture)"
        );
        assert!(next.wake.digests.is_empty(), "batch drains the queue");
        assert_eq!(next.last_digest_unix, digest_interval_secs() + 1);
    }

    #[test]
    fn adr0010_d5_counters_prove_zero_prompt_classes() {
        let m = idle_captain("P");
        let chatter = r#"{"kind":9,"id":"z","tags":[["p","Nobody"],["h","ch"]],"content":"hi"}"#;
        let (m, _) = tick(
            &m,
            &Msg::WatchNdjson {
                line: chatter.into(),
            },
        );
        let (m, _) = tick(&m, &Msg::AckSlaTick { now: 5 });
        assert_eq!(m.wake_counters.get("benign"), Some(&1));
        assert_eq!(m.wake_counters.get("routine"), Some(&1));
        assert_eq!(m.wake_counters.get("captain"), None);
        assert_eq!(m.wake_counters.get("urgent"), None);
    }

    #[test]
    fn adr0010_d5_render_digest_shape() {
        let entries = vec![
            DigestEntry {
                seq: 4,
                at_unix: 100,
                kind: "respawn".into(),
                pin: "P".into(),
                detail: "doorbell re-ring exhausted pin=P corr=c attempts=3 (bd-8jlv)".into(),
            },
            DigestEntry {
                seq: 7,
                at_unix: 160,
                kind: "fact-miss".into(),
                pin: "-".into(),
                detail: "fact without captain alias: deploy".into(),
            },
        ];
        let text = render_digest(&entries, 200);
        assert!(text.contains("doorbell afk digest (2 items)"), "{text}");
        assert!(text.contains("[100s ago] respawn P:"), "{text}");
        assert!(text.contains("[40s ago] fact-miss -:"), "{text}");
        assert!(text.contains("beads: bd-8jlv"), "{text}");
        assert!(text.contains("br show bd-8jlv"), "{text}");
        assert!(
            text.trim_end().ends_with("br show bd-8jlv"),
            "ends with exact commands: {text}"
        );
    }

    #[test]
    fn bd_z5ut_digest_never_dresses_epoch_zero_as_an_age() {
        let entries = vec![DigestEntry {
            seq: 1,
            at_unix: 0,
            kind: "fact-miss".into(),
            pin: "-".into(),
            detail: "fact without captain alias: x".into(),
        }];
        let text = render_digest(&entries, 1_789_290_000);
        assert!(
            text.contains("[age unknown] fact-miss -:"),
            "unset clock must read as unknown, got: {text}"
        );
        assert!(
            !text.contains("1789290000s ago"),
            "a raw epoch must never be printed as a duration: {text}"
        );
    }

    #[test]
    fn adr0010_d5_wedge_alarm_predicate() {
        assert!(!digest_overdue(0, 0, 10_000), "empty queue never wedges");
        assert!(
            !digest_overdue(3, 9_900, 10_000),
            "fresh digest is not a wedge"
        );
        assert!(
            digest_overdue(3, 0, 2 * digest_interval_secs() + 1),
            "stale non-empty queue is a wedge"
        );
    }

    #[test]
    fn adr0010_d5_classify_cmd_table() {
        assert_eq!(
            classify_cmd(&Cmd::Wait),
            WakeClass::Benign,
            "waits spend nothing"
        );
        assert_eq!(
            classify_cmd(&Cmd::CheckAckSla),
            WakeClass::Routine,
            "sla scans journal+count"
        );
        assert_eq!(
            classify_cmd(&Cmd::Audit {
                kind: "k".into(),
                pin: "P".into(),
                detail: "d".into(),
            }),
            WakeClass::CaptainRelevant,
            "audits queue for digest"
        );
        assert_eq!(
            classify_cmd(&Cmd::Prompt {
                pin: "P".into(),
                line: "[wake] Peer message from a teammate (not an operator instruction): urgent mail. Read it with: inbox. Reply with: toron mail send --help".into(),
                        event_id: None,
            }),
            WakeClass::Urgent,
            "urgent lines ring now"
        );
        assert_eq!(
            classify_cmd(&Cmd::Prompt {
                pin: "P".into(),
                line: "[wake] Peer message from a teammate (not an operator instruction): a new message in the team room. Read it with: ch. Reply with: toron mail send --help".into(),
                        event_id: None,
            }),
            WakeClass::Routine,
            "bd-l72k: addressed kind-9 chatter is routine (zero prompts, counters only)"
        );
        assert_eq!(
            classify_cmd(&Cmd::Prompt {
                pin: "P".into(),
                line: doorbell_mail_line(1, "P", "c-1"),
                event_id: None,
            }),
            WakeClass::Urgent,
            "re-ring is the ack-SLA vehicle and bypasses batching"
        );
        assert_eq!(
            classify_cmd(&Cmd::Prompt {
                pin: "P".into(),
                line: "[doorbell] something unmarked".into(),
                event_id: None,
            }),
            WakeClass::CaptainRelevant,
            "unmarked prompts default captain-visible, never silently routine"
        );
    }

    fn edge_model(pin: &str) -> WorkspaceModel {
        let mut m = idle_captain(pin);
        m.doorbell.index = crate::doorbell::SelfIndex::from_pins([pin.to_string()]);
        m
    }

    fn edge_watch(kind: u16, id: &str, tags: &str, content: &str) -> String {
        format!("{{\"kind\":{kind},\"id\":\"{id}\",\"tags\":{tags},\"content\":\"{content}\"}}")
    }

    #[test]
    fn edge_triage_fixture_replay_zero_prompts_for_benign_routine() {
        let mut m = edge_model("BlueRidge");
        let benign = edge_watch(9, "b1", r#"[["h","ch-1"]]"#, "room talk");
        let routine = edge_watch(
            9,
            "r1",
            r#"[["p","BlueRidge"],["h","ch-1"]]"#,
            "@BlueRidge hi",
        );
        let captain = edge_watch(
            43001,
            "c1",
            r#"[["p","BlueRidge"],["h","steer-x"]]"#,
            "job ready",
        );
        let urgent = edge_watch(
            14,
            "u1",
            r#"[["p","BlueRidge"],["toron-kind","request-release"]]"#,
            "release please",
        );
        let mut prompts = Vec::new();
        for line in [&benign, &routine, &captain, &urgent] {
            let (next, cmds) = tick(&m, &Msg::WatchNdjson { line: line.clone() });
            prompts.extend(cmds.iter().filter_map(|c| match c {
                Cmd::Prompt { pin, line, .. } => Some((pin.clone(), line.clone())),
                _ => None,
            }));
            m = next;
        }
        assert_eq!(
            prompts.len(),
            1,
            "only the urgent row may inject, got {prompts:?}"
        );
        assert_eq!(prompts[0].0, "BlueRidge");
        assert!(
            is_urgent_line(&prompts[0].1),
            "the surviving prompt must be urgent: {}",
            prompts[0].1
        );
        assert_eq!(
            m.wake.digests.len(),
            2,
            "digest holds the captain set plus the urgent record (urgent prompts AND queues), got {:?}",
            m.wake.digests
        );
        let kinds: Vec<&str> = m.wake.digests.iter().map(|e| e.kind.as_str()).collect();
        assert!(
            kinds.contains(&"prompt") && kinds.contains(&"urgent-prompt"),
            "one captain entry plus one urgent record, got {:?}",
            m.wake.digests
        );
        assert!(
            m.wake.digests.iter().all(|e| e.pin == "BlueRidge"),
            "no routine/benign entry may enter the digest: {:?}",
            m.wake.digests
        );
        assert!(
            m.wake.digests[0].detail.contains("job"),
            "digest entry is the job wake, got {}",
            m.wake.digests[0].detail
        );
        assert!(
            m.wake_counters.get("routine").copied().unwrap_or(0) >= 1,
            "routine ticks must be counted, got {:?}",
            m.wake_counters
        );
        assert!(
            m.wake_counters.get("benign").copied().unwrap_or(0) >= 1,
            "benign ticks must be counted, got {:?}",
            m.wake_counters
        );
    }

    #[test]
    fn edge_triage_routine_advances_markers_without_prompt_or_digest() {
        let m = edge_model("BlueRidge");
        let line = edge_watch(
            9,
            "r1",
            r#"[["p","BlueRidge"],["h","ch-1"]]"#,
            "@BlueRidge hi",
        );
        let (next, cmds) = tick(&m, &Msg::WatchNdjson { line });
        assert!(
            cmds.iter().all(|c| !matches!(c, Cmd::Prompt { .. })),
            "routine wakes spend zero prompts: {cmds:?}"
        );
        assert!(
            next.wake.digests.is_empty(),
            "routine never enters the digest: {:?}",
            next.wake.digests
        );
        assert_eq!(
            next.wake_counters.get("routine"),
            Some(&1),
            "routine tick counted once: {:?}",
            next.wake_counters
        );
        assert_eq!(next.seq, 1);
    }

    #[test]
    fn edge_triage_captain_queues_digest_and_batch_emits() {
        let m = edge_model("BlueRidge");
        let line = edge_watch(43001, "c1", r#"[["p","BlueRidge"]]"#, "job ready");
        let (m1, cmds) = tick(&m, &Msg::WatchNdjson { line });
        assert!(
            cmds.iter().all(|c| !matches!(c, Cmd::Prompt { .. })),
            "captain-relevant queues, never injects immediately: {cmds:?}"
        );
        assert_eq!(
            m1.wake.digests.len(),
            1,
            "job wake queued for digest: {:?}",
            m1.wake.digests
        );
        assert_eq!(m1.wake.digests[0].pin, "BlueRidge");
        let (m2, cmds2) = tick(&m1, &Msg::Tick { now: 1_000_000 });
        assert!(
            cmds2
                .iter()
                .any(|c| matches!(c, Cmd::Digest { entries } if entries.len() == 1)),
            "digest batch must emit the captain entry: {cmds2:?}"
        );
        assert!(m2.wake.digests.is_empty(), "batch drains the queue");
    }

    #[test]
    fn edge_triage_ladder_line_is_urgent_and_injects() {
        let line = doorbell_mail_line(1, "BlueRidge", "c-1");
        assert!(
            is_urgent_line(&line),
            "re-ring is the ack-SLA vehicle and must ring now: {line}"
        );
        assert_eq!(
            classify_cmd(&Cmd::Prompt {
                pin: "BlueRidge".into(),
                line: line.clone(),
                event_id: None,
            }),
            WakeClass::Urgent,
            "re-ring prompts bypass batching"
        );
    }
}

#[cfg(test)]
mod gs048_reclassify_tests {
    use super::*;

    fn idle_captain() -> WorkspaceModel {
        let mut m = WorkspaceModel::default();
        m.roles.insert("captain".into(), "cap".into());
        m.presence.insert(
            "cap".into(),
            Presence::Idle {
                source: "presence".into(),
            },
        );
        m.doorbell.index.insert_pin("cap".into(), None);
        m
    }

    #[test]
    fn sticky_open_holds_without_explicit_topology() {
        let (t, held) = sticky_open_topology(Some("open"), "swarm", None);
        assert_eq!(t, "open");
        assert!(held);
        let (t, held) = sticky_open_topology(Some("open"), "loop", None);
        assert_eq!(t, "open");
        assert!(held);
        let (t, held) = sticky_open_topology(Some("open"), "join", None);
        assert_eq!(t, "open");
        assert!(held);
        let (t, held) = sticky_open_topology(Some("open"), "none", None);
        assert_eq!(t, "open");
        assert!(held, "catalog miss must not wipe open→none");
        let (t, held) = sticky_open_topology(Some("open"), "swarm", Some("swarm"));
        assert_eq!(t, "swarm");
        assert!(!held);
        let (t, held) = sticky_open_topology(None, "swarm", None);
        assert_eq!(t, "swarm");
        assert!(!held);
        let (t, held) = sticky_open_topology(Some("loop"), "swarm", None);
        assert_eq!(t, "loop");
        assert!(held);
        let (t, held) = sticky_open_topology(Some("loop"), "swarm", Some("swarm"));
        assert_eq!(t, "swarm");
        assert!(!held);
    }

    #[test]
    fn catalog_miss_does_not_invent_flip_open_to_none() {
        let mut m = idle_captain();
        m.session_topology = Some("open".into());
        let (next, _cmds) = tick(
            &m,
            &Msg::DecisionReady {
                goal: "obscure gibberish xyzzy".into(),
                primitive: "none".into(),
                topology: "none".into(),
                argv: vec![],
                decide_reason: "catalog miss".into(),
                explicit_topology: None,
            },
        );
        assert_eq!(
            next.session_topology.as_deref(),
            Some("open"),
            "catalog miss must hold sticky-open, not invent-flip to none"
        );
        assert_eq!(next.last_vehicle.as_deref(), Some("none: raw"));
    }

    #[test]
    fn vehicle_announcement_none_raw() {
        assert_eq!(vehicle_announcement("none", "open"), "none: raw");
        assert_eq!(vehicle_announcement("", "open"), "none: raw");
        assert_eq!(
            vehicle_announcement("wave-swarm", "swarm"),
            "vehicle: wave-swarm (topology=swarm)"
        );
    }

    #[test]
    fn material_context_emits_reclassify_cmd_not_decide_io() {
        let m = idle_captain();
        let (next, cmds) = tick(
            &m,
            &Msg::MaterialContext {
                goal: "ship the mid-session reclassify".into(),
                explicit_topology: None,
                reason: "authoritative-objective".into(),
            },
        );
        assert!(
            cmds.iter().any(|c| matches!(
                c,
                Cmd::Reclassify {
                    goal,
                    reason,
                    ..
                } if goal.contains("mid-session") && reason == "authoritative-objective"
            )),
            "expected Cmd::Reclassify: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "material tick must not Prompt before DecisionReady: {cmds:?}"
        );
        assert_eq!(
            next.session_topology, None,
            "topology waits for DecisionReady"
        );
    }

    #[test]
    fn decision_ready_announces_vehicle_and_sticky_open() {
        let mut m = idle_captain();
        m.session_topology = Some("open".into());
        let (next, cmds) = tick(
            &m,
            &Msg::DecisionReady {
                goal: "fan out a swarm".into(),
                primitive: "wave-swarm".into(),
                topology: "swarm".into(),
                argv: vec!["workflow".into(), "compose".into()],
                decide_reason: "matched wave-swarm".into(),
                explicit_topology: None,
            },
        );
        assert_eq!(next.session_topology.as_deref(), Some("open"));
        assert_eq!(next.last_vehicle.as_deref(), Some("none: raw"));
        let prompts: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                Cmd::Prompt { line, .. } => Some(line.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            prompts.iter().any(|l| l.starts_with("[reclassify]")
                && l.contains("none: raw")
                && l.contains("sticky-open held")),
            "expected sticky announce: {prompts:?}"
        );
        assert!(
            prompts.iter().all(|l| !l.contains("wave-swarm")),
            "announce must not name invent wave-swarm under open: {prompts:?}"
        );
        assert!(
            prompts.iter().all(|l| is_urgent_line(l)),
            "reclassify announce must be urgent/deliverable: {prompts:?}"
        );
    }

    /// GS-049 airtight: loop sticky vs invent swarm announce — no wave-swarm name.
    #[test]
    fn decision_ready_loop_sticky_vs_swarm_announces_loop_vehicle() {
        let mut m = idle_captain();
        m.session_topology = Some("loop".into());
        let (next, cmds) = tick(
            &m,
            &Msg::DecisionReady {
                goal: "fan out a swarm".into(),
                primitive: "wave-swarm".into(),
                topology: "swarm".into(),
                argv: vec!["workflow".into(), "compose".into()],
                decide_reason: "matched wave-swarm".into(),
                explicit_topology: None,
            },
        );
        assert_eq!(next.session_topology.as_deref(), Some("loop"));
        assert_eq!(
            next.last_vehicle.as_deref(),
            Some("vehicle: loop-work (topology=loop)")
        );
        let prompts: Vec<_> = cmds
            .iter()
            .filter_map(|c| match c {
                Cmd::Prompt { line, .. } => Some(line.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            prompts
                .iter()
                .any(|l| l.contains("vehicle: loop-work (topology=loop)")
                    && l.contains("sticky-open held")),
            "{prompts:?}"
        );
        assert!(
            prompts.iter().all(|l| !l.contains("wave-swarm")),
            "must not name invent wave-swarm under held loop: {prompts:?}"
        );
    }

    #[test]
    fn sticky_held_vehicle_announcement_open_is_none_raw() {
        assert_eq!(
            sticky_held_vehicle_announcement("wave-swarm", "open"),
            "none: raw"
        );
        assert_eq!(
            sticky_held_vehicle_announcement("join", "open"),
            "none: raw"
        );
        assert_eq!(
            sticky_held_vehicle_announcement("wave-swarm", "loop"),
            "vehicle: loop-work (topology=loop)"
        );
    }

    #[test]
    fn decision_ready_explicit_swarm_overrides_sticky() {
        let mut m = idle_captain();
        m.session_topology = Some("open".into());
        let (next, cmds) = tick(
            &m,
            &Msg::DecisionReady {
                goal: "please swarm this".into(),
                primitive: "wave-swarm".into(),
                topology: "swarm".into(),
                argv: vec![],
                decide_reason: "explicit".into(),
                explicit_topology: Some("swarm".into()),
            },
        );
        assert_eq!(next.session_topology.as_deref(), Some("swarm"));
        let line = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::Prompt { line, .. } => Some(line.clone()),
                _ => None,
            })
            .expect("announce");
        assert!(line.contains("topology=swarm"), "{line}");
        assert!(!line.contains("sticky-open held"), "{line}");
    }

    #[test]
    fn routine_channel_wake_stays_prompt_only_no_reclassify() {
        let m = idle_captain();
        let line = r#"{"kind":9,"id":"ab","tags":[["p","cap"],["h","ch-1"]],"content":"@cap hi"}"#;
        let (_next, cmds) = tick(&m, &Msg::WatchNdjson { line: line.into() });
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Reclassify { .. })),
            "routine wake must not emit Reclassify: {cmds:?}"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::Prompt { line, .. } if line.starts_with("[reclassify]"))),
            "routine wake must not announce reclassify: {cmds:?}"
        );
    }

    /// GS-048: Job (kind 43001) and reservation (kind 30031) wakes stay
    /// Prompt-only — same golden posture as Channel (kind 9).
    #[test]
    fn routine_job_and_kind30031_wake_stays_prompt_only_no_reclassify() {
        let m = idle_captain();
        for (kind, id, content) in [
            (43001u16, "job1", "job ready"),
            (30031u16, "rsv1", "reservation conflict"),
        ] {
            let line = format!(
                r#"{{"kind":{kind},"id":"{id}","tags":[["p","cap"],["h","ch-1"]],"content":"{content}"}}"#
            );
            let (_next, cmds) = tick(&m, &Msg::WatchNdjson { line });
            assert!(
                !cmds.iter().any(|c| matches!(c, Cmd::Reclassify { .. })),
                "kind {kind} must not emit Reclassify: {cmds:?}"
            );
            assert!(
                !cmds.iter().any(|c| matches!(
                    c,
                    Cmd::Prompt { line, .. } if line.starts_with("[reclassify]")
                )),
                "kind {kind} must not announce reclassify: {cmds:?}"
            );
        }
    }

    #[test]
    fn material_fact_reason_detects_authoritative_and_skips_chatter() {
        assert_eq!(
            material_context_reason("AUTHORITATIVE OPERATOR OBJECTIVE: ship it"),
            Some("authoritative-objective")
        );
        assert_eq!(
            material_context_reason("please switch to swarm topology"),
            Some("explicit-topology-request")
        );
        assert_eq!(
            parse_explicit_topology("please switch to swarm topology").as_deref(),
            Some("swarm")
        );
        assert!(material_context_reason("@cap hi from channel").is_none());
        assert!(material_context_reason("kind 9 pull hint").is_none());
    }

    #[test]
    fn same_log_same_effects_material_and_decision_ready() {
        let m = idle_captain();
        let msg = Msg::MaterialContext {
            goal: "goal".into(),
            explicit_topology: None,
            reason: "new-work-unit".into(),
        };
        let (m1, c1) = tick(&m, &msg);
        let (m2, c2) = tick(&m, &msg);
        assert_eq!(c1, c2);
        assert_eq!(m1.seq, m2.seq);

        let ready = Msg::DecisionReady {
            goal: "goal".into(),
            primitive: "none".into(),
            topology: "none".into(),
            argv: vec![],
            decide_reason: "catalog miss".into(),
            explicit_topology: None,
        };
        let (r1, d1) = tick(&m, &ready);
        let (r2, d2) = tick(&m, &ready);
        assert_eq!(d1, d2);
        assert_eq!(r1.last_vehicle, r2.last_vehicle);
        assert_eq!(r1.last_vehicle.as_deref(), Some("none: raw"));
    }

    /// bd-gkwu: the reclassify effect key is a pure derivation of
    /// (goal, reason, topology, pin) — retries attach to the same journal
    /// primary key (Law 8: a retry is another attempt, not a new action).
    /// Authority rides the caller (threaded `&RuntimeAuthority`), never a
    /// mid-path resolve: same derivation ⇒ same key under any authority.
    #[test]
    fn authority_edge_reclassify_key_stable_per_derivation() {
        let k1 = reclassify_effect_key("goal-a", "why", None, "pin-x");
        let k2 = reclassify_effect_key("goal-a", "why", None, "pin-x");
        assert_eq!(k1, k2, "same derivation must journal under one key");
        assert_ne!(
            k1,
            reclassify_effect_key("goal-a", "other-why", None, "pin-x"),
            "a new reason is a new derivation"
        );
        assert_ne!(
            k1,
            reclassify_effect_key("goal-a", "why", None, "pin-y"),
            "a new pin is a new derivation"
        );
    }

    #[test]
    fn expand_reclassify_cmds_runs_decide_outside_tick() {
        let mut m = idle_captain();
        m.session_topology = Some("open".into());
        let (_, cmds) = tick(
            &m,
            &Msg::MaterialContext {
                goal: "compose a wave swarm for the lanes".into(),
                explicit_topology: None,
                reason: "explicit-topology-request".into(),
            },
        );
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Reclassify { .. })));
        let _home = crate::config::test_state_home();
        let expanded = expand_reclassify_cmds(
            &mut m,
            cmds,
            "gs050-expand-test",
            &crate::authority::RuntimeAuthority::resolve(),
            &|goal| {
                let d = crate::decide::decide(goal, &crate::decide::Snapshot::default());
                (
                    d.primitive.to_string(),
                    d.topology.to_string(),
                    d.argv.clone(),
                    d.reason.clone(),
                )
            },
        );
        assert!(
            !expanded.iter().any(|c| matches!(c, Cmd::Reclassify { .. })),
            "expand must consume Reclassify: {expanded:?}"
        );
        assert!(
            expanded
                .iter()
                .any(|c| matches!(c, Cmd::Prompt { line, .. } if line.starts_with("[reclassify]"))),
            "expand must produce announce Prompt: {expanded:?}"
        );
        assert_eq!(m.session_topology.as_deref(), Some("open"));
    }

    /// bd-lpn6: every reclassify expand appends one miss-ledger row keyed by
    /// the effect key — the mining surface for new routing needles.
    #[test]
    fn expand_appends_miss_ledger_row() {
        let mut m = idle_captain();
        m.session_topology = Some("open".into());
        let (_, cmds) = tick(
            &m,
            &Msg::MaterialContext {
                goal: "obscure ledger probe xyzzy".into(),
                explicit_topology: None,
                reason: "test-ledger".into(),
            },
        );
        assert!(cmds.iter().any(|c| matches!(c, Cmd::Reclassify { .. })));
        let _home = crate::config::test_state_home();
        let _ = expand_reclassify_cmds(
            &mut m,
            cmds,
            "gs050-ledger-test",
            &crate::authority::RuntimeAuthority::resolve(),
            &|goal| {
                let d = crate::decide::decide(goal, &crate::decide::Snapshot::default());
                (
                    d.primitive.to_string(),
                    d.topology.to_string(),
                    d.argv.clone(),
                    d.reason.clone(),
                )
            },
        );
        let rows = crate::judgment::miss_read();
        assert!(
            rows.iter().any(|r| r.goal.contains("ledger probe")),
            "expand appends a row for the goal: {rows:?}"
        );
    }

    /// GS-049 / GS-045 P1-1 (TURN-INV / ADR-0032 I8a): identical source Msg
    /// yields identical Cmds (same effect identity). Honesty: Rust `&Msg`
    /// borrow already prevents mutation — the real invariant is same-log →
    /// same Cmds, not an `&Msg` tautology.
    #[test]
    fn source_msg_immutable_same_log_same_effects_watch_ndjson() {
        let m = idle_captain();
        let line = r#"{"id":"evt-1","kind":9,"content":"hello"}"#.to_string();
        let msg = Msg::WatchNdjson { line: line.clone() };
        let (m1, c1) = tick(&m, &msg);
        let (m2, c2) = tick(&m, &msg);
        assert_eq!(c1, c2, "same source Msg must keep the same effect Cmds");
        assert_eq!(m1.seq, m2.seq);
        let _ = line;
        let (_, ca) = tick(
            &m,
            &Msg::MaterialContext {
                goal: "goal-A".into(),
                explicit_topology: None,
                reason: "authoritative-objective".into(),
            },
        );
        let (_, cb) = tick(
            &m,
            &Msg::MaterialContext {
                goal: "goal-B".into(),
                explicit_topology: None,
                reason: "authoritative-objective".into(),
            },
        );
        assert_ne!(
            ca, cb,
            "different MaterialContext goals must not collapse to same Cmds"
        );
    }

    /// GS-050 / GS-049 / GS-045 P1-1: MaterialContext goal/reason (+ pin /
    /// explicit) is the logical source-action identity; `effect_key` is the
    /// journal spelling of that identity (ADR-0032 I8a). Retries keep the
    /// same key — never mint a fresh one per attempt.
    #[test]
    fn source_action_reclassify_goal_stable_across_retries() {
        let m = idle_captain();
        let goal = "AUTHORITATIVE OPERATOR OBJECTIVE: ship residuals".to_string();
        let msg = Msg::MaterialContext {
            goal: goal.clone(),
            explicit_topology: None,
            reason: "authoritative-objective".into(),
        };
        let (_n1, cmds1) = tick(&m, &msg);
        let (_n2, cmds2) = tick(&m, &msg);
        assert_eq!(cmds1, cmds2);
        let keys: Vec<_> = cmds1
            .iter()
            .filter_map(|c| match c {
                Cmd::Reclassify {
                    goal,
                    reason,
                    pin,
                    explicit_topology,
                    effect_key,
                } => Some((
                    goal.clone(),
                    reason.clone(),
                    pin.clone(),
                    explicit_topology.clone(),
                    effect_key.clone(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(keys.len(), 1, "one Reclassify Cmd: {cmds1:?}");
        assert_eq!(keys[0].0, goal);
        assert_eq!(keys[0].1, "authoritative-objective");
        assert!(keys[0].3.is_none());
        let expected = reclassify_effect_key(&goal, "authoritative-objective", None, &keys[0].2);
        assert_eq!(
            keys[0].4, expected,
            "effect_key must derive from logical identity"
        );
        assert!(
            keys[0].4.starts_with("reclassify:"),
            "effect_key prefix: {}",
            keys[0].4
        );
        let keys2: Vec<_> = cmds2
            .iter()
            .filter_map(|c| match c {
                Cmd::Reclassify {
                    goal,
                    reason,
                    pin,
                    explicit_topology,
                    effect_key,
                } => Some((
                    goal.clone(),
                    reason.clone(),
                    pin.clone(),
                    explicit_topology.clone(),
                    effect_key.clone(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(keys, keys2);
        assert_eq!(keys[0].4, keys2[0].4, "retries keep the same effect_key");
    }

    /// I8a: post-effect retry — DecisionReady fold then re-tick the same
    /// MaterialContext; logical identity and journal `effect_key` unchanged.
    #[test]
    fn source_action_reclassify_post_effect_retry_keeps_identity() {
        let mut m = idle_captain();
        m.session_topology = Some("open".into());
        let goal = "AUTHORITATIVE OPERATOR OBJECTIVE: deepen elm".to_string();
        let msg = Msg::MaterialContext {
            goal: goal.clone(),
            explicit_topology: None,
            reason: "authoritative-objective".into(),
        };
        let (m1, cmds1) = tick(&m, &msg);
        let reclass: Vec<_> = cmds1
            .iter()
            .filter_map(|c| match c {
                Cmd::Reclassify {
                    goal,
                    reason,
                    pin,
                    explicit_topology,
                    effect_key,
                } => Some((
                    goal.clone(),
                    reason.clone(),
                    pin.clone(),
                    explicit_topology.clone(),
                    effect_key.clone(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(reclass.len(), 1);
        let key1 = reclass[0].4.clone();
        assert_eq!(
            key1,
            reclassify_effect_key(&goal, "authoritative-objective", None, &reclass[0].2)
        );
        let ready = Msg::DecisionReady {
            goal: reclass[0].0.clone(),
            primitive: "babysit-bead".into(),
            topology: "swarm".into(),
            argv: vec![
                "flywheel".into(),
                "workflow".into(),
                "compose".into(),
                "wave-swarm".into(),
            ],
            decide_reason: "test-swarm".into(),
            explicit_topology: reclass[0].3.clone(),
        };
        let (m2, cmds_ready) = tick(&m1, &ready);
        assert_eq!(
            m2.session_topology.as_deref(),
            Some("open"),
            "sticky-open holds against swarm invent flip"
        );
        assert!(
            cmds_ready
                .iter()
                .any(|c| matches!(c, Cmd::Prompt { line, .. } if line.contains("sticky-open held") || line.starts_with("[reclassify]"))),
            "DecisionReady must announce: {cmds_ready:?}"
        );
        let (_m3, cmds3) = tick(&m2, &msg);
        let reclass3: Vec<_> = cmds3
            .iter()
            .filter_map(|c| match c {
                Cmd::Reclassify {
                    goal,
                    reason,
                    explicit_topology,
                    effect_key,
                    ..
                } => Some((
                    goal.clone(),
                    reason.clone(),
                    explicit_topology.clone(),
                    effect_key.clone(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            reclass3,
            vec![(goal, "authoritative-objective".into(), None, key1.clone(),)],
            "post-effect retry must keep same source-action identity + effect_key"
        );
        assert_eq!(reclass3[0].3, key1, "retries keep the same effect_key");
    }

    /// Distinct logical reclassify identities must not collide on effect_key.
    #[test]
    fn reclassify_effect_key_distinct_for_distinct_goals() {
        let a = reclassify_effect_key("goal-A", "authoritative-objective", None, "cap");
        let b = reclassify_effect_key("goal-B", "authoritative-objective", None, "cap");
        let c = reclassify_effect_key("goal-A", "authoritative-objective", Some("swarm"), "cap");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            a,
            reclassify_effect_key("goal-A", "authoritative-objective", None, "cap"),
            "derivation is pure / stable"
        );
    }

    /// GS-050: None ≠ Some("") under length-prefixed effect_key encode.
    #[test]
    fn reclassify_effect_key_none_ne_some_empty_string() {
        let a = reclassify_effect_key("g", "r", None, "pin");
        let b = reclassify_effect_key("g", "r", Some(""), "pin");
        assert_ne!(a, b);
        let c = reclassify_effect_key("ab", "c", None, "pin");
        let d = reclassify_effect_key("a", "bc", None, "pin");
        assert_ne!(c, d);
    }

    /// GS-050: expand journals Prepared→InFlight→Settled under Cmd effect_key
    /// (durable I8a identity — not Cmd/stderr theater). Retries share PK.
    #[test]
    fn expand_reclassify_journals_effect_key_primary_key() {
        let _home = crate::config::test_state_home();
        let project = "gs050-reclassify-expand-journal";
        let mut m = idle_captain();
        let goal = "AUTHORITATIVE: journal the key".to_string();
        let (_, cmds) = tick(
            &m,
            &Msg::MaterialContext {
                goal: goal.clone(),
                explicit_topology: None,
                reason: "authoritative-objective".into(),
            },
        );
        let key = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::Reclassify { effect_key, .. } => Some(effect_key.clone()),
                _ => None,
            })
            .expect("Reclassify carries effect_key");
        let _ = expand_reclassify_cmds(
            &mut m,
            cmds,
            project,
            &crate::authority::RuntimeAuthority::resolve(),
            &|g| ("none".into(), "none".into(), vec![], format!("decided:{g}")),
        );
        let log = std::fs::read_to_string(crate::state::project_log_path(project))
            .expect("reclassify journal written");
        assert!(log.contains("\"journal\":\"reclassify\""), "{log}");
        assert!(
            log.contains(&key),
            "journal must carry Cmd effect_key: {log}"
        );
        for state in ["prepared", "in_flight", "settled"] {
            assert!(
                log.contains(&format!("\"state\":\"{state}\"")),
                "{state} missing: {log}"
            );
        }
        let mut m2 = idle_captain();
        let (_, cmds2) = tick(
            &m2,
            &Msg::MaterialContext {
                goal: goal.clone(),
                explicit_topology: None,
                reason: "authoritative-objective".into(),
            },
        );
        let key2 = cmds2
            .iter()
            .find_map(|c| match c {
                Cmd::Reclassify { effect_key, .. } => Some(effect_key.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(key, key2);
        let _ = expand_reclassify_cmds(
            &mut m2,
            cmds2,
            project,
            &crate::authority::RuntimeAuthority::resolve(),
            &|_| ("none".into(), "none".into(), vec![], "retry".into()),
        );
        let loaded = crate::agent_ref::load_reclassify_by_key(project, &key)
            .expect("load")
            .expect("row");
        assert_eq!(loaded.effect_key, key);
        assert!(
            loaded.attempt >= 1,
            "idempotent resume bumps attempt: {:?}",
            loaded.attempt
        );
    }
}

use toron_core::kernel::{
    Boot, Cmd as CmdTrait, Durability, Effect, Failure, Interpret, Program, Update,
};

/// Failure re-entry no real pane ever carries: heal_states is keyed by
/// pane name, so this emits no commands and touches no domain state on any
/// live model (only the ambient per-message bookkeeping every tick does).
pub const SHADOW_FAILURE_NAME: &str = "__shadow_failure__";

/// Stable content hash for durable intent keys (std hashes are randomly
/// seeded per process; journal keys must survive a restart).
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl CmdTrait for Cmd {
    fn durability(&self) -> Durability {
        match self {
            Cmd::FetchInbox
            | Cmd::ReadToronCursor { .. }
            | Cmd::CheckAckSla
            | Cmd::RestartWatch
            | Cmd::FactMiss { .. }
            | Cmd::Audit { .. }
            | Cmd::WaitCorr { .. }
            | Cmd::Wait
            | Cmd::LoudMiss { .. } => Durability::Ephemeral,
            Cmd::Digest { entries } => Durability::Durable {
                key: format!("digest:{:x}", fnv1a64(format!("{entries:?}").as_bytes())),
            },
            Cmd::Prompt { pin, line, .. } => Durability::Durable {
                key: format!("prompt:{pin}:{:x}", fnv1a64(line.as_bytes())),
            },
            Cmd::Send { name, text } => Durability::Durable {
                key: format!("send:{name}:{:x}", fnv1a64(text.as_bytes())),
            },
            Cmd::DispatchImmediate { name, text } => Durability::Durable {
                key: format!("dispatch:{name}:{:x}", fnv1a64(text.as_bytes())),
            },
            Cmd::PinStamp { bead_id, pin } => Durability::Durable {
                key: format!("pinstamp:{bead_id}:{pin}"),
            },
            Cmd::Blocked { name } => Durability::Durable {
                key: format!("blocked:{name}"),
            },
            Cmd::Nudge { name } => Durability::Durable {
                key: format!("nudge:{name}"),
            },
            Cmd::Diagnose { name, reason } => Durability::Durable {
                key: format!("diagnose:{name}:{:x}", fnv1a64(reason.as_bytes())),
            },
            Cmd::Respawn { name, reason } => Durability::Durable {
                key: format!("respawn:{name}:{:x}", fnv1a64(reason.as_bytes())),
            },
            Cmd::Reclaim {
                bead_id,
                pin,
                terminal_name,
                ..
            } => Durability::Durable {
                key: format!("reclaim:{bead_id}:{pin}:{terminal_name}"),
            },
            Cmd::SqliteUpsert { corr, .. } => Durability::Durable {
                key: format!("upsert:{corr}"),
            },
            Cmd::Publish { corr, .. } => Durability::Durable {
                key: format!("publish:{corr}"),
            },
            Cmd::Invoke { corr, .. } => Durability::Durable {
                key: format!("invoke:{corr}"),
            },
            Cmd::RebindBinding { effect_key, .. } => Durability::Durable {
                key: effect_key.clone(),
            },
            Cmd::Reclassify { effect_key, .. } => Durability::Durable {
                key: effect_key.clone(),
            },
        }
    }
}

pub struct WorkspaceProgram;

impl Program for WorkspaceProgram {
    type Model = WorkspaceModel;
    type Msg = Msg;
    type Cmd = Cmd;
    type Err = String;

    fn init(_boot: &Boot) -> Update<WorkspaceModel, Cmd> {
        Update::quiet(WorkspaceModel::default())
    }

    fn update(model: &WorkspaceModel, msg: &Msg) -> Update<WorkspaceModel, Cmd> {
        let (model, commands) = tick(model, msg);
        Update { model, commands }
    }

    fn failure(failure: &Failure<String>) -> Msg {
        let _ = failure;
        Msg::DiagnoseFailed {
            name: SHADOW_FAILURE_NAME.into(),
        }
    }
}

/// Shadow interpreter: completes every command as the quiet sentinel.
/// Outcomes are replayed, never performed — no services, no I/O.
#[cfg_attr(not(test), allow(dead_code))]
pub struct ShadowInterpret;

#[cfg_attr(not(test), allow(dead_code))]
impl Interpret<WorkspaceProgram> for ShadowInterpret {
    fn interpret(_cmd: &Cmd) -> Effect<Msg, String> {
        Effect::succeed(Msg::DiagnoseFailed {
            name: SHADOW_FAILURE_NAME.into(),
        })
    }
}

/// Production shadow: pure fold + compare. No I/O, no clock, no Loop —
/// the caller logs what drain_divergences returns. The Loop-boot half of
/// T2 is proven in tests; this is the zero-observable-cost half.
#[cfg_attr(not(test), allow(dead_code))]
pub struct ShadowDriver {
    divergences: Vec<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ShadowDriver {
    pub fn new() -> Self {
        Self {
            divergences: Vec::new(),
        }
    }

    /// Fold msg through the Program and compare with the live outcome.
    /// Pure: records, never logs.
    pub fn observe(
        &mut self,
        model: &WorkspaceModel,
        msg: &Msg,
        live_next: &WorkspaceModel,
        live_cmds: &[Cmd],
    ) -> bool {
        let out = WorkspaceProgram::update(model, msg);
        if out.model == *live_next && out.commands == *live_cmds {
            true
        } else {
            self.divergences.push(format!(
                "shadow divergence on {msg:?}: live {} cmds vs shadow {} cmds",
                live_cmds.len(),
                out.commands.len()
            ));
            false
        }
    }

    pub fn drain_divergences(&mut self) -> Vec<String> {
        std::mem::take(&mut self.divergences)
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl Default for ShadowDriver {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod shadow_tests {
    use super::*;
    use std::sync::Arc;
    use toron_core::kernel::{
        AppendError, Defect, DurableLog, Exit, FileLog, Layer, Loop, ManagedRuntime, Producer,
        Runtime, ServiceTag,
    };

    #[derive(Clone)]
    struct NoopSvc;
    struct NoopTag;
    impl ServiceTag for NoopTag {
        type Service = NoopSvc;
        const ID: &'static str = "workspace-shadow-test-noop";
    }

    fn test_runtime() -> ManagedRuntime {
        let layer = Layer::<NoopTag, std::convert::Infallible, ()>::from_value(NoopSvc);
        ManagedRuntime::make(&layer).expect("value layer builds")
    }

    fn msg_battery() -> Vec<Msg> {
        vec![
            Msg::Tick { now: 1 },
            Msg::Tick { now: 1_000_000 },
            Msg::Sweep { now: 7 },
            Msg::AckSlaTick { now: 9 },
            Msg::WatchDied,
            Msg::DiagnoseFailed {
                name: "pane".into(),
            },
            Msg::WatchNdjson {
                line: "not json".into(),
            },
            Msg::InboxItems {
                raw: "blob".into(),
                pin: "Pin".into(),
            },
            Msg::InboxRows {
                rows: vec![],
                pin: "Pin".into(),
            },
            Msg::PlaneStatus {
                pin: "P".into(),
                status: "idle".into(),
            },
            Msg::Fact {
                summary: "s".into(),
            },
            Msg::SlaFired {
                posts: vec!["c1".into()],
            },
            Msg::Reclaim {
                bead_id: "bd-x".into(),
                holder_pin: "H".into(),
                claimant_pin: "C".into(),
                is_live: false,
                reason: "test".into(),
            },
            Msg::Heal {
                name: "N".into(),
                status: "dead".into(),
                exited: true,
            },
            Msg::ClaimNudge {
                name: "N".into(),
                bead_id: "bd-x".into(),
            },
            Msg::CloseNudge {
                name: "N".into(),
                bead_id: "bd-x".into(),
                title: "t".into(),
            },
            Msg::Receipt {
                corr: "k".into(),
                receipt: Receipt::default(),
            },
            Msg::RoleBound {
                role: "captain".into(),
                pin: "P".into(),
            },
            Msg::Presence {
                pin: "P".into(),
                state: "bogus".into(),
            },
            Msg::HumanDecision {
                corr: "nope".into(),
                verdict: "approve".into(),
            },
            Msg::MaterialContext {
                goal: "g".into(),
                explicit_topology: None,
                reason: "r".into(),
            },
            Msg::DecisionReady {
                goal: "g".into(),
                primitive: "none".into(),
                topology: "open".into(),
                argv: vec![],
                decide_reason: "d".into(),
                explicit_topology: None,
            },
            Msg::Hc {
                event: crate::durable::PollEvent {
                    event_type: "status".into(),
                    pane_id: "w1:p1".into(),
                    agent: None,
                    status: Some("idle".into()),
                    seq: 1,
                },
            },
        ]
    }

    #[test]
    fn workspace_program_fold_matches_live_tick() {
        let seeded = tick(&WorkspaceModel::default(), &Msg::Tick { now: 1 }).0;
        for model in [WorkspaceModel::default(), seeded] {
            for msg in msg_battery() {
                let live = tick(&model, &msg);
                let shadow = WorkspaceProgram::update(&model, &msg);
                assert_eq!(
                    (shadow.model, shadow.commands),
                    live,
                    "divergence on {msg:?}"
                );
            }
        }
    }

    #[test]
    fn init_is_quiet_default() {
        let out = WorkspaceProgram::init(&Boot::default());
        assert_eq!(out.model, WorkspaceModel::default());
        assert!(out.commands.is_empty());
    }

    /// Every tick bumps ambient per-message bookkeeping (seq, wake
    /// counters) even when it changes no domain state. Mask those before
    /// comparing, so the tests pin domain quiet, not bookkeeping.
    fn without_ambient(m: &WorkspaceModel) -> WorkspaceModel {
        let mut m = m.clone();
        m.seq = 0;
        m.wake_counters.clear();
        m
    }

    #[test]
    fn failure_names_quiet_outcome_for_every_cause_kind() {
        let failures: Vec<Failure<String>> = vec![
            Failure::Fail("boom".into()),
            Failure::Defect(Defect::new("defect")),
            Failure::Interrupted,
            Failure::NotAppended(AppendError::PartialIdentity),
        ];
        let mut model = WorkspaceModel::default();
        model.swarm.heal_states.insert(
            "live-pane".into(),
            crate::durable::HealState {
                awaiting_clear: true,
                ..Default::default()
            },
        );
        for f in &failures {
            let msg = WorkspaceProgram::failure(f);
            let (next, cmds) = tick(&model, &msg);
            assert!(cmds.is_empty(), "failure outcome must emit nothing");
            assert_eq!(
                without_ambient(&next),
                without_ambient(&model),
                "failure outcome must touch no domain state"
            );
        }
    }

    #[test]
    fn durability_keys_are_stable_and_variant_scoped() {
        let a = Cmd::Send {
            name: "n".into(),
            text: "t".into(),
        };
        let b = Cmd::Send {
            name: "n".into(),
            text: "t".into(),
        };
        let c = Cmd::Send {
            name: "n".into(),
            text: "other".into(),
        };
        let key = |c: &Cmd| match c.durability() {
            Durability::Durable { key } => key,
            Durability::Ephemeral => "ephemeral".into(),
        };
        assert_eq!(key(&a), key(&b));
        assert_ne!(key(&a), key(&c));
        assert_ne!(
            key(&a),
            key(&Cmd::DispatchImmediate {
                name: "n".into(),
                text: "t".into()
            })
        );
        for ephemeral in [
            Cmd::FetchInbox,
            Cmd::CheckAckSla,
            Cmd::RestartWatch,
            Cmd::Wait,
        ] {
            assert_eq!(ephemeral.durability(), Durability::Ephemeral);
        }
        let reclassify = Cmd::Reclassify {
            goal: "g".into(),
            pin: "p".into(),
            explicit_topology: None,
            reason: "r".into(),
            effect_key: "k".into(),
        };
        match reclassify.durability() {
            Durability::Durable { key } => assert_eq!(key, "k"),
            Durability::Ephemeral => panic!("reclassify is durable"),
        }
    }

    #[test]
    fn interpret_completes_commands_as_quiet_followup() {
        let cmd = Cmd::Send {
            name: "n".into(),
            text: "t".into(),
        };
        let exit = Runtime::run(ShadowInterpret::interpret(&cmd));
        assert!(exit.is_success());
        let Exit::Success(msg) = exit else {
            panic!("shadow interpret succeeds");
        };
        let (next, cmds) = tick(&WorkspaceModel::default(), &msg);
        assert_eq!(
            without_ambient(&next),
            without_ambient(&WorkspaceModel::default())
        );
        assert!(cmds.is_empty());
    }

    #[test]
    fn loop_boots_and_recovers_across_crash() {
        let dir = std::env::temp_dir().join(format!("flywheel-ws-shadow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("shadow.jsonl");
        let msgs = vec![
            Msg::Tick { now: 1 },
            Msg::Tick { now: 2 },
            Msg::Sweep { now: 3 },
        ];

        let rt = test_runtime();
        let log: Arc<dyn DurableLog> = Arc::new(FileLog::open(&path).expect("open log"));
        let mut loop_ = Loop::<WorkspaceProgram, ShadowInterpret>::boot(
            &Boot::default(),
            rt,
            log,
            Producer::new("ws-shadow-file"),
        )
        .expect("boot runs init");
        for msg in &msgs {
            loop_.send(msg.clone()).expect("send folds");
        }
        assert!(path.exists(), "the journal reached disk");
        drop(loop_);

        let rt = test_runtime();
        let log: Arc<dyn DurableLog> = Arc::new(FileLog::open(&path).expect("reopen log"));
        let recovered = Loop::<WorkspaceProgram, ShadowInterpret>::recover(
            &Boot::default(),
            rt,
            log,
            Producer::new("ws-shadow-file"),
        )
        .expect("recover replays the journal");
        let sentinel = Msg::DiagnoseFailed {
            name: SHADOW_FAILURE_NAME.into(),
        };
        let mut live = WorkspaceModel::default();
        for msg in &msgs {
            let (m, cmds) = tick(&live, msg);
            live = m;
            for _ in &cmds {
                let (m2, follow) = tick(&live, &sentinel);
                assert!(follow.is_empty());
                live = m2;
            }
        }
        assert_eq!(
            recovered.model(),
            &live,
            "recover converges to the live model"
        );
        assert!(recovered.in_flight().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shadow_driver_soak_reports_zero_divergence() {
        let mut driver = ShadowDriver::new();
        let mut model = WorkspaceModel::default();
        for (i, msg) in msg_battery().into_iter().enumerate() {
            let (next, cmds) = tick(&model, &msg);
            assert!(
                driver.observe(&model, &msg, &next, &cmds),
                "divergence on {msg:?}"
            );
            model = next;
            let _ = i;
        }
        for msg in msg_battery() {
            let (next, cmds) = tick(&model, &msg);
            assert!(
                driver.observe(&model, &msg, &next, &cmds),
                "divergence on {msg:?}"
            );
            model = next;
        }
        assert!(driver.drain_divergences().is_empty());
        let _ = model;
    }

    #[test]
    fn shadow_driver_records_divergence_instead_of_crashing() {
        let mut driver = ShadowDriver::new();
        let model = WorkspaceModel::default();
        let msg = Msg::Tick { now: 1 };
        let (next, _) = tick(&model, &msg);
        assert!(!driver.observe(&model, &msg, &next, &[Cmd::Wait]));
        assert_eq!(driver.drain_divergences().len(), 1);
    }
}

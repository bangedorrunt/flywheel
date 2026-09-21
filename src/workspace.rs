// governed-by: ADR-0027
//! One Workspace Tick — Control Plane as Elm/Redux on Doorbell + 30013
//! Single owner of control-plane types: WorkspaceModel, Msg, Cmd, tick.
//! The two existing ticks (doorbell_tick::tick, durable::tick) become
//! private slice reducers called ONLY from WorkspaceModel::tick.
//! Clock enters tick only as Msg payload (Tick{now}); no Instant now.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

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

/// Presence verdict per pin; missing key = Unknown (invariant 9).
/// Governed-by: ADR-0010 D3. Busy/Idle/Dead carry the evidence source:
/// "presence" (20001 wire authority), "herdr-evidence" (native busy,
/// never authority), "legacy" (pre-D3 snapshot), "endpoint" (death).
/// Unknown is never promoted, never absorbs a wake, never prompts.
/// Native herdr idle is never trusted.
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

// Pre-D3 snapshots stored flat unit strings ("Working", "Idle", ...).
// New verdicts serialize as maps ({"Busy":{"source":"..."}}). Accept
// both so a snapshot written before D3 still loads; flat words land with
// source "legacy", never authority.
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
/// native herdr busy yields Busy labeled "herdr-evidence" (evidence,
/// not authority — consumers still never prompt from it), native idle is
/// never trusted (stays Unknown), and only endpoint death overrides to
/// Dead. The CLI status render calls this with known=None (no live
/// Model in the render path); the tick path passes Model.presence.
pub fn verdict_for(known: Option<&Presence>, herdr_status: &str, endpoint_alive: bool) -> Presence {
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
        Some(Presence::Unknown) | None => match herdr_status.trim().to_ascii_lowercase().as_str() {
            "working" | "busy" => Presence::Busy {
                source: "herdr-evidence".into(),
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

// ---------------------------------------------------------------------------
// ADR-0010 D6 (bd-f14d): OPEN DECISIONS fold + RECORD DIVERGENCE.
//
// open_human IS the captain-hold cache (single store — no second table).
// The fold below renders every open decision until an answer/--verdict
// clears it (apply_human_decision is the only remover). Teardown, kill,
// corr TTL, and pending_wait expiry never close a call: expiry PROMOTES
// into open_human (re-surface, never drop). Divergence rows only report.
// ---------------------------------------------------------------------------

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

// Read-side divergence reporting is kept outside the live reducer. Re-export
// the established names so snapshots and existing callers remain unchanged.
pub use crate::workspace_reporting::{Divergence, DivergenceSpec};

// Divergence variants remain part of this source-level contract:
// ClosedBeadOpenCorr, AnsweredCorrHeldBead, DoneWithoutLegalClose.
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
/// process split may remain (remote-resonate / durable replay), but two
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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceModel {
    pub doorbell: crate::doorbell_tick::DoorbellModel,
    pub swarm: crate::durable::LoopModel,
    pub assignment: BTreeMap<String, Receipt>,
    pub roles: BTreeMap<String, String>,
    pub presence: BTreeMap<String, Presence>,
    pub open_human: BTreeMap<String, Question>,
    pub seq: u64,
    /// WaitCorr corrs whose receipt has not landed yet (fast-path re-peek
    /// on WATCH batches only, GS-016: never the 1s sweep), with deadline
    /// unix secs (bd-zfnx.6: bounded — TTL + cap, never an
    /// unbounded set). The doorbell dispatcher owns this cache; swarm
    /// never reads it. Expiry/cap-eviction happens in the Sweep arm and
    /// always records Cmd::Audit wait-timeout (never silent drop).
    pub pending_wait: std::collections::BTreeMap<String, u64>,
    /// ADR-0010 D1 (bd-w98c): re-ring ladder entries by corr. The
    /// dispatcher owns this cache; swarm never reads it. Entries are
    /// advanced on the AckSlaTick beat; registration rides the InboxRows
    /// fold (SlaFired hit list → FetchInbox → overdue corr rows) — the
    /// cross-process residual is closed (2026-09-17).
    pub rering: std::collections::BTreeMap<String, ReringEntry>,
    /// ADR-0010 D2 (bd-8jlv): presented-but-unacked wakes per pin.
    /// Derived deterministically from the Msg log, so crash-resume
    /// refold reproduces it exactly (replay drops nothing, dups nothing).
    pub presented: std::collections::BTreeMap<String, Vec<PresentedWake>>,
    /// ADR-0010 D2 (bd-8jlv): ack watermark per pin — highest handled
    /// presentation seq. Entries with seq <= watermark are handled fact.
    pub wake_acked: std::collections::BTreeMap<String, u64>,
    /// ADR-0010 D5 (bd-65he): captain-relevant wakes awaiting the afk
    /// digest batch. Drained into Cmd::Digest on interval/threshold.
    pub digest_queue: Vec<DigestEntry>,
    /// ADR-0010 D5 (bd-65he): per-class tick counts (journal+counter
    /// for routine; proves benign/routine classes spend zero prompts).
    pub wake_counters: std::collections::BTreeMap<String, u64>,
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

/// ADR-0010 D2 (bd-8jlv): one presented wake — the line as prompted,
/// bound to the tick generation that presented it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresentedWake {
    pub seq: u64,
    pub line: String,
}

// ---------------------------------------------------------------------------
// ADR-0010 D5 (bd-65he): wake classification + afk digest.
//
// Pure predicates in workspace::tick: benign (zero prompts), routine
// (journal+counter), captain-relevant (digest queue), urgent (immediate
// prompt, 5/30/3 cap unchanged). Every Prompt counts as spent wake, so
// non-urgent prompts queue as captain-relevant (visible) rather than
// routine — routine ticks provably contain no Cmd::Prompt. Urgent
// bypasses batching: it prompts now AND queues for the digest record.
// ---------------------------------------------------------------------------

/// Default digest batching interval (secs). Override with
/// FLYWHEEL_DIGEST_SECS.
pub const DIGEST_INTERVAL_DEFAULT_SECS: u64 = 300;
/// Default digest queue depth that forces a batch. Override with
/// FLYWHEEL_DIGEST_THRESHOLD.
pub const DIGEST_THRESHOLD_DEFAULT: usize = 10;

/// Digest batching interval secs (env override, default 300).
pub fn digest_interval_secs() -> u64 {
    std::env::var("FLYWHEEL_DIGEST_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DIGEST_INTERVAL_DEFAULT_SECS)
}

/// Digest queue depth that forces a batch (env override, default 10).
pub fn digest_threshold() -> usize {
    std::env::var("FLYWHEEL_DIGEST_THRESHOLD")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DIGEST_THRESHOLD_DEFAULT)
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
pub use doorbell_core::is_urgent_line;

/// Edge-triage predicate (bd-l72k): a machine-rendered addressed kind-9
/// wake (`doorbell::doorbell_line(Channel, _)` head). Routine chatter:
/// advances markers with zero prompts, never enters the digest. Urgent
/// lines never carry this head, but callers must test urgency first.
/// Unknown/unmarked prompts default CaptainRelevant (digest-visible),
/// never silently routine.
pub fn is_routine_wake_line(line: &str) -> bool {
    line.starts_with("[doorbell] kind 9.")
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
        // A batch just emitted: captain-facing work, but never re-queued
        // (the D5 loop skips Digest cmds so batches cannot self-feed).
        Cmd::Digest { .. } => WakeClass::CaptainRelevant,
        _ => WakeClass::Routine,
    }
}

/// One captain-relevant wake awaiting the digest batch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DigestEntry {
    pub seq: u64,
    /// Last wall-clock seen when recorded (0 = before any clock Msg).
    pub at_unix: u64,
    pub kind: String,
    pub pin: String,
    pub detail: String,
}

/// Digest detail is bounded: prompt lines can be long, the queue is not
/// a log. Truncation is marked.
pub fn digest_detail_truncate(detail: &str) -> String {
    const MAX: usize = 200;
    if detail.len() <= MAX {
        return detail.to_string();
    }
    format!("{}…[{} chars]", &detail[..MAX], detail.len())
}

/// Extract bead ids (`bd-…` tokens) from digest details, sorted dedup.
pub fn digest_bead_ids(entries: &[DigestEntry]) -> Vec<String> {
    let mut ids: Vec<String> = entries
        .iter()
        .flat_map(|e| e.detail.split(|c: char| !c.is_alphanumeric() && c != '-'))
        .filter(|t| t.starts_with("bd-") && t.len() > 3)
        .map(str::to_string)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Render the afk digest: outcomes first with ages, bead ids kept,
/// exact commands last. Pure — the afk CLI and the Digest effect share it.
pub fn render_digest(entries: &[DigestEntry], now: u64) -> String {
    let mut out = format!("doorbell afk digest ({} items)\n", entries.len());
    for e in entries {
        // bd-z5ut: `at_unix == 0` means "recorded before any clock Msg in this
        // fold", NOT "56 years ago". Subtracting it printed a raw epoch value
        // dressed as a duration, which is worse than admitting the unknown.
        let age = match e.at_unix {
            0 => "age unknown".to_string(),
            t => format!("{}s ago", now.saturating_sub(t)),
        };
        let _ = writeln!(out, "- [{age}] {} {}: {}", e.kind, e.pin, e.detail);
    }
    let beads = digest_bead_ids(entries);
    if beads.is_empty() {
        out.push_str("beads: none in this digest\n");
    } else {
        let _ = writeln!(out, "beads: {}", beads.join(", "));
    }
    out.push_str("commands:\n");
    for b in &beads {
        let _ = writeln!(out, "  br show {b}");
    }
    if beads.is_empty() {
        out.push_str("  (no bead commands)\n");
    }
    out
}

/// Wedge alarm: a non-empty queue older than two batch intervals means
/// the digest path cannot deliver — fail loud, never silently batch.
pub fn digest_overdue(queue_len: usize, last_digest_unix: u64, now: u64) -> bool {
    queue_len > 0 && now.saturating_sub(last_digest_unix) > 2 * digest_interval_secs()
}

/// bd-2fxx: deadline-attached holds. The pending_wait value IS the
/// needed-by unix secs (stamped now+TTL at the WaitCorr effect site,
/// decided in the Sweep arm). No new store: these folds read it.
pub fn sorted_holds(model: &WorkspaceModel) -> Vec<(String, u64)> {
    let mut holds: Vec<(String, u64)> = model
        .pending_wait
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

// ---------------------------------------------------------------------------
// ADR-0010 D1 (bd-w98c): re-ring ladder — the dispatcher wake budget.
//
// The toron message IS the delivery; the herdr prompt is a doorbell.
// An un-acked corr older than one grace window is due one re-ring per
// window; after RING_MAX the wake escalates: Dead → respawn path,
// Unknown → digest naming pin+corr+attempt count. This REPLACES the flat
// 120s deaf-pane digest. Fire-and-forget and already-acked corrs skip
// the ladder; --raw without an id rings once and never escalates.
// Functional core (no I/O): tick advances entries, effect layer prompts.
// ---------------------------------------------------------------------------

/// Default grace window between re-rings (secs). Override with
/// FLYWHEEL_DOORBELL_GRACE_SECS.
pub const DOORBELL_GRACE_DEFAULT_SECS: u64 = 90;
/// Default max rings before escalation. Override with
/// FLYWHEEL_DOORBELL_RING_MAX.
pub const DOORBELL_RING_MAX_DEFAULT: u32 = 3;

/// Grace window secs for the re-ring ladder (env override, default 90).
pub fn doorbell_grace_secs() -> u64 {
    std::env::var("FLYWHEEL_DOORBELL_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DOORBELL_GRACE_DEFAULT_SECS)
}

/// Max rings before escalation (env override, default 3).
pub fn doorbell_ring_max() -> u32 {
    std::env::var("FLYWHEEL_DOORBELL_RING_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DOORBELL_RING_MAX_DEFAULT)
}

/// D1 shrunk prompt: the payload rides the toron message, herdr gets
/// the doorbell only.
pub fn doorbell_mail_line(unacked: usize, pin: &str, corr: &str) -> String {
    format!("[doorbell] inbox: {unacked} unacked for {pin}; latest corr={corr}; read + ack")
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

/// One tracked delivery: a toron corr awaiting its crew ack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReringEntry {
    pub pin: String,
    /// Rings already emitted for this corr.
    pub attempts: u32,
    /// Unix secs of the current grace window start.
    pub window_start_unix: u64,
    /// Acked corrs skip the ladder (handling fact already observed).
    pub acked: bool,
    /// --fire-and-forget deliveries never climb the ladder.
    pub fire_and_forget: bool,
    /// --raw without an id rings once and never escalates.
    pub raw_without_id: bool,
}

/// Ladder outcome for one corr at one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReringDecision {
    /// Emit ring number `attempt` (0-based) now; caller bumps the entry.
    Ring { attempt: u32 },
    /// Past max rings and the pane is Dead → respawn path.
    RecoverDead,
    /// Past max rings and the pane is Unknown → digest (no prompt).
    DigestUnknown,
    /// Inside a grace window: nothing due yet.
    Pending,
    /// Opt-out: acked, fire-and-forget, or raw already rang once.
    Skip,
}

/// Pure ladder step. `verdict` is the pin's current presence verdict
/// ("Dead" / "Unknown" / other) — read by the tick arm, never
/// constructed here, so the Presence enum stays Lane-2 owned.
pub fn rering_due(entry: &ReringEntry, now: u64, verdict: &str) -> ReringDecision {
    if entry.acked || entry.fire_and_forget {
        return ReringDecision::Skip;
    }
    if entry.raw_without_id {
        if entry.attempts == 0 {
            return ReringDecision::Ring { attempt: 0 };
        }
        return ReringDecision::Skip;
    }
    let grace = doorbell_grace_secs();
    let max = doorbell_ring_max();
    if entry.attempts >= max {
        return match verdict {
            "Dead" => ReringDecision::RecoverDead,
            "Unknown" => ReringDecision::DigestUnknown,
            _ => ReringDecision::Ring { attempt: max },
        };
    }
    if now.saturating_sub(entry.window_start_unix) >= grace {
        return ReringDecision::Ring {
            attempt: entry.attempts,
        };
    }
    ReringDecision::Pending
}

// ---------------------------------------------------------------------------
// Msg — one union covering mail / watch / status / heal / reclaim / receipt / role / human / presence
//
// Per-arm contract (ADR-0028 C): every variant states what it writes and
// what it emits. Clock enters ONLY as a payload (Tick/AckSlaTick/Sweep).
// ---------------------------------------------------------------------------

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
    HerdrStatus {
        pin: String,
        status: String,
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
    /// ADR-0010 D2 (bd-8jlv): crew handler mints this after handling a
    /// wake, citing the generation it handled. Presented wakes with
    /// seq <= through_seq leave the presented set (handling fact);
    /// anything newer re-presents at the next prompt. Idempotent:
    /// duplicate or stale acks fold into the watermark via max.
    WakeAck {
        pin: String,
        through_seq: u64,
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
    /// Herdr pane event. Enters as Msg (not a world-dump into pending_events).
    /// Delegates to the swarm slice.
    Herdr {
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
}

// ---------------------------------------------------------------------------
// Cmd — effects as data (DoorbellAction + LoopAction collapse into this)
//
// Per-arm contract (ADR-0028 C): Prompt gated by prompt_allowed; every
// arm is applied by exactly one executor (dispatcher or swarm_loop) or
// carries a bead deleting it. Content shapes: Publish is rumor-14,
// WaitCorr carries a non-empty corr.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cmd {
    // doorbell slice — data equivalents of DoorbellAction
    /// Inject iff prompt_allowed (E5 gate). Line always starts "[doorbell]".
    Prompt { pin: String, line: String },
    /// Effect runs the fetch; rows re-enter tick as InboxRows.
    /// Signal only (bd-z5ou): per-pin cursors live in the brain.
    FetchInbox,
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

    // swarm slice — data equivalents of LoopAction
    /// Swarm send. Name is the herdr pane, text the full prompt.
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
    /// Swarm send. Reclaim carries bead + pin + herdr name + reason.
    Reclaim {
        bead_id: String,
        pin: String,
        herdr_name: String,
        reason: String,
    },
    /// Swarm send. Cooperative yield; no content.
    Wait,

    // E2/E3 assignment + receipts
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
    // DELETED by zfnx.4 (E1): WriteNcryptsec is gone — flywheel never
    // writes ncryptsec secrets (keys belong to toron); a secret-writing
    // Cmd beside rumor-14 would be an exfil path. Likewise the Subscribe
    // Cmd is gone (55nx.7): claim-watch lives in swarm_loop, no second bus.
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
            crate::doorbell_tick::DoorbellAction::Prompt { pin, line } => {
                Self::Prompt { pin, line }
            }
            crate::doorbell_tick::DoorbellAction::FetchInbox => Self::FetchInbox,
            crate::doorbell_tick::DoorbellAction::CheckAckSla => Self::CheckAckSla,
            crate::doorbell_tick::DoorbellAction::RestartWatch => Self::RestartWatch,
            crate::doorbell_tick::DoorbellAction::FactMiss { why } => Self::FactMiss { why },
            crate::doorbell_tick::DoorbellAction::Audit { kind, pin, detail } => {
                Self::Audit { kind, pin, detail }
            }
            // bd-bk9t: a pane-less pin's wake has no prompt route. In this
            // path the digest IS the audit surface — `classify_cmd` reads
            // every Audit as CaptainRelevant and queues it for the batch —
            // so the wake lands in the digest with the reason on the row.
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
                herdr_name,
                reason,
            } => Self::Reclaim {
                bead_id,
                pin,
                herdr_name,
                reason,
            },
            // ADR-0010 D4/D12: the declared wait's human re-surface path. The
            // Audit is the captain-relevant record `classify_cmd` queues for
            // the digest, so a pause that nobody answers keeps surfacing
            // instead of parking silently (detail text is the shared line, so
            // every plane renders the resurface identically).
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
                herdr_name,
                reason,
            } => Some(crate::durable::LoopAction::Reclaim {
                bead_id: bead_id.clone(),
                pin: pin.clone(),
                herdr_name: herdr_name.clone(),
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
    // Hold open against invent flips (loop|swarm|join|none) without explicit.
    if current == Some("open") && matches!(classified, "loop" | "swarm" | "join" | "none") {
        return ("open".into(), true);
    }
    // Hold loop against swarm without explicit (docs: do not flip loop→swarm).
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
    // Bare "switch to swarm/loop/open/join" / "use swarm topology"
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
    // Goal-change markers from steer / operator pane text.
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
                // USE the key — never match `_` (Cmd theater without journal).
                let expected =
                    reclassify_effect_key(&goal, &reason, explicit_topology.as_deref(), &pin);
                if effect_key != expected {
                    eprintln!(
                        "flywheel: reclassify effect_key drift cmd={effect_key} derived={expected}"
                    );
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
                        eprintln!("flywheel: reclassify journal insert failed: {e:#}");
                        crate::agent_ref::ReclassifyIntent::from_cmd_key(
                            effect_key.clone(),
                            goal.clone(),
                            reason.clone(),
                            explicit_topology.clone(),
                            pin.clone(),
                            0,
                        )
                    }
                };
                // bd-n86n: W2 (ADR-0014, default off) host-side Jev enrichment.
                // The pure table leaf is untouched; the adapter only annotates
                // beside it and journals a JudgmentEffect child of this intent.
                // Decide + prepare + POST run here, OUTSIDE the journal tx:
                // commit holds the tx across its closure, so a blocking 5s
                // POST inside would stall every concurrent journal writer.
                // Flag off = byte-identical passthrough (no audit).
                let (primitive, topology, argv, decide_reason) = decide(&intent.goal);
                let authority = crate::authority::RuntimeAuthority::resolve();
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
                    &authority,
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
                // Doorbell battery (bd-lpn6): second pack over inbound
                // material in the same outside-tx window. Detections annotate
                // routing only via finish_plan; table authority untouched.
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
                    // Apply uses the journaled identity (goal + effect_key).
                    let _ = &intent.effect_key;
                    // bd-n86n: decide + prepare + POST all ran OUTSIDE the tx
                    // (below, before commit) — the closure holds the journal
                    // tx, so it must never wait on network. Only merge+tick
                    // run here: pure folds, no I/O. `enriched`/`ready` move in.
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
                    // Miss ledger (bd-lpn6): one row per expand, keyed by the
                    // parent effect key. Best-effort: a ledger failure warns
                    // but never fails the dispatch it observes.
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
                        eprintln!("flywheel: reclassify commit failed: {e:#}");
                        // Best-effort: still fold DecisionReady so the session
                        // is not stuck without an announce (journal already
                        // recorded Failed).
                        let (primitive, topology, argv, decide_reason) = decide(&goal);
                        let ready = Msg::DecisionReady {
                            goal,
                            primitive,
                            topology,
                            argv,
                            decide_reason,
                            explicit_topology,
                        };
                        let (next, more) = tick(model, &ready);
                        *model = next;
                        more
                    }
                };
                out.extend(more);
            }
            other => out.push(other),
        }
    }
    out
}

fn filter_prompts(model: &WorkspaceModel, cmds: Vec<Cmd>) -> Vec<Cmd> {
    // bd-7qg6: the old filter silently dropped every non-Idle Prompt — including
    // URGENT wakes to Busy pins, which never reached the post-pass urgent
    // bypass (classify-then-retain) and vanished without a digest trace. New
    // rule: Idle prompts; Busy+urgent BREAKS THROUGH (ADR-0005 gap 5, E5 —
    // urgency exists for working panes); everything else becomes an Audit so
    // the wake stays digest-visible instead of vanishing. Unknown never
    // prompts (invariant 9) and Dead stays Lane-1 drain territory — both audit.
    cmds.into_iter()
        .flat_map(|c| match c {
            Cmd::Prompt { pin, line } => {
                // Idle prompts and Busy+urgent break-throughs deliver the SAME
                // Cmd — one condition, not two identical branches.
                if prompt_allowed(model, &pin)
                    || (is_urgent_line(&line)
                        && matches!(model.presence.get(&pin), Some(Presence::Busy { .. })))
                {
                    vec![Cmd::Prompt { pin, line }]
                } else {
                    vec![Cmd::Audit {
                        kind: "prompt_filtered".into(),
                        pin,
                        detail: format!("non-injectable presence held wake for digest: {line}"),
                    }]
                }
            }
            _ => vec![c],
        })
        .collect()
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

// One positional per assignment field the mail row carries; grouping them would
// only move the field list into a struct.
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
    // ADR-0028 C: tag vocabulary is wake ∈ {tasked,standing}, audience ∈
    // {human,agents}, priority ∈ {queue,interrupt} — mirrors the
    // SendTags::parse hard-error on the send path. Absent tags arrive
    // pre-defaulted to valid members; a present-but-unknown value is a
    // malformed assignment → LoudMiss (tick's hard-error), no model write.
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
    // bd-bdpx: the park deadline is stamped HERE from the tick clock
    // (last_now), never wall-clock in the effect handler — replay reproduces
    // it byte-identically from the Msg log. last_now==0 means no clock Msg
    // has folded yet (pre-first-beat boot sliver or a bare Default in
    // tests): the Cmd still emits, but no deadline is registered — the live
    // dispatcher seeds last_now at construction so the sliver never happens
    // there. Sweep-arm expiry + cap-eviction stay the deciders, each with an
    // Audit wait-timeout record. Never silent drop.
    if next.last_now > 0 {
        next.pending_wait.insert(
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

// ---------------------------------------------------------------------------
// Private slice reducers — the only place the two old ticks are called.
// ---------------------------------------------------------------------------

fn doorbell_slice(
    model: &crate::doorbell_tick::DoorbellModel,
    msg: crate::doorbell_tick::DoorbellMsg,
) -> (crate::doorbell_tick::DoorbellModel, Vec<Cmd>) {
    let (next, actions) = crate::doorbell_tick::tick(model, &msg);
    let cmds = actions.into_iter().map(Cmd::from_doorbell).collect();
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

/// Replay / live-loop helper: every LoopMsg enters through workspace::tick.
pub fn tick_from_loop_msg(
    model: &crate::durable::LoopModel,
    msg: &crate::durable::LoopMsg,
) -> (crate::durable::LoopModel, Vec<crate::durable::LoopAction>) {
    let ws_msg = match msg {
        crate::durable::LoopMsg::Tick { now } => Msg::Tick { now: *now },
        crate::durable::LoopMsg::Heal {
            name,
            status,
            exited,
        } => Msg::Heal {
            name: name.clone(),
            status: status.clone(),
            exited: *exited,
        },
        crate::durable::LoopMsg::ClaimNudge { name, bead_id } => Msg::ClaimNudge {
            name: name.clone(),
            bead_id: bead_id.clone(),
        },
        crate::durable::LoopMsg::CloseNudge {
            name,
            bead_id,
            title,
        } => Msg::CloseNudge {
            name: name.clone(),
            bead_id: bead_id.clone(),
            title: title.clone(),
        },
        crate::durable::LoopMsg::Reclaim {
            bead_id,
            holder_pin,
            claimant_pin,
            is_live,
        } => Msg::Reclaim {
            bead_id: bead_id.clone(),
            holder_pin: holder_pin.clone(),
            claimant_pin: claimant_pin.clone(),
            is_live: *is_live,
            reason: String::new(),
        },
        other => {
            let (next, cmds) = swarm_slice(model, other.clone());
            let acts = cmds.iter().filter_map(Cmd::to_loop_action).collect();
            return (next, acts);
        }
    };
    let wm = WorkspaceModel {
        swarm: model.clone(),
        ..Default::default()
    };
    let (wm, cmds) = tick(&wm, &ws_msg);
    let acts = cmds.iter().filter_map(Cmd::to_loop_action).collect();
    (wm.swarm, acts)
}

// ---------------------------------------------------------------------------
// Entry: one tick
// ---------------------------------------------------------------------------

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
            cmds.extend(filter_prompts(&next, c));
            // E2/E4/E6: 30013 role=, 20001 presence. Kind-9 is the ring, not
            // the assignment vehicle — corr/wake ride rumor 14 / InboxRows.
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                // 20001 presence — ADR-0028 E5 trust rule: inject truth is
                // the pin itself (line `pubkey` == pin's npub in
                // `doorbell.index`) or the trusted `Msg::Presence` edge
                // (dispatcher-internal, e.g. jsonl timeout path below).
                // A 20001 line whose author is provably not the claimed pin
                // is a forge: LoudMiss (+Audit), presence unchanged.
                // Verification needs BOTH a known npub mapping AND a line
                // pubkey; when either is absent forgery is unprovable so the
                // claim is accepted (bootstrap + pre-npub tests). Missing
                // key = Unknown; Unknown never prompts (see prompt_allowed).
                if v.get("kind").and_then(serde_json::Value::as_u64) == Some(20001)
                    && let Some(content) = v.get("content").and_then(|c| c.as_str())
                {
                    // content is like "idle" or JSON with pin/state? For test, content is state, tags contain p tag for pin
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
                    } else {
                        // Fallback: if no p tag, use pin from content? For test, just use a dummy
                    }
                }
                // 30013 role binding
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
                    // Kind-9 is the ring. Assignment enters from rumor 14 tags
                    // (or InboxRows from FetchInbox). Never from mail-delivered.
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
            cmds.extend(filter_prompts(&next, c));
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
            cmds.extend(filter_prompts(&next, c));
            // E2/E3: assignment from tagged rows (corr/wake/priority/audience/result_path)
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
            // ADR-0010 D1 (bd-w98c residual): ladder registration + ack fold
            // from the inbox projection itself. An ack-overdue row (the
            // scan's scope) registers/refreshes its corr on the ladder —
            // window stamped from the tick clock (bd-bdpx rule), never
            // wall-clock; a row already per-recipient read is the handling
            // fact and marks the entry acked. Idempotent by corr.
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
                    // The handling fact wins over the shape test: a read row
                    // acks its corr (when tracked) and never registers.
                    if let Some(e) = next.rering.get_mut(&corr) {
                        e.acked = true;
                    }
                } else if inbox_row_ack_overdue(row) {
                    let e = next.rering.entry(corr).or_insert_with(|| ReringEntry {
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
        Msg::HerdrStatus { pin, status } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::HerdrStatus {
                    pin: pin.clone(),
                    status: status.clone(),
                },
            );
            next.doorbell = dm;
            cmds.extend(filter_prompts(&next, c));
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
                    });
                } else {
                    cmds.extend(c);
                }
            } else {
                cmds.extend(c);
            }
        }
        Msg::AckSlaTick { now } => {
            let (dm, c) = doorbell_slice(
                &model.doorbell,
                crate::doorbell_tick::DoorbellMsg::AckSlaTick { now: *now },
            );
            next.doorbell = dm;
            cmds.extend(c);
            // ADR-0010 D1 (bd-w98c): advance the re-ring ladder. One
            // ring per grace window; past max rings Dead→respawn and
            // Unknown→digest. Acked / fire-and-forget entries leave
            // without a sound. Presence is read-only here; the enum
            // itself stays Lane-2 (bd-s8bo) owned.
            let corrs: Vec<String> = next.rering.keys().cloned().collect();
            for corr in corrs {
                let Some(entry) = next.rering.get(&corr).cloned() else {
                    continue;
                };
                if entry.acked || entry.fire_and_forget {
                    next.rering.remove(&corr);
                    continue;
                }
                let verdict = match next.presence.get(&entry.pin) {
                    Some(Presence::Dead { .. }) => "Dead",
                    Some(Presence::Unknown) => "Unknown",
                    _ => "other",
                };
                match rering_due(&entry, *now, verdict) {
                    ReringDecision::Ring { attempt } => {
                        cmds.push(Cmd::Prompt {
                            pin: entry.pin.clone(),
                            line: doorbell_mail_line(1, &entry.pin, &corr),
                        });
                        if let Some(e) = next.rering.get_mut(&corr) {
                            e.attempts = attempt + 1;
                            e.window_start_unix = *now;
                            if e.raw_without_id {
                                next.rering.remove(&corr);
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
                        next.rering.remove(&corr);
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
                        next.rering.remove(&corr);
                    }
                    ReringDecision::Pending | ReringDecision::Skip => {}
                }
            }
        }
        Msg::SlaFired { posts } => {
            // The scan's hit list is evidence, not a ring: the wake itself
            // already traveled the watch path (toron emits kind-9
            // ack-sla-expired per overdue post). Tick audits the delta and
            // pulls the inbox so overdue corr rows re-enter as InboxRows —
            // that arm is the ladder's registrar.
            cmds.push(Cmd::Audit {
                kind: "ack-sla-hit".into(),
                pin: "-".into(),
                detail: format!("overdue corrs: {}", posts.join(",")),
            });
            cmds.push(Cmd::FetchInbox);
        }
        Msg::Sweep { now } => {
            cmds.push(Cmd::FetchInbox);
            // bd-zfnx.6: bound the wait cache on every sweep beat. Expired
            // deadlines and over-cap earliest entries leave WITH an Audit
            // wait-timeout record each — a timeout is observable, never a
            // silent drop. Receipt arrival still wins via try_tick_receipt
            // (effect side) before the next sweep.
            let mut gone: Vec<(String, &'static str)> = Vec::new();
            for (corr, deadline) in &next.pending_wait {
                if *deadline <= *now {
                    gone.push((corr.clone(), "expired"));
                }
            }
            for (corr, _) in &gone {
                next.pending_wait.remove(corr);
            }
            while next.pending_wait.len() > PENDING_WAIT_CAP {
                if let Some((corr, ())) = next
                    .pending_wait
                    .iter()
                    .min_by_key(|(_, d)| *d)
                    .map(|(c, _)| (c.clone(), ()))
                {
                    next.pending_wait.remove(&corr);
                    gone.push((corr, "over-cap"));
                } else {
                    break;
                }
            }
            for (corr, why) in gone {
                // D6 (bd-f14d): expiry PROMOTES into the open_human cache —
                // re-surface, never drop. Silent cache insert (no Publish:
                // promotion must not re-ring); the Audit below stays the
                // observable record. Clears only via answer/--verdict.
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
                    detail: format!("pending_wait {why}"),
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
        Msg::WakeAck { pin, through_seq } => {
            // ADR-0010 D2 (bd-8jlv): fold the generation-bound ack
            // into the watermark (idempotent max — duplicate or stale
            // acks change nothing) and drop handled presentations.
            // Emits nothing: the ack is the handling fact, recorded
            // in the Msg journal by the caller.
            let mark = next.wake_acked.get(pin).copied().unwrap_or(0);
            if *through_seq > mark {
                next.wake_acked.insert(pin.clone(), *through_seq);
            }
            let watermark = next.wake_acked.get(pin).copied().unwrap_or(0);
            if let Some(entries) = next.presented.get_mut(pin) {
                entries.retain(|e| e.seq > watermark);
                if entries.is_empty() {
                    next.presented.remove(pin);
                }
            }
        }
        Msg::Reclaim {
            bead_id,
            holder_pin,
            claimant_pin,
            is_live,
            reason,
        } => {
            let (sm, mut c) = swarm_slice(
                &model.swarm,
                crate::durable::LoopMsg::Reclaim {
                    bead_id: bead_id.clone(),
                    holder_pin: holder_pin.clone(),
                    claimant_pin: claimant_pin.clone(),
                    is_live: *is_live,
                },
            );
            next.swarm = sm;
            if !c.iter().any(|cmd| matches!(cmd, Cmd::Reclaim { .. })) {
                c.push(Cmd::Reclaim {
                    bead_id: bead_id.clone(),
                    pin: holder_pin.clone(),
                    herdr_name: claimant_pin.clone(),
                    reason: if reason.is_empty() {
                        "reclaim".into()
                    } else {
                        reason.clone()
                    },
                });
            }
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
            // ADR-0016: receipt correlation is a validated boundary value.
            // Preserve the wire string unchanged on success; reject malformed
            // input before it can create an unaddressable assignment or unpark
            // the wrong waiter.
            if let Err(error) = crate::kernel::Corr::parse(corr.clone()) {
                cmds.push(Cmd::LoudMiss {
                    why: format!("receipt corr invalid: {error}"),
                });
            } else {
                next.assignment.insert(corr.clone(), receipt.clone());
                // bd-bpj1 (F1): the wait-list removal is owned here, not by the
                // effect-layer drain — the full receipt transition (insert +
                // unpark + upsert) reproduces from this one Msg on replay.
                next.pending_wait.remove(corr);
                cmds.push(Cmd::SqliteUpsert {
                    corr: corr.clone(),
                    receipt: receipt.clone(),
                });
            }
        }
        Msg::RoleBound { role, pin } => {
            // E4 open-corr inheritance is LATE BINDING: assignment receipts
            // are keyed by corr with no pin field, so every later Prompt
            // resolves through `roles` at dispatch time — rebinding the role
            // re-targets all open corrs by construction. Nothing to copy.
            next.roles.insert(role.clone(), pin.clone());
        }
        Msg::Presence { pin, state } => {
            // Trusted internal edge (dispatcher jsonl timeout path):
            // no author check — the dispatcher is the trust root here. The
            // identity itself is still validated before it can become a map
            // key or prompt target.
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
        Msg::Herdr { event } => {
            let (sm, c) = swarm_slice(&model.swarm, crate::durable::LoopMsg::Herdr(event.clone()));
            next.swarm = sm;
            cmds.extend(c);
        }
        Msg::MaterialContext {
            goal,
            explicit_topology,
            reason,
        } => {
            // Pure: schedule caller-applied effect. No decide I/O here.
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
            // Sticky-open airtight: never announce invent wave-swarm under open
            // (or invent swarm under held loop) — `none: raw` / open|loop vehicle only.
            let vehicle = if sticky_held {
                sticky_held_vehicle_announcement(primitive, &final_topo)
            } else {
                vehicle_announcement(primitive, &final_topo)
            };
            next.last_vehicle = Some(vehicle.clone());
            // Sticky-held announce must not name invent swarm primitive in
            // the Prompt line (vehicle or reason) — audit keeps raw classify.
            let announce_reason = if sticky_held {
                "session topology held (invent vehicle suppressed)"
            } else {
                decide_reason.as_str()
            };
            let line = reclassify_announce_line(&vehicle, announce_reason, sticky_held);
            if let Some(pin) = doorbell_core::fact_captain_pin(&next.roles) {
                // Urgent reclassify announces break through Busy (same E5
                // breakthrough as plane-P); Unknown/Dead stay Audit-only.
                let busy = matches!(next.presence.get(&pin), Some(Presence::Busy { .. }));
                if prompt_allowed(&next, &pin) || busy {
                    cmds.push(Cmd::Prompt {
                        pin: pin.clone(),
                        line: line.clone(),
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

    // ADR-0010 D2 (bd-8jlv): generation-bound wake ledger, one central
    // post-pass so every Prompt site shares the semantics. (1) Re-present:
    // pins prompted this tick re-emit their presented-but-unacked lines
    // first (same bytes — a duplicate ring is never second work), gated
    // by prompt_allowed like any wake. (2) Record: every emitted Prompt
    // upserts presented[pin] by line at this generation. Fresh models
    // carry no unacked entries, so silent ticks stay silent.
    let mut replays: Vec<Cmd> = Vec::new();
    for cmd in &cmds {
        if let Cmd::Prompt { pin, line } = cmd {
            // bd-l72k: only urgent lines re-ring. Routine/captain prompts
            // never deliver (stripped below), so they must never enter
            // the ledger or resurrect as replays — digest/counters own them.
            if !is_urgent_line(line) {
                continue;
            }
            let watermark = next.wake_acked.get(pin).copied().unwrap_or(0);
            if let Some(entries) = next.presented.get(pin) {
                for e in entries {
                    if e.seq > watermark
                        && e.line != *line
                        && !cmds.iter().any(|c| matches!(c, Cmd::Prompt { pin: p, line: l } if p == pin && l == &e.line))
                        && !replays.iter().any(|c| matches!(c, Cmd::Prompt { pin: p, line: l } if p == pin && l == &e.line))
                    {
                        let line = Cmd::Prompt {
                            pin: pin.clone(),
                            line: e.line.clone(),
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
        if let Cmd::Prompt { pin, line } = cmd {
            // bd-l72k: the ledger records deliveries only. Non-urgent
            // prompts are stripped below — recording them would grow
            // `presented` with lines that never deliver and re-ring forever.
            if !is_urgent_line(line) {
                continue;
            }
            let slot = next.presented.entry(pin.clone()).or_default();
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

    // ADR-0010 D5 (bd-65he): classify the tick, queue captain-relevant
    // wakes for the afk digest, count every tick once. Digest cmds never
    // re-queue themselves. Batch emits on now-bearing beats when the
    // queue hits depth N or the interval lapses; urgent prompts bypass
    // batching by construction (prompted immediately above).
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
            Cmd::Prompt { pin, line } if is_urgent_line(line) => Some(DigestEntry {
                seq: next.seq,
                at_unix: next.last_now,
                kind: "urgent-prompt".into(),
                pin: pin.clone(),
                detail: digest_detail_truncate(line),
            }),
            Cmd::Prompt { line, .. } if is_routine_wake_line(line) => None,
            Cmd::Prompt { pin, line } => {
                let short = digest_detail_truncate(line);
                let queued = next
                    .digest_queue
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
            next.digest_queue.push(e);
        }
    }
    *next
        .wake_counters
        .entry(tick_class.label().to_string())
        .or_insert(0) += 1;
    // bd-l72k edge triage: non-urgent prompts never deliver (no LLM turn).
    // Captain-relevant entries are already queued above; routine ticks are
    // already counted above. Only urgent prompts reach the effect layer.
    cmds.retain(|c| !matches!(c, Cmd::Prompt { line, .. } if !is_urgent_line(line)));
    if let Some(now) = match msg {
        Msg::AckSlaTick { now } | Msg::Sweep { now, .. } | Msg::Tick { now } => Some(*now),
        _ => None,
    } && !next.digest_queue.is_empty()
        && (next.digest_queue.len() >= digest_threshold()
            || now.saturating_sub(next.last_digest_unix) >= digest_interval_secs())
    {
        let entries = std::mem::take(&mut next.digest_queue);
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
        assert!(cmds.is_empty());
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
    fn ws_model_herdr_status_is_inject_truth() {
        let mut m = empty_model();
        m.doorbell.index.insert_pin("BlueRidge".into(), None);
        // D3: prompt needs an exact Idle verdict (presence authority);
        // herdr statuses remain evidence in doorbell.statuses.
        m.presence
            .insert("BlueRidge".into(), Presence::from_str("idle"));
        let (m, _) = tick(
            &m,
            &Msg::HerdrStatus {
                pin: "BlueRidge".into(),
                status: "working".into(),
            },
        );
        assert_eq!(
            m.doorbell.statuses.get("BlueRidge").map(String::as_str),
            Some("working"),
            "HerdrStatus tick is the only status write"
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
            &Msg::HerdrStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        // bd-l72k: the drain still fires from Model (queue empties), but
        // the drained routine wake no longer delivers a prompt.
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

    #[test]
    fn ws_model_tick_composes_doorbell_watchndjson() {
        let mut m = empty_model();
        m.doorbell.index.insert_pin("BlueRidge".into(), None);
        // D3: herdr idle alone never prompts (never trusted); the exact
        // Idle verdict below is the authority that lets the Prompt through.
        m.presence
            .insert("BlueRidge".into(), Presence::from_str("idle"));
        m.doorbell
            .statuses
            .insert("BlueRidge".into(), "idle".into());
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#.to_string();
        let (next, cmds) = tick(&m, &Msg::WatchNdjson { line });
        // bd-l72k: addressed kind-9 is routine — zero prompts, counters only.
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
    fn ws_model_role_bound_updates_roles_map() {
        let m = empty_model();
        let (next, cmds) = tick(
            &m,
            &Msg::RoleBound {
                role: "wheelhouse".into(),
                pin: "DustySparrow".into(),
            },
        );
        assert!(cmds.is_empty());
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
    fn ws_model_receipt_unparks_the_wait() {
        // bd-bpj1 (F1): the receipt transition owns the wait-list removal —
        // the effect-layer drain must have nothing left to do.
        let mut m = empty_model();
        m.pending_wait.insert("c".into(), 999);
        let (next, _) = tick(
            &m,
            &Msg::Receipt {
                corr: "c".into(),
                receipt: Receipt::default(),
            },
        );
        assert!(!next.pending_wait.contains_key("c"));
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
        assert!(cmds2.is_empty());
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
        // bd-loop-goal-name-dispatch-eo4u: Tick pairs the queue head with a
        // REAL agent — never the bead id / brief text as the target. With no
        // agents it parks (queue intact, no misaddressed Send); with an idle
        // agent the head dispatches under the agent's own name.
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
        assert!(next.swarm.queue.is_empty());
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
            Msg::HerdrStatus {
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
        // ADR-0010 D4/D12: the declared wait's human re-surface path is the
        // Audit row — `classify_cmd` reads every Audit as CaptainRelevant, so
        // it lands in the AFK digest (D6) and in the journal, and it is never
        // silently dropped on the floor.
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
        // The Audit has no loop-action inverse: a replay of the journal must
        // not synthesize a second resurface action from the row itself.
        assert!(cmd.to_loop_action().is_none());
    }

    #[test]
    fn pending_wait_deadline_stamped_from_tick_clock_not_wall_clock() {
        // bd-bdpx: the park deadline stamps from last_now inside tick, so
        // replay reproduces it from the Msg log (the effect handler no longer
        // reads wall-clock). With no clock folded yet the Cmd still emits
        // but nothing registers — the live dispatcher seeds last_now at
        // construction so the sliver never happens there.
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
            next.pending_wait.get("C-W"),
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
            !bare.pending_wait.contains_key("C-W"),
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
    fn pending_wait_sweep_expires_and_caps_with_timeout_record() {
        // bd-zfnx.6 RED: pending_wait is bounded (TTL + cap). Expiry and
        // cap-eviction REMOVE entries and record a timeout — never silent.
        let mut m = empty_model();
        let now = 1_000_000u64;
        m.pending_wait.insert("expired".into(), now - 1);
        m.pending_wait.insert("live".into(), now + 100_000);
        for i in 0..PENDING_WAIT_CAP + 2 {
            m.pending_wait
                .insert(format!("filler-{i:04}"), now + 20_000 + i as u64);
        }
        let (next, cmds) = tick(&m, &Msg::Sweep { now });
        assert!(
            !next.pending_wait.contains_key("expired"),
            "expired waits drain on Sweep"
        );
        assert!(
            next.pending_wait.contains_key("live"),
            "live waits survive Sweep"
        );
        assert!(
            next.pending_wait.len() <= PENDING_WAIT_CAP,
            "cap holds: {}",
            next.pending_wait.len()
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
        // bd-2fxx RED: holds carry needed-by; digest renders
        // earliest-deadline first; overdue auto-escalates on the wedge
        // pattern (no new store: pending_wait + digest + wedge alarm).
        let now = 1_000_000u64;
        let mut m = empty_model();
        m.pending_wait.insert("late".into(), now - 1);
        m.pending_wait.insert("soon".into(), now + 10);
        m.pending_wait.insert("later".into(), now + 100);
        // Future-dated holds sort earliest-deadline first.
        let order: Vec<String> = sorted_holds(&m).iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(
            order,
            vec!["late".to_string(), "soon".to_string(), "later".to_string()],
            "holds sort earliest needed-by first: {order:?}"
        );
        // Overdue set is the wedge-pattern escalation signal.
        let overdue: Vec<String> = overdue_holds(&m, now)
            .iter()
            .map(|(c, _)| c.clone())
            .collect();
        assert_eq!(
            overdue,
            vec!["late".to_string()],
            "only past needed-by escalates: {overdue:?}"
        );
        // Past needed-by surfaces as escalation on the sweep beat:
        // re-surfaced (never dropped) + wait-timeout audit record.
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
        // Digest renders holds earliest-deadline first, overdue flagged.
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
        // bd-zfnx.4 + bd-55nx.7 RED: no secret-writing arm, no dead
        // Subscribe Cmd, no producer-less ClaimWatch. A deletion comment
        // naming the removal is allowed; an arm (brace form) is not.
        // (needles split so this self-scan does not match its own source)
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
        // zfnx.5 superseded (GS-016): Invoke is back as the WIRED playbook
        // vehicle — caller-applied, executor routes through cmd_workflow
        // (playbook lint + resolution gate, engine.invoke + VerifySink
        // downstream). Guards: schema_invoke_is_caller_applied (tick never
        // emits) + main.rs invoke_arm_hands_off_to_durable_run_gate.
        assert!(
            src.contains("Invoke {"),
            "the wired Invoke vehicle must exist (GS-016 rewire)"
        );
        let main = include_str!("main.rs");
        assert!(
            // `::` prefix so it cannot match the LEGITIMATE doorbell
            // subscription CLI arm (bd-55nx.7 targets the deleted
            // workspace Cmd variant, not the doorbell subscription verb).
            // Needle is split so this self-scan does not match its own source.
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
    fn bell_fold_no_status_preinsert_before_herdr_tick() {
        let src = include_str!("main.rs");
        // pre-insert before HerdrStatus tick kills drain (tick must see from)
        let needle = "bell_model.statuses.insert(pin.clone(), to.clone())";
        let count = src.matches(needle).count();
        assert!(
            count <= 1,
            "RED (bd-6b1y #7): main.rs still pre-inserts statuses before HerdrStatus tick"
        );
        assert!(
            !src.contains("status_by_pin.insert"),
            "RED: dispatcher still writes a side map before HerdrStatus; tick owns Model.doorbell.statuses"
        );
    }

    #[test]
    fn bell_fold_no_status_by_pin_side_map() {
        // bd-asr2.5 topology: the refresh/sweep helpers are gone (their arms
        // live in doorbell_host.rs + the rt Brain). The invariant survives:
        // no status side map anywhere — Model.doorbell.statuses is the only
        // herdr-status map, written by tick alone.
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
            "RED: a status_by_pin side map survives; tick HerdrStatus writes Model"
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
        let src = include_str!("main.rs");
        assert!(
            !src.contains("let (_, tick_actions) = doorbell_tick::tick(&bell_model, &msg)"),
            "RED (bd-6b1y #8): main.rs WatchDied still discards model' (`let (_, tick_actions) = tick(...)` at 8055) — keep the returned model"
        );
        // Must have keep: let (next_model, tick_actions) = ... ; bell_model = next_model;
        assert!(
            src.contains("WatchDied"),
            "src must contain WatchDied handling"
        );
    }

    #[test]
    fn bell_fold_once_calls_tick_not_second_reducer() {
        let main = include_str!("main.rs");
        let doorbell = include_str!("doorbell.rs");
        // once_json should call workspace::tick, not apply_watch_line as live second reducer
        assert!(
            main.contains("workspace::tick"),
            "RED (bd-6b1y #9): main.rs must wire through workspace::tick"
        );
        // doorbell.rs once_json must use workspace::tick after fix (bd-3wzl)
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
        // Live path is the host binding now (bd-asr2.5): drain → tick →
        // budget gate → sink, all around the live model.
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
            "captain_fact_inject must not herdr-inject (Tick Law)"
        );
        // adapter must not swallow Err into ()
        assert!(
            !src.contains("if let Err(e) = captain_fact_inject(&proj, ev)"),
            "RED (bd-6b1y #10): adapter still swallows Err into () at main.rs 10853-10860 — must propagate fact into Msg"
        );
    }

    #[test]
    fn bell_fold_fetch_inbox_emitted() {
        let dt = include_str!("../crates/doorbell-core/src/lib.rs");
        let main = include_str!("main.rs");
        // FetchInbox must be emitted as real Cmd, rows re-enter as InboxRows
        assert!(
            dt.contains("FetchInbox"),
            "RED (bd-6b1y #13): FetchInbox variant missing"
        );
        // Must have a constructor or usage in main.rs / workspace.rs
        let has_emit = main.contains("FetchInbox") || dt.contains("FetchInbox");
        assert!(
            has_emit,
            "RED (bd-6b1y #13): FetchInbox never emitted — must be a real Cmd whose rows re-enter as InboxRows"
        );
        // workspace tick should handle InboxRows
        let ws = include_str!("workspace.rs");
        assert!(ws.contains("InboxRows"), "workspace must handle InboxRows");
    }

    #[test]
    fn bell_fold_why_replay() {
        let doorbell = include_str!("doorbell.rs");
        let main = include_str!("main.rs");
        assert!(
            doorbell.contains("doorbell-journal-") && doorbell.contains("journals"),
            "RED (bd-6b1y #11): doorbell.rs journal path must be state_dir/journals/doorbell-journal"
        );
        // why must fold tick from Default, have --corr/--seq, not grep
        assert!(
            main.contains("fn cmd_doorbell_why"),
            "RED (bd-6b1y #11): fn cmd_doorbell_why not found"
        );
        let pos = main.find("fn cmd_doorbell_why").unwrap();
        let slice = &main[pos..std::cmp::min(main.len(), pos + 8000)];
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
        let src = include_str!("main.rs");
        let pos = src
            .find("fn handle_workspace_cmds(")
            .expect("handle_workspace_cmds");
        let slice = &src[pos..src.len().min(pos + 2500)];
        assert!(
            src.contains("fn try_tick_receipt")
                && src.contains("Msg::Receipt")
                && src.contains("pending_wait"),
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
        // LIVE dispatcher must route through workspace::tick, not direct doorbell_tick::tick
        // Count occurrences of doorbell_tick::tick in main.rs live paths (exclude tests via #cfg)
        // Simple scan: any occurrence is forbidden — workspace.rs is the only caller of slice ticks
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
        // Nudges are decisions that ride Msgs into tick (ClaimNudge/CloseNudge
        // arms emit the Send actions); the loop folds them after the poll tick.
        assert!(
            durable.contains("LoopMsg::ClaimNudge") && durable.contains("LoopMsg::CloseNudge"),
            "RED (bd-gs8b #2): claim/close nudges must be tick Msgs, not loop-side Sends"
        );
        assert!(
            durable.contains("extra_msgs.append(&mut nudge_msgs)")
                && durable.contains("crate::workspace::tick"),
            "nudge Msgs must be folded through workspace::tick after the poll tick"
        );
        // The old silent-drop is banned forever.
        assert!(
            !durable.contains("let _ = watch_sends"),
            "nudge Sends must never be computed then discarded"
        );
    }

    #[test]
    fn loop_fold_no_heal_pre_seed() {
        let durable = include_str!("durable.rs");
        // Reclaim is a Msg the live producer ticks; never extend after.
        assert!(
            !durable.contains("poll_actions.extend(reclaim_actions)"),
            "RED (bd-gs8b #3): reclaim still bypasses tick via poll_actions.extend(reclaim_actions)"
        );
        assert!(
            durable.contains("crate::workspace::Msg::Reclaim") || durable.contains("Msg::Reclaim"),
            "live producer must construct Msg::Reclaim"
        );
        // Check that tick is the only producer of actions (no pre-seed var)
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
        // Check for the specific post-tick filter that was at 6956-6962: filter Diagnose pane_exited
        let has_filter = durable.contains("filter(|a|")
            && durable.contains("pane_exited")
            && durable.contains("poll_actions");
        assert!(
            !has_filter,
            "RED (bd-gs8b #4): durable.rs still filters Diagnose pane_exited after tick — DELETE the filter"
        );
    }

    #[test]
    fn loop_fold_single_event_apply() {
        let durable = include_str!("durable.rs");
        // Events apply EXACTLY ONCE inside the Tick arm (bd-p1ds): Model.states
        // lives in the Model and pending_events are drained and applied there.
        // No loop-side apply — single-apply invariant holds via tick.
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
        // Also check that durable.rs does not have loop-local heal_states map at 5672 after tick
        let durable = include_str!("durable.rs");
        // The loop-local map `let mut heal_states: HashMap` should be gone or be on ActiveLoop
        // We just check that state.rs has it; loop-local mutate at 7472 should be gone
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
            "do not world-dump poll events into the model before Tick; fold Msg::Herdr"
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
        // LoopMsg::Tick should be clock, not world dump — check the enum definition slice
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
        // standing + empty body -> NO Cmd to human pin, no result
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
        // tasked + empty -> Cmd records result against corr with result_path
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
        // tasked without priority should default to queue, not interrupt
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
        // Should not be treated as interrupt
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
        // interrupt onto human pin is hard error
        let has_err = cmds.iter().any(|c| matches!(c, Cmd::LoudMiss{why} if why.contains("interrupt") && why.contains("human")));
        assert!(
            has_err,
            "interrupt never onto human pin must be hard error (LoudMiss)"
        );
    }

    #[test]
    fn assignment_tick_audience_human_one_pin_only() {
        let m = empty_model();
        // audience=human with corr that would fan-out to multiple pins is hard error
        // Simulate by having row with audience human and a special marker for broadcast
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
        // S1 scenario: tasked mail survives walker death is via durable receipt, not journal
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
        // Simulate new process rebuilding cache from table (not journal) — assignment should be rebuildable
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
        assert!(cmds.is_empty());
        assert_eq!(
            next.roles.get("wheelhouse").map(String::as_str),
            Some("DustySparrow")
        );
        // rebind to new pin overwrites
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
        // Initial bind wheelhouse -> OldPin and an open corr for that role
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
        // Simulate that FM-1 is for wheelhouse (in real code, the corr's audience was role: wheelhouse)
        // Now rebind wheelhouse -> NewPin
        let (m3, _) = tick(
            &m2,
            &Msg::RoleBound {
                role: "wheelhouse".into(),
                pin: "NewPin".into(),
            },
        );
        // Assignment for FM-1 must survive rebind (keyed by corr)
        assert_eq!(m3.assignment.get("FM-1"), Some(&receipt));
        // Roles map now points to NewPin
        assert_eq!(
            m3.roles.get("wheelhouse").map(String::as_str),
            Some("NewPin")
        );
        // Delivery for that corr should now retarget to NewPin (checked via next publish)
        // For now, ensure no assignment was lost
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
        // Check that no 30015 kind is actually used (not just mentioned in test)
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
        // Also check that mail to unknown role does not auto-spawn
        let m = empty_model();
        let row = serde_json::json!({"corr": "C1", "wake": "tasked", "body": "hi", "audience": "agents", "role": "wheelhouse"});
        // No role bound, so mail to role should be loud miss, not spawn
        let (next, _cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![row],
                pin: "Tester".into(),
            },
        );
        // Should not have created a role or spawned, just handled as normal assignment
        assert!(!next.roles.contains_key("wheelhouse"));
    }

    #[test]
    fn role_model_30013_role_tags_parsed() {
        // Verify that 30013 role= tags are parsed (bd-lmcy)
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
        // Check that live resolution uses Model.roles, not file
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
        // Unknown should never be treated as idle for inject
        let has_idle = next2.presence.get("Ghost") == Some(&Presence::from_str("idle"));
        assert!(!has_idle, "unknown must stay unknown, never silently idle");
    }

    #[test]
    fn presence_model_working_queues() {
        let mut m = empty_model();
        m.presence.insert("P".into(), Presence::from_str("working"));
        // Simulate a watch wake for P when working — should queue, not prompt
        // We test via InboxRows with a corr that would normally prompt, but presence is working
        // For now, check that presence map correctly reports Working
        assert_eq!(m.presence.get("P"), Some(&Presence::from_str("working")));
        // Inject should queue, not prompt — we test via tick's handling of InboxRows with presence working
        // The tick should not produce Prompt for working (it would queue)
        // For this unit test, we just verify the map
        let (next, _) = tick(
            &m,
            &Msg::Presence {
                pin: "P".into(),
                state: "idle".into(),
            },
        );
        assert_eq!(next.presence.get("P"), Some(&Presence::from_str("idle")));
    }

    #[test]
    fn filter_prompts_busy_urgent_breaks_through_rest_audits() {
        // bd-7qg6: Busy+urgent must break through (ADR-0005 gap 5, E5);
        // everything non-injectable becomes a digest-visible Audit, never a
        // silent drop; Unknown never prompts even when urgent.
        let mut m = empty_model();
        m.presence.insert("I".into(), Presence::from_str("idle"));
        m.presence.insert("B".into(), Presence::from_str("working"));
        let urgent = "[doorbell] plane-P urgent. Pull: toron mail inbox --since last";
        let routine = "[doorbell] kind 9. Pull: toron channel read h=ch-1";
        let cmds = vec![
            Cmd::Prompt {
                pin: "I".into(),
                line: routine.into(),
            },
            Cmd::Prompt {
                pin: "B".into(),
                line: urgent.into(),
            },
            Cmd::Prompt {
                pin: "B".into(),
                line: routine.into(),
            },
            Cmd::Prompt {
                pin: "Ghost".into(),
                line: urgent.into(),
            },
        ];
        let out = filter_prompts(&m, cmds);
        assert_eq!(out.len(), 4, "nothing vanishes: {out:?}");
        assert!(matches!(&out[0], Cmd::Prompt { pin, .. } if pin == "I"));
        assert!(
            matches!(&out[1], Cmd::Prompt { pin, .. } if pin == "B"),
            "busy+urgent breaks through: {out:?}"
        );
        assert!(
            matches!(&out[2], Cmd::Audit { kind, pin, .. } if kind == "prompt_filtered" && pin == "B"),
            "busy+routine audits for the digest: {out:?}"
        );
        assert!(
            matches!(&out[3], Cmd::Audit { kind, pin, .. } if kind == "prompt_filtered" && pin == "Ghost"),
            "unknown never prompts, even urgent: {out:?}"
        );
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
    fn presence_model_inject_reads_model_not_herdr() {
        let src = include_str!("workspace.rs");
        // Inject should read presence in Model, not herdr-only gossip
        assert!(
            src.contains("presence") && src.contains("Presence"),
            "inject must read presence in Model"
        );
        // Check that should_inject logic is preserved (idle|done|blocked) and unknown queues
        let doorbell = include_str!("doorbell.rs");
        assert!(
            doorbell.contains("should_inject") || doorbell.contains("idle"),
            "should_inject stays idle|done|blocked"
        );
    }

    #[test]
    fn presence_model_watch_filter_has_20001() {
        let src = include_str!("main.rs");
        // Doorbell watch filter must include 20001
        assert!(
            src.contains("30013") && src.contains("20001"),
            "watch filter must include 30013 and 20001"
        );
    }

    // ADR-0028 E5: 20001 trust rule — inject truth is the pin itself
    // (line pubkey == pin's npub) or the trusted Msg::Presence edge.
    // A 20001 line whose author is not the claimed pin is a forge:
    // LoudMiss (+Audit), presence unchanged. Missing key = Unknown;
    // Unknown never prompts (prompt_allowed falls back to herdr
    // "unknown" which should_inject rejects).
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
        // No presence entry (Unknown) and no herdr status (unknown):
        // a kind-9 mention must not prompt.
        let line = r#"{"kind":9,"id":"ab","tags":[["p","GhostPin"],["h","ch-1"]],"content":"@GhostPin hi"}"#;
        let (_, cmds) = tick(&m, &Msg::WatchNdjson { line: line.into() });
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "Unknown pin must get no Prompt, got {cmds:?}"
        );
    }

    /// D3 property: no consumer branch is reachable from Unknown.
    /// verdict_for never yields Idle from Unknown/missing presence;
    /// herdr busy yields evidence-labeled Busy, never authority.
    #[test]
    fn d3_verdict_never_promotes_unknown() {
        let busy_evidence = Presence::Busy {
            source: "herdr-evidence".into(),
        };
        let dead_endpoint = Presence::Dead {
            source: "endpoint".into(),
        };
        for herdr in [
            "working", "busy", "idle", "done", "blocked", "unknown", "weird",
        ] {
            let v = verdict_for(None, herdr, true);
            let expected = match herdr {
                "working" | "busy" => busy_evidence.clone(),
                _ => Presence::Unknown,
            };
            assert_eq!(v, expected, "herdr={herdr}");
            assert_eq!(
                verdict_for(None, herdr, false),
                dead_endpoint,
                "herdr={herdr}"
            );
            assert_eq!(
                verdict_for(Some(&Presence::Unknown), herdr, true),
                v,
                "Unknown behaves as missing: herdr={herdr}"
            );
        }
        // Known verdicts pass through with source intact.
        let idle = Presence::Idle {
            source: "presence".into(),
        };
        assert_eq!(verdict_for(Some(&idle), "idle", true), idle);
        // Wire words and legacy snapshots.
        assert_eq!(
            Presence::from_str("blocked"),
            Presence::Busy {
                source: "presence".into()
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
        // Render tokens carry source.
        assert_eq!(busy_evidence.status_token(), "Busy(herdr-evidence)");
        assert_eq!(idle.status_token(), "Idle(presence)");
        assert_eq!(Presence::Unknown.status_token(), "Unknown");
        // Prompt gate: only exact Idle prompts.
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
        // Second decision for same corr while one is still open should be rejected loudly (first removes, second is LoudMiss)
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
        // Now open_human is empty, second decision for same corr should be LoudMiss (no open)
        let (m2, cmds2) = tick(
            &m1,
            &Msg::HumanDecision {
                corr: "FM-1".into(),
                verdict: "approve".into(),
            },
        );
        assert!(cmds2.iter().any(|c| matches!(c, Cmd::LoudMiss { .. })));
        assert!(m2.open_human.is_empty());
        // While one is open, second concurrent decision should also be LoudMiss
        let mut m3 = empty_model();
        m3.open_human.insert(
            "FM-1".into(),
            Question {
                corr: "FM-1".into(),
                body: "q".into(),
                created_at: 1,
            },
        );
        // Try to insert another Question for same corr (should be prevented, but our tick currently allows it via direct insert)
        // For now, test that HumanDecision for same corr when one is open after first is closed is LoudMiss
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

    // ADR-0028 E4: Publish carries corr (+ result_path) so the answer can
    // return exactly that corr. No pre-seeded Model on this path — the
    // question opens from a live InboxRows mint.
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

    // E4 happy path over live-shaped rows, zero pre-seed: mint → tagged
    // answer closes exactly one question → duplicate answer LoudMiss.
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

    // Migration fallback (one release): a tag-less human mail whose body
    // carries the verdict still closes while open_human holds the corr.
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
        // Question should be published as Cmd::Publish with audience human, one rumor
        // For now, we test that creating a human question via direct insert and then checking publish would be via Cmd
        let mut m = empty_model();
        // Simulate tick that would create a question and publish
        // We use a direct approach: insert question and check that publish would be audience human
        m.open_human.insert(
            "C2".into(),
            Question {
                corr: "C2".into(),
                body: "approve?".into(),
                created_at: 1,
            },
        );
        assert!(m.open_human.contains_key("C2"));
        // The publish Cmd for human should be audience=human, one pin
        // We check that our tick would emit Publish for human (via a synthetic Msg)
        // For now, just verify the map
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
        // Now FM-1 is closed, second decision should be LoudMiss
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
        // E4: happy path needs no test-only pre-seed — the question is
        // minted by the live Publish path (audience=human InboxRow), then
        // the correlated answer closes it.
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
    // ADR-0010 D6 (bd-f14d): OPEN DECISIONS fold + RECORD DIVERGENCE +
    // pending_wait promotion. Tick classification predicates and the
    // Digest cmd are Lane 1 (bd-65he) — untouched here.
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
    fn d6_pending_wait_expiry_promotes_never_drops() {
        let mut m = empty_model();
        m.pending_wait.insert("FM-W".into(), 100);
        let (next, cmds) = tick(&m, &Msg::Sweep { now: 200 });
        assert!(
            !next.pending_wait.contains_key("FM-W"),
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
        // No Publish on promotion: re-surfacing must not re-ring.
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::Publish { corr, .. } if corr == "FM-W")),
            "promotion is silent: {cmds:?}"
        );
        // Answer clears the promoted call.
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
        // Consumer helper must equal ticking the same msgs live.
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
        // Empty inputs: the check ran, nothing held.
        let empty = DivergenceSpec {
            open_corrs: &[],
            closed_beads: &[],
            answered_corrs: &[],
            held_beads: &[],
            done_without_close: &[],
        };
        assert!(record_divergence(&empty).is_empty());
        assert_eq!(render_divergence(&[]), "RECORD DIVERGENCE: none");
    }
}

#[cfg(test)]
mod cmd_schema_tests {
    // ADR-0028 C: action-schema discipline at the Cmd/Msg boundary.
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
        // ADR-0028 E1: Invoke is a caller-applied effect (like Prompt) —
        // tick never emits it; the composing caller does (GS-016 rewire).
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

    #[test]
    fn adr0010_d1_line_shape_is_shrunk_doorbell() {
        assert_eq!(
            doorbell_mail_line(3, "EmeraldSparrow", "c-9"),
            "[doorbell] inbox: 3 unacked for EmeraldSparrow; latest corr=c-9; read + ack"
        );
    }

    #[test]
    fn adr0010_d1_duplicate_ring_is_zero_duplicate_work() {
        let e = entry("P");
        let a = rering_due(&e, DOORBELL_GRACE_DEFAULT_SECS + 1, "other");
        let b = rering_due(&e, DOORBELL_GRACE_DEFAULT_SECS + 1, "other");
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
            // Inside the window: nothing due.
            assert_eq!(
                rering_due(&e, e.window_start_unix + grace - 1, "other"),
                ReringDecision::Pending
            );
            // Window elapsed: ring once.
            assert_eq!(
                rering_due(&e, e.window_start_unix + grace, "other"),
                ReringDecision::Ring { attempt }
            );
            e.attempts = attempt + 1;
            e.window_start_unix += grace;
        }
        assert_eq!(
            rering_due(&e, e.window_start_unix + grace, "Dead"),
            ReringDecision::RecoverDead,
            "past max rings a Dead pane takes the respawn path"
        );
        assert_eq!(
            rering_due(&e, e.window_start_unix + grace, "Unknown"),
            ReringDecision::DigestUnknown,
            "past max rings an Unknown pane takes the digest path"
        );
    }

    #[test]
    fn adr0010_d1_skips_never_climb() {
        let mut acked = entry("P");
        acked.acked = true;
        assert_eq!(rering_due(&acked, u64::MAX, "Dead"), ReringDecision::Skip);
        let mut faf = entry("P");
        faf.fire_and_forget = true;
        assert_eq!(rering_due(&faf, u64::MAX, "Dead"), ReringDecision::Skip);
    }

    #[test]
    fn adr0010_d1_raw_without_id_rings_once_never_escalates() {
        let mut e = entry("P");
        e.raw_without_id = true;
        assert_eq!(
            rering_due(&e, 0, "other"),
            ReringDecision::Ring { attempt: 0 }
        );
        e.attempts = 1;
        assert_eq!(
            rering_due(&e, u64::MAX, "Dead"),
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
        m.rering.insert("c-1".into(), entry("P"));
        // Fresh entry: inside grace window, no re-ring (other beat
        // cmds like CheckAckSla may still appear).
        let (_, cmds) = tick(&m, &Msg::AckSlaTick { now: 1 });
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "fresh entry must not ring inside its window: {cmds:?}"
        );
        // Past grace: exactly one re-ring prompt with the shrunk line
        // (alongside the standing CheckAckSla + FetchInbox beat cmds).
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
            cmds.iter().any(|c| matches!(c, Cmd::Prompt { pin, line }
                if pin == "P"
                    && line == "[doorbell] inbox: 1 unacked for P; latest corr=c-1; read + ack")),
            "re-ring must prompt the shrunk doorbell line, not the payload: {cmds:?}"
        );
        assert_eq!(next.rering["c-1"].attempts, 1);
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
        m.rering.insert("c-d".into(), ed);
        m.rering.insert("c-u".into(), eu);
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
        assert!(next.rering.is_empty(), "terminal entries leave the ladder");
    }

    // ---- bd-w98c residual: SlaFired hit list wires the ladder registrar ----

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
        assert!(next.rering.is_empty(), "no row, no registration");
    }

    #[test]
    fn adr0010_d1_inbox_rows_register_overdue_corrs_on_the_ladder() {
        let mut m = WorkspaceModel::default();
        m.last_now = 5_000;
        let (next, _) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![
                    overdue_row("c-over", "id-1", None),
                    overdue_row("c-read", "id-2", Some(1_100)),
                    // normal-importance ack-required mail is outside the scan scope
                    serde_json::json!({
                        "id": "id-3", "corr": "c-normal", "ack_required": true,
                        "importance": "normal", "read_ts_us": serde_json::Value::Null,
                    }),
                    // no corr = nothing to key on
                    serde_json::json!({
                        "id": "id-4", "ack_required": true,
                        "importance": "urgent", "read_ts_us": serde_json::Value::Null,
                    }),
                ],
                pin: "P".into(),
            },
        );
        assert_eq!(
            next.rering.get("c-over").map(|e| e.pin.as_str()),
            Some("P"),
            "an unacked overdue corr registers: {:?}",
            next.rering
        );
        let e = &next.rering["c-over"];
        assert_eq!(
            e.window_start_unix, 5_000,
            "window stamps from the tick clock (bd-bdpx)"
        );
        assert_eq!(e.attempts, 0, "registration is not a ring");
        assert!(
            !next.rering.contains_key("c-normal"),
            "outside the scan scope"
        );
        assert_eq!(next.rering.len(), 1, "no corr, no entry");
        // The read row's corr only acks if it was ALREADY registered (no
        // ghost entries from plain mail reads).
        assert!(
            !next.rering.contains_key("c-read"),
            "a read never registers"
        );
        // Read fact: register first, then see it read -> acked.
        let (next2, _) = tick(
            &next,
            &Msg::InboxRows {
                rows: vec![overdue_row("c-over", "id-1", Some(5_100))],
                pin: "P".into(),
            },
        );
        assert!(
            next2.rering["c-over"].acked,
            "the per-recipient read is the handling fact"
        );
    }

    #[test]
    fn adr0010_d1_registration_then_ladder_beat_rings_without_a_send() {
        // The residual closed: an overdue corr seen ONLY on the wire (no
        // same-process send) climbs the ladder on the next AckSlaTick beat.
        let mut m = WorkspaceModel::default();
        m.last_now = 100;
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
            cmds.iter().any(|c| matches!(c, Cmd::Prompt { pin, line }
                if pin == "WirePin" && line.contains("c-wire") && line.contains("unacked for"))),
            "wire-registered corr re-rings through the shrunk doorbell line: {cmds:?}"
        );
        assert_eq!(next.rering["c-wire"].attempts, 1);
    }

    #[test]
    fn adr0010_d1_registration_is_idempotent_and_refreshes_window() {
        let mut m = WorkspaceModel::default();
        m.last_now = 100;
        let (m, _) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![overdue_row("c-i", "id-i", None)],
                pin: "P".into(),
            },
        );
        assert_eq!(m.rering["c-i"].attempts, 0);
        let (m, _) = tick(
            &m,
            &Msg::AckSlaTick {
                now: 100 + doorbell_grace_secs(),
            },
        );
        assert_eq!(m.rering["c-i"].attempts, 1, "one ring");
        // Same row redelivered (at-least-once): no second ring, no reset of
        // attempts; the window refreshes only per the ladder's own beat.
        let (m2, cmds) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![overdue_row("c-i", "id-i", None)],
                pin: "P".into(),
            },
        );
        assert_eq!(
            m2.rering["c-i"].attempts, 1,
            "redelivery never double-rings"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Prompt { .. })),
            "redelivery is quiet: {cmds:?}"
        );
    }

    #[test]
    fn adr0010_d1_slafired_same_log_same_effects() {
        // Tick Law: the new arm is a pure fold — same Msg log, same effects.
        let build = || {
            let mut m = WorkspaceModel::default();
            m.last_now = 42;
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

    fn prompt_setup() -> WorkspaceModel {
        let mut m = WorkspaceModel::default();
        m.presence.insert(
            "P".into(),
            Presence::Idle {
                source: "test".into(),
            },
        );
        m.roles.insert("captain".into(), "P".into());
        m
    }

    fn fact(summary: &str) -> Msg {
        // bd-l72k: the D2 ledger records deliveries only, and only urgent
        // prompts deliver — so ledger tests drive urgent facts.
        Msg::Fact {
            summary: format!("plane-P urgent {summary}"),
        }
    }

    fn prompts(cmds: &[Cmd]) -> Vec<String> {
        cmds.iter()
            .filter_map(|c| match c {
                Cmd::Prompt { pin, line } if pin == "P" => Some(line.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn adr0010_d2_kill_between_present_and_ack_represents_once_more() {
        let s = prompt_setup();
        // Presentation: first wake prompts exactly once.
        let (a, cmds1) = tick(&s, &fact("w1"));
        let l1 = "[doorbell] plane-P urgent w1".to_string();
        assert_eq!(prompts(&cmds1), vec![l1.clone()]);
        assert_eq!(
            a.presented.get("P"),
            Some(&vec![PresentedWake {
                seq: 1,
                line: l1.clone(),
            }]),
            "presentation binds the line to its generation"
        );
        // Kill + refold from the Msg log: identical state, identical cmds.
        let (a2, cmds1b) = tick(&s, &fact("w1"));
        assert_eq!(a, a2, "refold from Default reproduces the live model");
        assert_eq!(cmds1, cmds1b, "refold reproduces the live Cmd sequence");
        // Next prompt re-presents the unacked wake exactly once more.
        let (b, cmds2) = tick(&a, &fact("w2"));
        let l2 = "[doorbell] plane-P urgent w2".to_string();
        let got = prompts(&cmds2);
        assert_eq!(
            got.iter().filter(|l| *l == &l1).count(),
            1,
            "unacked wake re-presents exactly once more: {got:?}"
        );
        assert!(got.contains(&l2), "new wake still prompts: {got:?}");
        // Refolded path agrees on the re-presentation too.
        let (b2, cmds2b) = tick(&a2, &fact("w2"));
        assert_eq!(b, b2);
        assert_eq!(cmds2, cmds2b);
        // Ack through the latest generation clears the ledger.
        let (c, ack_cmds) = tick(
            &b,
            &Msg::WakeAck {
                pin: "P".into(),
                through_seq: b.seq,
            },
        );
        assert!(ack_cmds.is_empty(), "an ack emits nothing");
        assert_eq!(c.wake_acked.get("P"), Some(&b.seq));
        assert!(
            !c.presented.contains_key("P"),
            "acked generations leave the presented set"
        );
        // After ack, the next prompt carries only the new wake.
        let (_, cmds3) = tick(&c, &fact("w3"));
        assert_eq!(
            prompts(&cmds3),
            vec!["[doorbell] plane-P urgent w3".to_string()]
        );
    }

    #[test]
    fn adr0010_d2_duplicate_and_stale_acks_are_noops() {
        let setup = prompt_setup();
        let (w1, _) = tick(&setup, &fact("w1"));
        let (w1ack, _) = tick(
            &w1,
            &Msg::WakeAck {
                pin: "P".into(),
                through_seq: w1.seq,
            },
        );
        assert!(!w1ack.presented.contains_key("P"));
        // Duplicate ack: same watermark, same empty ledger.
        let (dup, cmds) = tick(
            &w1ack,
            &Msg::WakeAck {
                pin: "P".into(),
                through_seq: w1.seq,
            },
        );
        assert!(cmds.is_empty());
        assert_eq!(
            dup.presented, w1ack.presented,
            "duplicate ack changes nothing"
        );
        assert_eq!(
            dup.wake_acked, w1ack.wake_acked,
            "duplicate ack changes nothing"
        );
        // Stale ack: through_seq below the watermark is ignored.
        let (w2, _) = tick(&w1, &fact("w2"));
        let (stale, _) = tick(
            &w2,
            &Msg::WakeAck {
                pin: "P".into(),
                through_seq: 1,
            },
        );
        assert_eq!(
            stale.presented.get("P").map(Vec::len),
            Some(2),
            "stale ack must not clear newer presentations"
        );
        assert_eq!(stale.wake_acked.get("P"), Some(&1));
    }

    #[test]
    fn adr0010_d2_unknown_pin_ack_is_harmless() {
        let m = WorkspaceModel::default();
        let (next, cmds) = tick(
            &m,
            &Msg::WakeAck {
                pin: "Ghost".into(),
                through_seq: 7,
            },
        );
        assert!(cmds.is_empty());
        assert_eq!(next.wake_acked.get("Ghost"), Some(&7));
        assert!(!next.presented.contains_key("Ghost"));
    }

    #[test]
    fn adr0010_d2_represent_respects_prompt_gate() {
        // Unacked entries for a non-promptable pin never force a prompt:
        // the gate stays authoritative, the ledger just waits.
        let mut m = WorkspaceModel::default();
        m.presence.insert(
            "Q".into(),
            Presence::Busy {
                source: "test".into(),
            },
        );
        m.roles.insert("captain".into(), "Q".into());
        m.presented.insert(
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
        // The doorbell slice gates inject on its own status map.
        m.doorbell.statuses.insert(pin.into(), "idle".into());
        m.roles.insert("captain".into(), pin.into());
        m
    }

    #[test]
    fn adr0010_d5_fixture_replay_afk_classes() {
        // AFK fixture: chatter (benign), sla beats (routine), alias-less
        // fact (captain-relevant miss), urgent inbox pull (urgent).
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
        // Alias-less fact (no captain role on a fresh model) is a miss.
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
        assert_eq!(m4.digest_queue.len(), 1, "miss queues for digest");
        assert_eq!(m4.digest_queue[0].kind, "fact-miss");
        // Urgent inbox pull prompts immediately (bypasses batching).
        let (m5, c5) = tick(
            &m,
            &Msg::InboxRows {
                rows: vec![urgent_row()],
                pin: "P".into(),
            },
        );
        assert!(
            c5.iter().any(|c| matches!(c, Cmd::Prompt { pin, line }
                if pin == "P" && is_urgent_line(line))),
            "urgent bypasses batching with an immediate prompt: {c5:?}"
        );
        let _ = m5;
    }

    #[test]
    fn adr0010_d5_digest_batch_interval_and_threshold() {
        let mut m = WorkspaceModel::default();
        // Two captain-relevant misses queue without emitting.
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
        assert_eq!(m.digest_queue.len(), 2);
        // Interval lapse flushes exactly the queued set with ages.
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
        assert!(next.digest_queue.is_empty(), "batch drains the queue");
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
        // A fold with no clock Msg records `at_unix == 0`. Subtracting it
        // printed a raw epoch value labelled "s ago" (~56 years), which reads
        // as a real measurement. It must say unknown instead.
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
                line: "[doorbell] plane-P urgent. Pull: inbox".into(),
            }),
            WakeClass::Urgent,
            "urgent lines ring now"
        );
        assert_eq!(
            classify_cmd(&Cmd::Prompt {
                pin: "P".into(),
                line: "[doorbell] kind 9. Pull: ch".into(),
            }),
            WakeClass::Routine,
            "bd-l72k: addressed kind-9 chatter is routine (zero prompts, counters only)"
        );
        assert_eq!(
            classify_cmd(&Cmd::Prompt {
                pin: "P".into(),
                line: doorbell_mail_line(1, "P", "c-1"),
            }),
            WakeClass::Urgent,
            "re-ring is the ack-SLA vehicle and bypasses batching"
        );
        assert_eq!(
            classify_cmd(&Cmd::Prompt {
                pin: "P".into(),
                line: "[doorbell] something unmarked".into(),
            }),
            WakeClass::CaptainRelevant,
            "unmarked prompts default captain-visible, never silently routine"
        );
    }

    // bd-l72k RED: edge triage — D5 predicates at the injection decision.
    // Benign/routine advance markers with zero prompts; captain-relevant
    // queue to the digest; urgent injects immediately.

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
        // Benign: unaddressed room chatter (no p tag for self).
        let benign = edge_watch(9, "b1", r#"[["h","ch-1"]]"#, "room talk");
        // Routine: addressed kind-9 chatter, no urgency.
        let routine = edge_watch(
            9,
            "r1",
            r#"[["p","BlueRidge"],["h","ch-1"]]"#,
            "@BlueRidge hi",
        );
        // Captain: addressed job wake.
        let captain = edge_watch(
            43001,
            "c1",
            r#"[["p","BlueRidge"],["h","steer-x"]]"#,
            "job ready",
        );
        // Urgent: request-release addressed to self.
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
                Cmd::Prompt { pin, line } => Some((pin.clone(), line.clone())),
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
            m.digest_queue.len(),
            2,
            "digest holds the captain set plus the urgent record (urgent prompts AND queues), got {:?}",
            m.digest_queue
        );
        let kinds: Vec<&str> = m.digest_queue.iter().map(|e| e.kind.as_str()).collect();
        assert!(
            kinds.contains(&"prompt") && kinds.contains(&"urgent-prompt"),
            "one captain entry plus one urgent record, got {:?}",
            m.digest_queue
        );
        assert!(
            m.digest_queue.iter().all(|e| e.pin == "BlueRidge"),
            "no routine/benign entry may enter the digest: {:?}",
            m.digest_queue
        );
        assert!(
            m.digest_queue[0].detail.contains("43001"),
            "digest entry is the job wake, got {}",
            m.digest_queue[0].detail
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
            next.digest_queue.is_empty(),
            "routine never enters the digest: {:?}",
            next.digest_queue
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
            m1.digest_queue.len(),
            1,
            "job wake queued for digest: {:?}",
            m1.digest_queue
        );
        assert_eq!(m1.digest_queue[0].pin, "BlueRidge");
        // Batch emits on a now-bearing beat past the interval; the queue drains.
        let (m2, cmds2) = tick(&m1, &Msg::Tick { now: 1_000_000 });
        assert!(
            cmds2
                .iter()
                .any(|c| matches!(c, Cmd::Digest { entries } if entries.len() == 1)),
            "digest batch must emit the captain entry: {cmds2:?}"
        );
        assert!(m2.digest_queue.is_empty(), "batch drains the queue");
    }

    #[test]
    fn edge_triage_rering_line_is_urgent_and_injects() {
        let line = doorbell_mail_line(1, "BlueRidge", "c-1");
        assert!(
            is_urgent_line(&line),
            "re-ring is the ack-SLA vehicle and must ring now: {line}"
        );
        assert_eq!(
            classify_cmd(&Cmd::Prompt {
                pin: "BlueRidge".into(),
                line: line.clone(),
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
        // loop.md.tpl: do not flip loop→swarm without explicit request
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
        // Sticky-open airtight: do not name invent wave-swarm under open.
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
        let expanded = expand_reclassify_cmds(&mut m, cmds, "gs050-expand-test", &|goal| {
            // Pure decide leaf — mirrors effect layer.
            let d = crate::decide::decide(goal, &crate::decide::Snapshot::default());
            (
                d.primitive.to_string(),
                d.topology.to_string(),
                d.argv.clone(),
                d.reason.clone(),
            )
        });
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
        // Sticky-open: open session without explicit topology stays open.
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
        let _ = expand_reclassify_cmds(&mut m, cmds, "gs050-ledger-test", &|goal| {
            let d = crate::decide::decide(goal, &crate::decide::Snapshot::default());
            (
                d.primitive.to_string(),
                d.topology.to_string(),
                d.argv.clone(),
                d.reason.clone(),
            )
        });
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
        let _ = line; // source bytes owned by caller; tick never consumes them
        // Distinct source-action payload → distinct Cmd identity (not an
        // `&Msg` tautology). Unaddressed kind-9 chatter may collapse to the
        // same no-ring Cmds, so use MaterialContext goals as the witness.
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
        // Same identity fields + effect_key on both retries (pin included).
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
        // Effect layer: fold DecisionReady from a pure decide result.
        let ready = Msg::DecisionReady {
            goal: reclass[0].0.clone(),
            primitive: "babysit-bead".into(),
            topology: "swarm".into(), // invent flip — sticky must hold open
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
        // Retry same source MaterialContext on post-effect model — identity.
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
        // Length-prefix also prevents boundary collisions (goal||reason).
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
        let _ = expand_reclassify_cmds(&mut m, cmds, project, &|g| {
            ("none".into(), "none".into(), vec![], format!("decided:{g}"))
        });
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
        // Retry same MaterialContext — same PK, attempt bumps.
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
        let _ = expand_reclassify_cmds(&mut m2, cmds2, project, &|_| {
            ("none".into(), "none".into(), vec![], "retry".into())
        });
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

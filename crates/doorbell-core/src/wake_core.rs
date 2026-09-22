// governed-by: ADR-0035
//! Fenced wake decision core (ADR-0035 Specs 1+5, bd-0nem).
//!
//! Consolidation of the wake decision state (bd-0cet flood
//! table, bd-fuql routing vocabulary) into a dependency-free module:
//! **std + serde only**. No tokio, no Nostr client types, no filesystem,
//! clock, env, or sibling-crate imports (fenced by test below).
//!
//! ## Law
//!
//! One `Model`, one `Msg` union, one [`tick`]: `tick(&Model, &Msg) ->
//! (Model, Vec<Effect>)`. Zero I/O — wall-clock enters only as Msg
//! payloads; sensors stay Msg producers; the thin hosts (durable loop,
//! daemon file-host) execute effects. Same Msg log → byte-identical
//! Effects (tested).
//!
//! - [`Guard`]s are named pure predicates — no inline boolean soup in
//!   [`tick`] arms.
//! - [`WakeEffect`]s are parameterized data — no closures, no captured
//!   environment inside the machine.
//! - [`machine_config`] serializes the machine (states, transitions, guard
//!   names) to JSON so a future visualizer renders it without the runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// One presence-word verdict (bd-z5ou): the single owner of the
/// 20001/jsonl state-word map. Both hosts read through this — the
/// workspace `Presence` adds the evidence source, the daemon host reads
/// the verdict directly. blocked reads as Busy (occupied, digest not
/// prompt); done reads as Idle; anything unparseable is Unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceVerdict {
    Idle,
    Busy,
    Dead,
    Unknown,
}

pub fn parse_presence_word(s: &str) -> PresenceVerdict {
    match s.trim().to_ascii_lowercase().as_str() {
        "idle" | "done" => PresenceVerdict::Idle,
        "working" | "busy" | "blocked" => PresenceVerdict::Busy,
        "dead" | "endpoint-dead" | "endpoint" => PresenceVerdict::Dead,
        _ => PresenceVerdict::Unknown,
    }
}

/// One typed inject gate (bd-zawk G6): the single owner of the
/// inject-vs-digest decision for BOTH the herdr-string path
/// (`route_or_queue`, `is_drain_transition` inputs) and the presence-verdict
/// path (`apply_presence_filter`). blocked parses to Busy, so blocked+urgent
/// injects while blocked+routine digests with a reason — never a silent drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectGate {
    Inject,
    Digest(&'static str),
}

/// Verdict + urgency -> decision. Idle always injects; Busy breaks through
/// only for urgent lines; Dead/Unknown always digest with a reason.
// governed-by: bd-zawk
pub fn inject_gate(verdict: PresenceVerdict, urgent: bool) -> InjectGate {
    match (verdict, urgent) {
        (PresenceVerdict::Idle, _) => InjectGate::Inject,
        (PresenceVerdict::Busy, true) => InjectGate::Inject,
        (PresenceVerdict::Busy, false) => {
            InjectGate::Digest("non-injectable presence held wake for digest")
        }
        (PresenceVerdict::Dead, _) => InjectGate::Digest("dead presence held wake for digest"),
        (PresenceVerdict::Unknown, _) => {
            InjectGate::Digest("unknown presence held wake for digest")
        }
    }
}

/// Herdr-status string + line urgency -> decision. Parses through
/// `parse_presence_word` so blocked->Busy and done->Idle agree with the
/// presence path by construction.
// governed-by: bd-zawk
pub fn inject_gate_for_herdr(herdr_status: &str, urgent: bool) -> InjectGate {
    inject_gate(parse_presence_word(herdr_status), urgent)
}

/// More than this many previously-unseen ready beads in ONE poll is the
/// runaway-create signature (bd-eign.2: 40 junk beads in ~4 min; legit
/// ADR-0022 waves are 5-6). Matches the retired `bead_flood_verdict`.
pub const FLOOD_NEW_THRESHOLD: usize = 6;

/// One wake's lifecycle (Spec 5: queued → presented → acked | digested
/// | expired). Terminal rows leave the table; the flood/sla seen-sets
/// below are the cross-resume memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WakeStatus {
    Queued,
    Presented,
    Acked,
    Digested,
    Expired,
}

/// The persisted table: live rows plus the two seen-sets that must
/// survive crash-resume (flood guard continuity + AckSLA continuity).
/// Dispatcher working sets ride `waits`/`ladders`/`shown`/`digests`
/// (bd-fuql absorption) — one home, one decision, one why.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WakeModel {
    /// Bead ids the flood guard already counted (pause absorbs them).
    pub flood_seen: BTreeSet<String>,
    /// Corrs the AckSLA ladder already pinged (no re-ping on resume).
    pub sla_seen: BTreeSet<String>,
    /// Live wake rows by id (terminal transitions remove the row).
    pub wakes: BTreeMap<String, WakeStatus>,
    /// WaitCorr holds: corr -> needed-by unix secs.
    pub waits: BTreeMap<String, u64>,
    /// Re-ring ladder entries by corr.
    pub ladders: BTreeMap<String, ReringEntry>,
    /// Shown-but-unacked wakes per pin.
    pub shown: BTreeMap<String, Vec<PresentedWake>>,
    /// Captain-relevant wakes awaiting the afk digest batch.
    pub digests: Vec<DigestEntry>,
}

impl WakeModel {
    /// True when nothing is tracked (serde skip guard for ActiveLoop).
    pub fn is_empty(&self) -> bool {
        self.flood_seen.is_empty()
            && self.sla_seen.is_empty()
            && self.wakes.is_empty()
            && self.waits.is_empty()
            && self.ladders.is_empty()
            && self.shown.is_empty()
            && self.digests.is_empty()
    }

    /// Flood-guard memory depth (test observable).
    pub fn flood_seen_len(&self) -> usize {
        self.flood_seen.len()
    }

    /// Ladder step through the table (bd-fuql): folds the pure verdict
    /// for a tracked corr. Unknown corrs read Pending — never invent
    /// tracking for a stranger.
    pub fn ladder_step(
        &mut self,
        corr: &str,
        now: u64,
        verdict: &str,
        grace_secs: u64,
        ring_max: u32,
    ) -> ReringDecision {
        match self.ladders.get(corr) {
            Some(entry) => rering_due_with_policy(entry, now, verdict, grace_secs, ring_max),
            None => ReringDecision::Pending,
        }
    }

    /// Mark one wake presented (idempotent on unknown ids — never
    /// invents a row for a stranger).
    pub fn present(&mut self, id: &str) {
        self.wakes.insert(id.to_string(), WakeStatus::Presented);
    }

    /// Terminal transitions remove the row (the seen-sets keep history).
    pub fn ack(&mut self, id: &str) {
        if self.wakes.get(id) == Some(&WakeStatus::Presented) {
            self.wakes.remove(id);
        }
    }

    /// Digest is terminal like ack (stays visible in digest history
    /// elsewhere; the table only tracks live routing).
    pub fn digest(&mut self, id: &str) {
        self.wakes.remove(id);
    }

    /// Expiry is terminal (a re-ring re-queues explicitly, never via
    /// the stale row).
    pub fn expire(&mut self, id: &str) {
        self.wakes.remove(id);
    }
}

/// The flood guard's verdict (bd-0cet plane: dispatch Proceed vs Pause).
/// Named `FloodGate` (not `GateDecision`) on purpose: the Spec-5
/// `GateDecision` vocabulary (Inject/Digest/Queue/Drop, below) governs
/// wake ROUTING; this one governs bead-DISPATCH pausing. Two planes,
/// two names, no conflation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloodGate {
    /// At or below threshold (or nothing unseen) — dispatch proceeds.
    Proceed,
    /// Over threshold — pause dispatch this poll; the listed ids are
    /// now absorbed, so a legit batch dispatches next poll while a
    /// runaway minting fresh ids keeps pausing.
    Pause { unseen: Vec<String> },
}

/// Pure flood gate (bd-0cet): unseen candidates join the table's memory
/// and the verdict follows the shared threshold. Deterministic in the
/// candidate list — same ids, same verdict, every replay.
pub fn flood_decide(mut table: WakeModel, candidate_ids: &[String]) -> (WakeModel, FloodGate) {
    let mut unseen = Vec::new();
    for id in candidate_ids {
        if table.flood_seen.insert(id.clone()) {
            unseen.push(id.clone());
        }
    }
    if unseen.len() > FLOOD_NEW_THRESHOLD {
        (table, FloodGate::Pause { unseen })
    } else {
        (table, FloodGate::Proceed)
    }
}

/// The one `why` projection (Spec 5): the verdict plus its evidence in
/// a single line — the loop's CRITICAL log and any operator `why` read
/// the same string.
pub fn why_flood(decision: &FloodGate, total: usize, unseen: usize) -> String {
    match decision {
        FloodGate::Proceed => {
            format!("flood gate proceed: {unseen}/{total} unseen (threshold {FLOOD_NEW_THRESHOLD})")
        }
        FloodGate::Pause { .. } => {
            format!(
                "flood gate PAUSE: {unseen}/{total} unseen over threshold {FLOOD_NEW_THRESHOLD} — runaway bead creation suspected (bd-eign.2); pausing dispatch this poll"
            )
        }
    }
}

/// Spec-5 routing verdict (bd-fuql): exactly one of these per wake.
/// Distinct from [`FloodGate`] (dispatch pausing) — this governs where
/// one wake GOES.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Prompt the pin now with this line.
    Inject { pin: String, line: String },
    /// Hold for the pin's digest batch with the reason recorded.
    Digest { pin: String, reason: String },
    /// Hold for a later poll (budget spent, transition pending).
    Queue { wake: String },
    /// Never deliver (superseded, expired, duplicate).
    Drop { reason: String },
}

/// What `route_wake` decides on (all inputs are already-resolved facts:
/// presence word, route existence, budget state — never lookups).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteInput {
    pub pin: String,
    pub line: String,
    pub urgent: bool,
    pub presence: PresenceVerdict,
    pub has_route: bool,
    pub budget_allows: bool,
}

/// ONE pure routing decide (bd-fuql): composes the existing gates in
/// Spec order — route existence (bd-bk9t), budget (Queue on spent),
/// then presence × urgency (`inject_gate`, bd-zawk). Deterministic in
/// its inputs; the caller owns persistence and delivery.
pub fn route_wake(input: &RouteInput) -> GateDecision {
    if !input.has_route {
        return GateDecision::Digest {
            pin: input.pin.clone(),
            reason: "no route: unroutable pin digests (bd-bk9t)".to_string(),
        };
    }
    if !input.budget_allows {
        return GateDecision::Queue {
            wake: input.line.clone(),
        };
    }
    match inject_gate(input.presence, input.urgent) {
        InjectGate::Inject => GateDecision::Inject {
            pin: input.pin.clone(),
            line: input.line.clone(),
        },
        InjectGate::Digest(reason) => GateDecision::Digest {
            pin: input.pin.clone(),
            reason: reason.to_string(),
        },
    }
}

/// ADR-0010 D2 (bd-8jlv): one presented wake — the line as prompted,
/// bound to the tick generation that presented it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PresentedWake {
    pub seq: u64,
    pub line: String,
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

/// One tracked delivery: a toron corr awaiting its crew ack.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

pub fn rering_due_with_policy(
    entry: &ReringEntry,
    now: u64,
    verdict: &str,
    grace_secs: u64,
    ring_max: u32,
) -> ReringDecision {
    if entry.acked || entry.fire_and_forget {
        return ReringDecision::Skip;
    }
    if entry.raw_without_id {
        if entry.attempts == 0 {
            return ReringDecision::Ring { attempt: 0 };
        }
        return ReringDecision::Skip;
    }
    let grace = grace_secs;
    let max = ring_max;
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
/// The routing `why`: one line per decision, same string for logs and
/// operator reads.
pub fn why_route(decision: &GateDecision) -> String {
    match decision {
        GateDecision::Inject { pin, line } => format!("inject {pin}: {line}"),
        GateDecision::Digest { pin, reason } => format!("digest {pin}: {reason}"),
        GateDecision::Queue { wake } => format!("queue (budget spent): {wake}"),
        GateDecision::Drop { reason } => format!("drop: {reason}"),
    }
}

/// Named pure predicates over routing inputs. Composite variants compose
/// with and/or/not — tick arms name guards, never inline conditions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guard {
    /// Unseen count over the flood threshold.
    UnseenOverThreshold {
        unseen: usize,
    },
    /// The wake has a delivery route.
    HasRoute,
    /// The pin's prompt budget allows one more wake.
    BudgetAllows,
    And(Box<Guard>, Box<Guard>),
    Or(Box<Guard>, Box<Guard>),
    Not(Box<Guard>),
}

impl Guard {
    pub fn and(a: Guard, b: Guard) -> Guard {
        Guard::And(Box::new(a), Box::new(b))
    }

    pub fn or(a: Guard, b: Guard) -> Guard {
        Guard::Or(Box::new(a), Box::new(b))
    }

    #[allow(clippy::should_implement_trait)]
    pub fn not(a: Guard) -> Guard {
        Guard::Not(Box::new(a))
    }

    /// Evaluate against a routing input plus unseen count. Pure.
    pub fn eval(&self, input: &RouteInput, unseen: usize) -> bool {
        match self {
            Guard::UnseenOverThreshold { .. } => unseen > FLOOD_NEW_THRESHOLD,
            Guard::HasRoute => input.has_route,
            Guard::BudgetAllows => input.budget_allows,
            Guard::And(a, b) => a.eval(input, unseen) && b.eval(input, unseen),
            Guard::Or(a, b) => a.eval(input, unseen) || b.eval(input, unseen),
            Guard::Not(a) => !a.eval(input, unseen),
        }
    }

    /// Canonical name for config/docs (leaf name or operator expression).
    pub fn name(&self) -> String {
        match self {
            Guard::UnseenOverThreshold { .. } => "unseen_over_threshold".to_string(),
            Guard::HasRoute => "has_route".to_string(),
            Guard::BudgetAllows => "budget_allows".to_string(),
            Guard::And(a, b) => format!("({} and {})", a.name(), b.name()),
            Guard::Or(a, b) => format!("({} or {})", a.name(), b.name()),
            Guard::Not(a) => format!("(not {})", a.name()),
        }
    }
}

/// All state changes enter as messages (clock arrives as payload, never read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeMsg {
    /// Flood-guard candidates for this poll.
    FloodCheck { ids: Vec<String> },
    /// Route one wake (already-resolved facts only).
    Route { input: RouteInput },
    /// Mark one wake presented.
    Present { id: String },
    /// Ack one presented wake (terminal).
    Ack { id: String },
    /// Digest one wake (terminal).
    Digest { id: String },
    /// Expire one wake (terminal; re-ring re-queues explicitly).
    Expire { id: String },
    /// Note one AckSLA ping (no re-ping on resume).
    NoteSla { corr: String },
}

/// Intents the core journals; the host executes. No I/O in [`tick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeEffect {
    /// Flood verdict for this poll's candidates.
    FloodVerdict { gate: FloodGate },
    /// Routing verdict for one wake.
    Routed { decision: GateDecision },
    /// Row transition applied (terminal ones leave the table).
    RowSettled { id: String, status: WakeStatus },
    /// AckSLA ping recorded.
    SlaNoted { corr: String },
}

/// Pure transition: one model in, new model + journaled effects out.
/// Routing arms delegate to the single behavior owners (`flood_decide`,
/// `route_wake`) — the tick folds, never re-decides.
pub fn tick(model: &WakeModel, msg: &WakeMsg) -> (WakeModel, Vec<WakeEffect>) {
    let mut next = model.clone();
    let effects = match msg {
        WakeMsg::FloodCheck { ids } => {
            let (t, gate) = flood_decide(next, ids);
            next = t;
            vec![WakeEffect::FloodVerdict { gate }]
        }
        // Guard for this arm: Guard::and(Guard::HasRoute, Guard::BudgetAllows)
        // (recorded in machine_config); the behavior owner is route_wake.
        WakeMsg::Route { input } => {
            vec![WakeEffect::Routed {
                decision: route_wake(input),
            }]
        }
        WakeMsg::Present { id } => {
            next.present(id);
            vec![WakeEffect::RowSettled {
                id: id.clone(),
                status: WakeStatus::Presented,
            }]
        }
        WakeMsg::Ack { id } => {
            next.ack(id);
            vec![WakeEffect::RowSettled {
                id: id.clone(),
                status: WakeStatus::Acked,
            }]
        }
        WakeMsg::Digest { id } => {
            next.digest(id);
            vec![WakeEffect::RowSettled {
                id: id.clone(),
                status: WakeStatus::Digested,
            }]
        }
        WakeMsg::Expire { id } => {
            next.expire(id);
            vec![WakeEffect::RowSettled {
                id: id.clone(),
                status: WakeStatus::Expired,
            }]
        }
        WakeMsg::NoteSla { corr } => {
            next.sla_seen.insert(corr.clone());
            vec![WakeEffect::SlaNoted { corr: corr.clone() }]
        }
    };
    (next, effects)
}

/// Serializable machine config (states, transitions, guard names) — the
/// visualizer input. Debug-printable at minimum; JSON because it is cheap.
pub fn machine_config() -> serde_json::Value {
    serde_json::json!({
        "machine": "wake_core",
        "states": ["model"],
        "transitions": [
            {"on": "FloodCheck", "guard": "unseen_over_threshold", "effects": ["FloodVerdict"]},
            {"on": "Route", "guard": "(has_route and budget_allows)", "effects": ["Routed"]},
            {"on": "Present", "guard": null, "effects": ["RowSettled"]},
            {"on": "Ack", "guard": null, "effects": ["RowSettled"]},
            {"on": "Digest", "guard": null, "effects": ["RowSettled"]},
            {"on": "Expire", "guard": null, "effects": ["RowSettled"]},
            {"on": "NoteSla", "guard": null, "effects": ["SlaNoted"]},
        ],
        "guards": ["unseen_over_threshold", "has_route", "budget_allows"],
    })
}
#[cfg(test)]
mod wake_core_flood_tests {
    use super::{FloodGate, WakeModel, flood_decide, why_flood};

    #[test]
    fn red_table_starts_empty_and_roundtrips() {
        let t = WakeModel::default();
        assert!(t.is_empty());
        let v = serde_json::to_value(&t).expect("serializes");
        let back: WakeModel = serde_json::from_value(v).expect("parses");
        assert_eq!(t, back);
    }

    #[test]
    fn red_seven_new_ids_pause_then_proceed() {
        let ids: Vec<String> = (0..7).map(|i| format!("bd-n{i}")).collect();
        let (t, d) = flood_decide(WakeModel::default(), &ids);
        assert!(matches!(d, FloodGate::Pause { .. }), "7 new trips");
        assert_eq!(t.flood_seen_len(), 7);
        let (t2, d2) = flood_decide(t, &ids);
        assert!(
            matches!(d2, FloodGate::Proceed),
            "known batch dispatches next poll"
        );
        assert_eq!(t2.flood_seen_len(), 7);
    }

    #[test]
    fn red_runaway_fresh_ids_keep_pausing() {
        let fresh: Vec<String> = (0..7).map(|i| format!("bd-r{i}")).collect();
        let (_, d) = flood_decide(WakeModel::default(), &fresh);
        assert!(matches!(d, FloodGate::Pause { .. }));
        let one = ["bd-only".to_string()];
        let (_, d2) = flood_decide(WakeModel::default(), &one);
        assert!(matches!(d2, FloodGate::Proceed), "1 new proceeds");
    }

    #[test]
    fn red_rows_transition_and_why_names_counts() {
        let mut t = WakeModel::default();
        t.present("bd-x");
        t.ack("bd-x");
        assert!(t.is_empty(), "acked rows leave the table");
        t.present("bd-y");
        t.digest("bd-y");
        assert!(t.is_empty(), "digested rows leave the table");
        let w = why_flood(
            &FloodGate::Pause {
                unseen: vec!["a".to_string()],
            },
            7,
            1,
        );
        assert!(w.contains('7') && w.contains('1'), "why names counts: {w}");
    }
}

#[cfg(test)]
mod route_tests_bd_fuql {
    use super::{GateDecision, PresenceVerdict, RouteInput, route_wake, why_route};

    fn input(urgent: bool, presence: PresenceVerdict, route: bool, budget: bool) -> RouteInput {
        RouteInput {
            pin: "TestPin".to_string(),
            line: "wake line".to_string(),
            urgent,
            presence,
            has_route: route,
            budget_allows: budget,
        }
    }

    #[test]
    fn red_no_route_digests() {
        let d = route_wake(&input(true, PresenceVerdict::Idle, false, true));
        assert!(
            matches!(d, GateDecision::Digest { .. }),
            "unroutable wake digests, never prompts: {d:?}"
        );
    }

    #[test]
    fn red_budget_spent_queues() {
        let d = route_wake(&input(true, PresenceVerdict::Idle, true, false));
        assert!(
            matches!(d, GateDecision::Queue { .. }),
            "spent budget holds the wake: {d:?}"
        );
    }

    #[test]
    fn red_idle_injects_and_dead_digests() {
        let d = route_wake(&input(false, PresenceVerdict::Idle, true, true));
        assert!(matches!(d, GateDecision::Inject { .. }));
        let d2 = route_wake(&input(true, PresenceVerdict::Dead, true, true));
        assert!(matches!(d2, GateDecision::Digest { .. }));
    }

    #[test]
    fn red_why_names_decision() {
        let w = why_route(&GateDecision::Drop {
            reason: "superseded".to_string(),
        });
        assert!(w.contains("drop") && w.contains("superseded"), "{w}");
    }
}

#[cfg(test)]
mod wake_core_tests_bd_0nem {
    use super::*;

    /// Standalone-build fence: this file's IMPORTS must never touch the
    /// runtime (tokio, Nostr clients, sibling backends). Scoped to `use`
    /// lines so doc prose may name what is forbidden.
    #[test]
    fn core_source_has_no_runtime_imports() {
        let src = include_str!("wake_core.rs");
        let shipped = src
            .split("#[cfg(test)]")
            .next()
            .expect("test module marker");
        let imports: Vec<&str> = shipped
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("use "))
            .collect();
        assert!(!imports.is_empty(), "fence must see the use lines");
        for banned in [
            "tokio",
            "nostr",
            "flywheel",
            "doorbell",
            "herdr",
            "sqlx",
            "axum",
            "rmcp",
            "parking_lot",
            "crate::",
            "super::",
        ] {
            for line in &imports {
                assert!(
                    !line.contains(banned),
                    "wake_core.rs imports banned `{banned}`: {line}"
                );
            }
        }
    }

    fn scripted_log() -> Vec<WakeMsg> {
        use WakeMsg::*;
        vec![
            FloodCheck {
                ids: (0..7).map(|i| format!("bd-n{i}")).collect(),
            },
            Route {
                input: RouteInput {
                    pin: "P".into(),
                    line: "w".into(),
                    urgent: false,
                    presence: PresenceVerdict::Idle,
                    has_route: true,
                    budget_allows: true,
                },
            },
            Present { id: "bd-n0".into() },
            Ack { id: "bd-n0".into() },
            NoteSla { corr: "c1".into() },
        ]
    }

    /// Same Msg log twice → byte-identical models + effects.
    #[test]
    fn same_msg_log_byte_identical_effects() {
        fn run() -> (WakeModel, Vec<WakeEffect>) {
            let mut model = WakeModel::default();
            let mut out = Vec::new();
            for msg in scripted_log() {
                let (next, eff) = tick(&model, &msg);
                model = next;
                out.extend(eff);
            }
            (model, out)
        }
        let (m1, e1) = run();
        let (m2, e2) = run();
        assert_eq!(m1, m2, "models identical");
        assert_eq!(e1, e2, "effects identical");
        // Spot-check the scripted outcomes: pause, inject, settle, note.
        assert!(matches!(e1[0], WakeEffect::FloodVerdict { .. }));
        assert!(matches!(
            e1[1],
            WakeEffect::Routed {
                decision: GateDecision::Inject { .. }
            }
        ));
    }

    /// Model path: fresh flood-sized batch pauses once, then proceeds;
    /// a runaway minting fresh ids pauses again.
    #[test]
    fn flood_pause_then_proceed_model_path() {
        let ids: Vec<String> = (0..7).map(|i| format!("bd-p{i}")).collect();
        let (m1, e1) = tick(
            &WakeModel::default(),
            &WakeMsg::FloodCheck { ids: ids.clone() },
        );
        assert!(
            matches!(
                e1.as_slice(),
                [WakeEffect::FloodVerdict {
                    gate: FloodGate::Pause { .. }
                }]
            ),
            "fresh batch pauses"
        );
        let (m2, e2) = tick(&m1, &WakeMsg::FloodCheck { ids });
        assert!(
            matches!(
                e2.as_slice(),
                [WakeEffect::FloodVerdict {
                    gate: FloodGate::Proceed
                }]
            ),
            "known batch proceeds"
        );
        let fresh: Vec<String> = (0..7).map(|i| format!("bd-r{i}")).collect();
        let (_, e3) = tick(&m2, &WakeMsg::FloodCheck { ids: fresh });
        assert!(
            matches!(
                e3.as_slice(),
                [WakeEffect::FloodVerdict {
                    gate: FloodGate::Pause { .. }
                }]
            ),
            "fresh wave pauses again"
        );
        let cfg = machine_config();
        assert_eq!(cfg["machine"], "wake_core");
    }
}

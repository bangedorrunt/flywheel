// governed-by: ADR-0012
//! Doorbell pure core (ADR-0012 Phase 0): ring rules, queue coalesce, inject
//! gate, watch/inbox parsing, and the Elm tick. Moved verbatim from
//! `src/doorbell.rs` (pure fns, no signature changes) and `src/doorbell_tick.rs`
//! (verbatim). No I/O, no `&mut` across the API, no tokio/toron/herdr/resonate.
//! The effect layer (`cmd_internal_doorbell`, pid files, supervision) stays in
//! flywheel. Tests assert `same Msg log -> byte-identical Action sequence`.

use std::collections::{BTreeMap, BTreeSet};

/// Fenced wake kernel (bd-0nem): Model/Msg/Effect/tick live here.
pub mod wake_core;
/// Compat re-exports: decision vocabulary moved into `wake_core` (single
/// home); these paths keep existing users resolving while call sites
/// migrate to `wake_core::`.
pub use wake_core::{
    GateDecision, InjectGate, PresenceVerdict, RouteInput, inject_gate, inject_gate_for_herdr,
    parse_presence_word,
};

/// One class of doorbell. Queued last-write-wins per class (ADR-0005 D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventClass {
    Channel,
    Job,
    RequestRelease,
    ReservationConflict,
    PlanePUrgent,
    /// Sealed-mail delivery notice (bd-v8vi): urgency unknowable on the
    /// watch path, so it rings a routine inbox-pull. True urgency comes
    /// from the inbox fold (`parse_inbox_item` reads importance).
    PlanePMail,
}

/// A watch/inbox edge the dispatcher might ring on.
#[derive(Debug, Clone)]
pub struct RingInput {
    pub kind: u16,
    /// Kind-9 `p` / job assignee / request-release target is this pin.
    pub addressed_to_self: bool,
    pub toron_kind_request_release: bool,
    pub toron_kind_mail_delivered: bool,
    /// Kind-9 `ack-sla-expired`: the ack never came — urgent by nature,
    /// unlike a routine delivery (bd-v8vi split).
    pub toron_kind_ack_sla: bool,
    pub importance_high_or_urgent: bool,
    pub reservation_conflicts_self: bool,
    /// Room chatter: kind 9 with no self address.
    pub is_presence: bool,
    pub is_ack_only: bool,
}

/// Classify a bus/inbox edge. `None` = do not ring.
pub fn classify(ev: &RingInput) -> Option<EventClass> {
    if ev.is_presence || ev.is_ack_only {
        return None;
    }
    if ev.toron_kind_request_release && ev.addressed_to_self {
        return Some(EventClass::RequestRelease);
    }
    if ev.reservation_conflicts_self && ev.kind == 30031 {
        return Some(EventClass::ReservationConflict);
    }
    if ev.addressed_to_self && ev.kind == 43001 {
        return Some(EventClass::Job);
    }
    // GS-006 bug 1: kind 9 mail-delivered (NIP-17 gift-wrap wake) must pull mail, not channel.
    // bd-v8vi: delivery is routine (urgency unknowable sealed); only the
    // ack-sla-expiry ring stays urgent. Phantom urgents otherwise.
    if ev.addressed_to_self && ev.kind == 9 && ev.toron_kind_ack_sla {
        return Some(EventClass::PlanePUrgent);
    }
    // Tagged `toron-kind=mail-delivered` on kind 9; routine inbox pull.
    if ev.addressed_to_self && ev.kind == 9 && ev.toron_kind_mail_delivered {
        return Some(EventClass::PlanePMail);
    }
    if ev.addressed_to_self && ev.kind == 9 {
        return Some(EventClass::Channel);
    }
    // GS-006 bug 2: kind 39002 join/leave addressed to self must ring.
    // Reuse Channel semantics (membership wake) — hint falls back to channel read.
    if ev.addressed_to_self && ev.kind == 39002 {
        return Some(EventClass::Channel);
    }
    if ev.importance_high_or_urgent && ev.addressed_to_self {
        return Some(EventClass::PlanePUrgent);
    }
    None
}

/// State-level injectability (queue drain transitions only). Kept
/// urgency-agnostic on purpose: drain is about leaving working for an
/// await-signal state, not about one line's urgency. Per-wake decisions
/// MUST use `inject_gate` / `inject_gate_for_herdr` instead.
// governed-by: bd-zawk
pub fn should_inject(herdr_status: &str) -> bool {
    matches!(herdr_status, "idle" | "done" | "blocked")
}

/// Drain when leaving a non-injectable state for an injectable one.
/// Working is the only non-injectable state; idle|done|blocked are injectable.
pub fn is_drain_transition(from: &str, to: &str) -> bool {
    !should_inject(from) && should_inject(to)
}

/// Last-write-wins queue, one line per (EventClass, hint/channel).
/// Gap 6 fix: per-channel coalesce — two channels while working no longer collide.
#[derive(Debug, Default)]
pub struct DoorbellQueue {
    pending: BTreeMap<(EventClass, String), String>,
}

impl DoorbellQueue {
    pub fn push(&mut self, class: EventClass, line: impl Into<String>) {
        let line = line.into();
        // hint is the Pull part; use full line as key fallback when hint unknown.
        // For correct per-channel dedup, callers pass the hint-derived line.
        // Extract the hint suffix for keying when possible.
        let hint = line.split("Pull: ").nth(1).unwrap_or(&line).to_string();
        self.pending.insert((class, hint), line);
    }

    /// Direct insertion with explicit channel/hint key (preserves per-channel).
    pub fn push_with_hint(&mut self, class: EventClass, hint: String, line: String) {
        self.pending.insert((class, hint), line);
    }

    /// Queue depth (asserted by the queue coalesce tests).
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Drain in class+hint order. Empty after.
    pub fn drain(&mut self) -> Vec<(EventClass, String)> {
        if self.is_empty() {
            return Vec::new();
        }
        std::mem::take(&mut self.pending)
            .into_iter()
            .map(|((class, _), line)| (class, line))
            .collect()
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SelfIndex {
    pins: BTreeMap<String, String>,
    npubs: BTreeMap<String, String>,
}

impl SelfIndex {
    pub fn from_pins<I, S>(pins: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut idx = Self::default();
        for p in pins {
            idx.insert_pin(p.into(), None);
        }
        idx
    }

    pub fn insert_pin(&mut self, pin: String, npub: Option<String>) {
        if let Some(n) = npub.filter(|s| !s.is_empty()) {
            let n = n.to_lowercase();
            self.npubs.insert(n.clone(), pin.clone());
            self.pins.insert(pin, n);
        } else {
            self.pins.entry(pin).or_default();
        }
    }

    pub fn resolve(&self, token: &str) -> Option<String> {
        if self.pins.contains_key(token) {
            return Some(token.to_string());
        }
        self.npubs.get(&token.to_lowercase()).cloned()
    }

    pub fn pin_names(&self) -> impl Iterator<Item = &str> {
        self.pins.keys().map(String::as_str)
    }

    pub fn npub(&self, pin: &str) -> Option<&str> {
        let n = self.pins.get(pin)?;
        if n.is_empty() { None } else { Some(n.as_str()) }
    }
}

/// Seconds after inject with no agent-originated event before the pin is
/// unanswered (presence / kind-9 / reaction from their npub).
pub const UNANSWERED_SECS: u64 = 120;

/// bd-durable-doorbell-lngf: when NO wake-state high-water exists (first
/// run, or state lost), the inbox sweep looks this far back instead of
/// `now` — mail that arrived while the dispatcher was dead still wakes.
/// Bounded so a never-seen inbox does not replay forever; the `[doorbell]`
/// line + per-pin budget keep the replay idempotent and cheap.
pub const REDELIVERY_LOOKBACK_HOURS: u64 = 24;

/// Bounded backfill floor for cursor-less pins (bd-5d0v): sweep `since` is
/// wall-clock now, so fetching strictly-newer-than-now for a pin with no
/// `since_by_pin` entry returns empty forever — and the cursor never
/// initializes because the high-water only advances on non-empty batches.
/// The floor bounds the backfill instead; steady state resumes from the
/// row high-water after the first non-empty batch.
pub fn redelivery_floor_rfc3339(now_unix: u64) -> String {
    let floor = now_unix.saturating_sub(REDELIVERY_LOOKBACK_HOURS * 3600);
    chrono::DateTime::from_timestamp(floor as i64, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
}

/// fnv1a64 (bd-z5ou): single owner of the registry-key hash. Formerly
/// duplicated byte-identically in flywheel `state` and doorbell-rt
/// `file_host` behind "mirrors" comments — the second copy is how key
/// skew goes silent.
pub fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Registry key (bd-z5ou): bare slugs pass through, slash paths hash to
/// `{base}-{8hex}`. Single owner; both the flywheel registry and the
/// daemon resolver read through this.
pub fn registry_key(project: &str) -> String {
    if project.contains('/') {
        let base = project.rsplit('/').next().unwrap_or(project);
        let h = fnv1a64(project);
        format!("{base}-{:08x}", (h & 0xffff_ffff) as u32)
    } else {
        project.to_string()
    }
}

/// Captain-fact promotion, split in two so both hosts share the rule
/// (bd-z5ou): `fact_captain_pin` owns the role precedence (captain, else
/// wheelhouse); `fact_prompt_line` owns the line shape. The Idle check
/// stays host-side (workspace reads its Presence map, the daemon its
/// sticky verdicts) — only the precedence + format unify.
pub fn fact_captain_pin(roles: &BTreeMap<String, String>) -> Option<String> {
    roles
        .get("captain")
        .or_else(|| roles.get("wheelhouse"))
        .cloned()
}

pub fn fact_prompt_line(summary: &str) -> String {
    format!("[doorbell] {summary}")
}

pub fn watch_pubkey(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("pubkey")
        .and_then(|p| p.as_str())
        .map(str::to_lowercase)
}

/// Event id on a watch NDJSON line (gap 17 trust: hint carries proof).
pub fn watch_id(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("id")
        .and_then(|p| p.as_str())
        .map(std::string::ToString::to_string)
}

/// `created_at` unix secs on a watch NDJSON line (gap 1 fix: plane-S cursor).
pub fn watch_created_at(line: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("created_at").and_then(serde_json::Value::as_u64)
}

/// Persisted per-workspace wake state (bd-durable-doorbell-lngf). File I/O
/// stays in flywheel; this struct + `note_wake` are the pure core.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct WakeState {
    /// plane-P inbox high-water per pin (RFC3339) — survives restarts.
    #[serde(default)]
    pub since_by_pin: BTreeMap<String, String>,
    /// plane-S high-water (unix micros of newest seen watch event) — gap 1 fix.
    #[serde(default)]
    pub plane_s_cursor_us: Option<i64>,
    /// last successful wake injection (unix secs) — status liveness.
    #[serde(default)]
    pub last_wake_unix: Option<u64>,
    #[serde(default)]
    pub last_wake_pin: Option<String>,
}

/// Record one delivered wake (the stamp `status` liveness reads).
/// bd-7qg6: one writer, one ratchet — refuse a same-second or behind stamp
/// so rapid double prompts cannot fake liveness for `send --wait`.
/// Returns true when the stamp advanced.
pub fn note_wake(st: &mut WakeState, pin: &str, now_unix: u64) -> bool {
    if let Some(prev) = st.last_wake_unix
        && now_unix <= prev
    {
        return false;
    }
    st.last_wake_unix = Some(now_unix);
    st.last_wake_pin = Some(pin.to_string());
    true
}

pub fn watch_targets(line: &str, index: &SelfIndex) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let mut out = BTreeSet::new();
    if let Some(tags) = v.get("tags").and_then(|t| t.as_array()) {
        for t in tags {
            let Some(a) = t.as_array() else { continue };
            let Some(key) = a.first().and_then(|x| x.as_str()) else {
                continue;
            };
            let Some(val) = a.get(1).and_then(|x| x.as_str()) else {
                continue;
            };
            if matches!(key, "p" | "assignee")
                && let Some(pin) = index.resolve(val)
            {
                out.insert(pin);
            }
        }
    }
    if v.get("kind").and_then(serde_json::Value::as_u64) == Some(9)
        && let Some(content) = v.get("content").and_then(|c| c.as_str())
    {
        for pin in index.pin_names() {
            if content.contains(&format!("@{pin}")) {
                out.insert(pin.to_string());
            }
        }
    }
    out.into_iter().collect()
}

/// Parse one `toron watch` NDJSON line (`{kind,id,tags,content}`).
pub fn parse_watch_ndjson(line: &str, index: &SelfIndex) -> Option<(EventClass, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = v.get("kind")?.as_u64()? as u16;
    let tags = v.get("tags")?.as_array()?;
    let addressed_to_self = !watch_targets(line, index).is_empty();
    let toron_kind_request_release = tags.iter().any(|t| {
        t.as_array()
            .map(|a| {
                a.first().and_then(|x| x.as_str()) == Some("toron-kind")
                    && a.get(1).and_then(|x| x.as_str()) == Some("request-release")
            })
            .unwrap_or(false)
    });
    // mail-delivered AND ack-sla-expired are mailbox wakes (kind 9 metadata
    // rings for sealed NIP-17 mail) — both must pull the inbox, never
    // `channel read` on a thread id.
    let toron_kind_is = |want: &str| {
        tags.iter().any(|t| {
            t.as_array()
                .map(|a| {
                    a.first().and_then(|x| x.as_str()) == Some("toron-kind")
                        && a.get(1).and_then(|x| x.as_str()) == Some(want)
                })
                .unwrap_or(false)
        })
    };
    // bd-v8vi: delivery notice vs expiry are different urgencies — a
    // sealed delivery is routine until the inbox fold says otherwise.
    let toron_kind_mail_delivered = toron_kind_is("mail-delivered");
    let toron_kind_ack_sla = toron_kind_is("ack-sla-expired");
    let class = classify(&RingInput {
        kind,
        addressed_to_self,
        toron_kind_request_release,
        toron_kind_mail_delivered,
        toron_kind_ack_sla,
        importance_high_or_urgent: false,
        reservation_conflicts_self: kind == 30031 && addressed_to_self,
        is_presence: kind == 20001,
        is_ack_only: false,
    })?;
    let h = tags.iter().find_map(|t| {
        let a = t.as_array()?;
        if a.first()?.as_str()? == "h" {
            a.get(1)?.as_str()
        } else {
            None
        }
    });
    let hint = pull_hint(class, h);
    Some((class, hint))
}

fn pull_hint(class: EventClass, channel: Option<&str>) -> String {
    match class {
        EventClass::Channel => match channel {
            Some(h) => format!("toron channel read h={h}"),
            None => "toron channel read".into(),
        },
        EventClass::Job => "toron job list".into(),
        EventClass::RequestRelease | EventClass::PlanePUrgent | EventClass::PlanePMail => {
            "toron mail inbox --since".into()
        }
        EventClass::ReservationConflict => "toron reserve list".into(),
    }
}

/// Classify one `fetch_inbox` row (plane P). Inbox is already pin-scoped.
pub fn parse_inbox_item(v: &serde_json::Value) -> Option<(EventClass, String)> {
    if v.get("ack_only")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let tags = v.get("tags").and_then(|t| t.as_array());
    let request_release = tags
        .map(|tags| {
            tags.iter().any(|t| {
                t.as_array()
                    .map(|a| {
                        a.first().and_then(|x| x.as_str()) == Some("toron-kind")
                            && a.get(1).and_then(|x| x.as_str()) == Some("request-release")
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let importance = v.get("importance").and_then(|x| x.as_str()).unwrap_or("");
    let class = if request_release {
        EventClass::RequestRelease
    } else if matches!(importance, "high" | "urgent") {
        EventClass::PlanePUrgent
    } else {
        return None;
    };
    Some((class, pull_hint(class, None)))
}

/// Rumor-time of one inbox JSON row: live ToJson uses `created_ts_us` (i64
/// micros); CLI/legacy blobs may still carry `created_ts` RFC3339.
pub fn row_created_us(v: &serde_json::Value) -> Option<i64> {
    if let Some(n) = v.get("created_ts_us") {
        if let Some(i) = n.as_i64() {
            return Some(i);
        }
        if let Some(u) = n.as_u64() {
            return Some(u as i64);
        }
        if let Some(s) = n.as_str() {
            return s.parse().ok();
        }
    }
    v.get("created_ts")
        .and_then(|t| t.as_str())
        .and_then(|s| parse_rfc3339_micros(s).ok())
}

fn us_to_rfc3339(us: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_micros(us)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
}

fn inbox_items(v: &serde_json::Value) -> Vec<&serde_json::Value> {
    if let Some(arr) = v.as_array() {
        arr.iter().collect()
    } else if let Some(arr) = v.get("messages").and_then(|m| m.as_array()) {
        arr.iter().collect()
    } else if v.is_object() {
        vec![v]
    } else {
        Vec::new()
    }
}

pub fn inbox_high_water_from_values(items: Vec<&serde_json::Value>) -> Option<String> {
    let mut best: Option<(i64, String)> = None;
    for i in items {
        let Some(us) = row_created_us(i) else {
            continue;
        };
        let label = i
            .get("created_ts")
            .and_then(|t| t.as_str())
            .map(str::to_string)
            .or_else(|| us_to_rfc3339(us));
        let Some(label) = label else {
            continue;
        };
        if best.as_ref().map(|(u, _)| us > *u).unwrap_or(true) {
            best = Some((us, label));
        }
    }
    best.map(|(_, s)| s)
}

/// Newest created time in an inbox blob as RFC3339, for the high-water.
/// Compares numerically (micros). Live JSON is `created_ts_us`.
pub fn inbox_high_water(raw: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    inbox_high_water_from_values(inbox_items(&v))
}

/// Newest created time as unix-micros (None if unparseable).
pub fn inbox_high_water_micros(raw: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    inbox_items(&v).into_iter().filter_map(row_created_us).max()
}

pub fn parse_rfc3339_micros(ts: &str) -> Result<i64, chrono::ParseError> {
    chrono::DateTime::parse_from_rfc3339(ts).map(|dt| dt.timestamp_micros())
}

/// One `[doorbell]` line. Body stays pull.
pub fn doorbell_line(class: EventClass, hint: &str) -> String {
    let kind = match class {
        EventClass::Channel => "kind 9",
        EventClass::Job => "kind 43001",
        EventClass::RequestRelease => "request-release",
        EventClass::ReservationConflict => "kind 30031",
        EventClass::PlanePUrgent => "plane-P urgent",
        EventClass::PlanePMail => "plane-P mail",
    };
    format!("[doorbell] {kind}. Pull: {hint}")
}

/// Trust-aware line that carries the triggering event id for proof (gap 17).
pub fn doorbell_line_with_event(class: EventClass, hint: &str, event_id: &str) -> String {
    if event_id.is_empty() {
        doorbell_line(class, hint)
    } else {
        let kind = match class {
            EventClass::Channel => "kind 9",
            EventClass::Job => "kind 43001",
            EventClass::RequestRelease => "request-release",
            EventClass::ReservationConflict => "kind 30031",
            EventClass::PlanePUrgent => "plane-P urgent",
            EventClass::PlanePMail => "plane-P mail",
        };
        format!("[doorbell] {kind} event={event_id}. Pull: {hint}")
    }
}

/// Parse `--status pin=idle` pairs for the herdr-mock (`__doorbell --once`).
pub fn parse_status_pair(s: &str) -> Option<(String, String)> {
    let (pin, st) = s.split_once('=')?;
    if pin.is_empty() || st.is_empty() {
        return None;
    }
    Some((pin.to_string(), st.to_string()))
}

// ---- Elm tick (moved verbatim from src/doorbell_tick.rs) ------------------
// Pure `tick(Model,Msg)->(Model,Actions)` — no I/O, no &mut, no herdr/toron.

/// One doorbell message. Every bus/inbox/status edge is a Msg; nothing
/// happens outside `tick`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DoorbellMsg {
    /// Plane-S watch NDJSON line (`{kind,id,tags,content}`).
    WatchNdjson { line: String },
    /// Plane-P inbox rows for one pin (raw `fetch_inbox` JSON).
    InboxItems { raw: String, pin: String },
    /// Plane-P sweep path: rows already parsed (no serde round-trip).
    InboxRows {
        rows: Vec<serde_json::Value>,
        pin: String,
    },
    /// Herdr status change for one pin.
    HerdrStatus { pin: String, status: String },
    /// Captain-bound fact (no alias info here — honest FactMiss).
    Fact { summary: String },
    /// Periodic tick (60s cadence in the live dispatcher); drives the
    /// Ack-SLA scan effect.
    AckSlaTick { now: u64 },
    /// The watch child died; supervisor must restart (never exit).
    WatchDied,
}

/// What the effect layer should do. Prompt is the only delivery; FetchInbox /
/// CheckAckSla / RestartWatch are effects whose results re-enter as Msgs;
/// FactMiss and Audit are loud misses (journal + stderr), never silent Ok(()).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DoorbellAction {
    Prompt {
        pin: String,
        line: String,
    },
    /// Fetch signal only (bd-z5ou): per-pin cursors live in the brain, so
    /// this carries no payload — rows re-enter as InboxRows/InboxFetched.
    FetchInbox,
    /// Effect: run `check_ack_sla_expired` for the project (toron mailbox).
    /// Overdue wakes re-enter as WatchNdjson Msgs via plane S.
    CheckAckSla,
    RestartWatch,
    FactMiss {
        why: String,
    },
    Audit {
        kind: String,
        pin: String,
        detail: String,
    },
    /// bd-bk9t: a wake for a pin the index knows cannot be routed to a pane
    /// (a subscribed identity with no spawn row). The effect layer puts it
    /// in the digest batch; `why` rides the journal row so the skip is never
    /// silent. Distinct from `Audit` because the wake still reaches the
    /// human — it is only the *prompt* route that does not exist.
    Digest {
        pin: String,
        line: String,
        why: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoorbellModel {
    /// LWW queues per pin (working panes hold their wake here).
    pub queues: BTreeMap<String, Vec<String>>,
    /// Last-known herdr status per pin.
    pub statuses: BTreeMap<String, String>,
    /// Pins (+npubs) this dispatcher owns.
    pub index: SelfIndex,
    /// Per-pin prompt budgets (D4.5, bd-asr2.9): durable model state,
    /// rebuilt exactly by journal refold (budget rows). The gate stays in
    /// the effect layer (take_allowed below) — threading it into tick
    /// would reshape decided rows and break the .6 parity gate.
    pub budgets: BTreeMap<String, PinBudget>,
    /// Pins the index knows but cannot route to: subscribed identities with
    /// no spawn row, so no pane and no herdr target (bd-bk9t). Their wakes
    /// must NOT park in `queues` — that map only drains on a
    /// `working -> injectable` [`is_drain_transition`], and a pane-less pin
    /// never produces one, so the entry would sit in RAM forever and die
    /// with the process. `route_or_queue` sends them to the digest instead.
    pub unroutable: BTreeSet<String>,
}

/// Per-pin 5/30/3 budget (D1 item 2, D4.5 home).
/// Rolling token bucket: 3 prompts per 30-min window, refilled continuously.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinBudget {
    pub budget_minutes: u32,
    pub max_prompts: u32,
    /// unix secs of each granted prompt (rolling window)
    pub prompts: Vec<u64>,
    pub started_unix: u64,
}

impl PinBudget {
    pub fn adr_default(started_unix: u64) -> Self {
        Self::new(30, 3, started_unix)
    }

    pub fn new(budget_minutes: u32, max_prompts: u32, started_unix: u64) -> Self {
        Self {
            budget_minutes,
            max_prompts,
            prompts: Vec::new(),
            started_unix,
        }
    }

    fn window_secs(&self) -> u64 {
        u64::from(self.budget_minutes) * 60
    }

    pub fn allows(&self, now_unix: u64) -> bool {
        let window = self.window_secs();
        let recent = self
            .prompts
            .iter()
            .filter(|t| now_unix.saturating_sub(**t) < window)
            .count();
        (recent as u32) < self.max_prompts
    }

    pub fn record_at(&mut self, now_unix: u64) {
        let window = self.window_secs();
        self.prompts
            .retain(|t| now_unix.saturating_sub(*t) < window);
        self.prompts.push(now_unix);
    }
}

/// Drop injects that exceed the pin's budget (rolling). Pure model
/// transition over the map (D4.5): no clock reads, `now` arrives as a
/// parameter — replayable, refoldable.
pub fn take_allowed(
    injects: Vec<(String, String)>,
    budgets: &mut BTreeMap<String, PinBudget>,
    now_unix: u64,
) -> Vec<(String, String)> {
    take_allowed_like(injects, budgets, now_unix, 30, 3)
}

/// Budget gate with a subscription override template (D5).
pub fn take_allowed_like(
    injects: Vec<(String, String)>,
    budgets: &mut BTreeMap<String, PinBudget>,
    now_unix: u64,
    window_minutes: u32,
    max_prompts: u32,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (pin, msg) in injects {
        let b = budgets
            .entry(pin.clone())
            .or_insert_with(|| PinBudget::new(window_minutes, max_prompts, now_unix));
        if b.allows(now_unix) {
            b.record_at(now_unix);
            out.push((pin, msg));
        }
    }
    out
}
/// A doorbell line that must ring now, never batch: plane-P urgent,
/// request-release, reservation conflicts, ack-SLA expiry (including the
/// re-ring vehicle `doorbell_mail_line`, which carries "unacked for").
/// Moved verbatim from flywheel `workspace.rs` (bd-ene2): one definition —
/// the daemon FileHost presence filter and the embedded path share it.
pub fn is_urgent_line(line: &str) -> bool {
    line.contains("plane-P urgent")
        || line.contains("request-release")
        || line.contains("kind 30031")
        || line.contains("ack-sla-expired")
        || line.contains("unacked for")
        // GS-048: mid-session reclassify vehicle announce must deliver (counts as wake).
        || line.starts_with("[reclassify]")
}

/// Expand an inbox JSON blob into row values: array, single object, or the
/// `{"messages":[...]}` wrapper `fetch_inbox` emits.
fn expand_inbox_rows(v: &serde_json::Value) -> Vec<&serde_json::Value> {
    match v {
        serde_json::Value::Array(arr) => arr.iter().collect(),
        other if other.get("messages").is_some() => other
            .get("messages")
            .and_then(|m| m.as_array())
            .map(|arr| arr.iter().collect())
            .unwrap_or_default(),
        other => vec![other],
    }
}

/// Pure, total, deterministic: no I/O, no &mut, no herdr/toron calls.
pub fn tick(model: &DoorbellModel, msg: &DoorbellMsg) -> (DoorbellModel, Vec<DoorbellAction>) {
    let mut next = model.clone();
    let mut actions = Vec::new();
    match msg {
        DoorbellMsg::WatchNdjson { line } => {
            let Some((class, hint)) = parse_watch_ndjson(line, &next.index) else {
                return (next, actions);
            };
            let line_out = doorbell_line(class, &hint);
            for pin in watch_targets(line, &next.index) {
                route_or_queue(&mut next, &mut actions, &pin, line_out.clone());
            }
        }
        DoorbellMsg::InboxItems { raw, pin } => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
                actions.push(DoorbellAction::Audit {
                    kind: "inbox_parse_failure".into(),
                    pin: pin.clone(),
                    detail: raw.clone(),
                });
                return (next, actions);
            };
            for item in expand_inbox_rows(&v) {
                let Some((class, hint)) = parse_inbox_item(item) else {
                    continue;
                };
                let line_out = doorbell_line(class, &hint);
                route_or_queue(&mut next, &mut actions, pin, line_out);
            }
        }
        DoorbellMsg::InboxRows { rows, pin } => {
            // Plane-P sweep path: rows already parsed (Values), no round-trip.
            for item in rows {
                let Some((class, hint)) = parse_inbox_item(item) else {
                    continue;
                };
                let line_out = doorbell_line(class, &hint);
                route_or_queue(&mut next, &mut actions, pin, line_out);
            }
        }
        DoorbellMsg::HerdrStatus { pin, status } => {
            let from = next
                .statuses
                .get(pin)
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            if is_drain_transition(&from, status)
                && let Some(q) = next.queues.get_mut(pin)
            {
                for line in std::mem::take(q) {
                    actions.push(DoorbellAction::Prompt {
                        pin: pin.clone(),
                        line,
                    });
                }
            }
            next.statuses.insert(pin.clone(), status.clone());
        }
        DoorbellMsg::Fact { summary } => {
            // The doorbell-side reducer has no captain alias — an alias-less
            // fact is a LOUD miss (never registry.first()).
            actions.push(DoorbellAction::FactMiss {
                why: format!("fact without captain alias: {}", summary),
            });
        }
        DoorbellMsg::AckSlaTick { .. } => {
            // The scan is check_ack_sla_expired (toron), not an inbox fetch.
            // Overdue wakes arrive back as kind-9 ack-sla-expired WatchNdjson.
            actions.push(DoorbellAction::CheckAckSla);
            // leftover #13: FetchInbox is now a real Cmd whose rows re-enter as InboxRows
            actions.push(DoorbellAction::FetchInbox);
        }
        DoorbellMsg::WatchDied => {
            // Never exit the dispatcher: restart is the only path (heal_tick is backup).
            actions.push(DoorbellAction::RestartWatch);
        }
    }
    (next, actions)
}

/// Idle/done panes get a Prompt now; blocked+urgent prompts while
/// blocked+routine digests with a reason (bd-zawk G6); working panes queue
/// (drained on the next working -> idle|done|blocked HerdrStatus).
fn route_or_queue(
    next: &mut DoorbellModel,
    actions: &mut Vec<DoorbellAction>,
    pin: &str,
    line: String,
) {
    let status = next
        .statuses
        .get(pin)
        .map(String::as_str)
        .unwrap_or("unknown");
    // governed-by: bd-zawk — per-wake verdict via the typed InjectGate so the
    // herdr-string path agrees with the presence-verdict path by construction.
    // blocked+urgent prompts; blocked+routine digests with a reason.
    if should_inject(status) {
        match inject_gate_for_herdr(status, is_urgent_line(&line)) {
            InjectGate::Inject => {
                actions.push(DoorbellAction::Prompt {
                    pin: pin.to_string(),
                    line,
                });
            }
            InjectGate::Digest(reason) => {
                actions.push(DoorbellAction::Digest {
                    pin: pin.to_string(),
                    line: format!("{line} [{reason}]"),
                    why: reason.to_string(),
                });
            }
        }
    } else if next.unroutable.contains(pin) {
        // bd-bk9t: no pane means no drain is possible, ever. Queueing here
        // would be a memory leak wearing a delivery intent — the RAM-only
        // queue is lost on restart and no `HerdrStatus` will ever drain it.
        // The wake is real and the human is the addressee, so it goes to
        // the digest with a named reason instead.
        actions.push(DoorbellAction::Digest {
            pin: pin.to_string(),
            line,
            why: "no_route".into(),
        });
    } else {
        // bd-ztek: a queued ring leaves one journal-visible Audit on the
        // empty→non-empty transition per pin. Without it the queue was
        // silent (no action row), so `why` showed nothing for dead-pane
        // pins and the wake looked lost. Bounded: further enqueues while
        // non-empty stay quiet; the drain still emits the Prompts.
        let fresh = next.queues.get(pin).is_none_or(std::vec::Vec::is_empty);
        next.queues
            .entry(pin.to_string())
            .or_default()
            .push(line.clone());
        if fresh {
            actions.push(DoorbellAction::Audit {
                kind: "queued".into(),
                pin: pin.to_string(),
                detail: line,
            });
        }
    }
}

impl DoorbellAction {
    /// Test/debug label (also keeps the enum honest in logs).
    pub fn to_label(&self) -> String {
        match self {
            Self::Prompt { pin, .. } => format!("prompt {pin}"),
            Self::FetchInbox => "fetch-inbox".into(),
            Self::CheckAckSla => "check-ack-sla".into(),
            Self::RestartWatch => "restart-watch".into(),
            Self::FactMiss { why } => format!("fact-miss {why}"),
            Self::Audit { kind, pin, .. } => format!("audit {kind} {pin}"),
            Self::Digest { pin, why, .. } => format!("digest {pin} ({why})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_with(pin: &str) -> DoorbellModel {
        let mut m = DoorbellModel::default();
        m.index.insert_pin(pin.to_string(), None);
        m.statuses.insert(pin.to_string(), "idle".into());
        m
    }

    #[test]
    fn presence_word_map_is_single_sourced() {
        // bd-z5ou: both hosts read through this — blocked is Busy, done
        // is Idle, dead words are Dead, garbage is Unknown (never idle).
        for w in ["idle", "done", " IDLE "] {
            assert_eq!(parse_presence_word(w), PresenceVerdict::Idle);
        }
        for w in ["working", "busy", "blocked"] {
            assert_eq!(parse_presence_word(w), PresenceVerdict::Busy);
        }
        for w in ["dead", "endpoint-dead", "endpoint"] {
            assert_eq!(parse_presence_word(w), PresenceVerdict::Dead);
        }
        for w in ["", "mystery", "null"] {
            assert_eq!(parse_presence_word(w), PresenceVerdict::Unknown);
        }
    }

    #[test]
    fn inject_gate_blocked_urgent_injects_routine_digests() {
        // bd-zawk G6: one typed gate for both paths — blocked+urgent injects,
        // blocked+routine digests with a reason, never a silent drop.
        assert_eq!(inject_gate_for_herdr("blocked", true), InjectGate::Inject);
        assert!(matches!(
            inject_gate_for_herdr("blocked", false),
            InjectGate::Digest(_)
        ));
        assert_eq!(inject_gate_for_herdr("idle", false), InjectGate::Inject);
        assert_eq!(inject_gate_for_herdr("done", false), InjectGate::Inject);
        assert!(matches!(
            inject_gate_for_herdr("working", false),
            InjectGate::Digest(_)
        ));
        assert!(matches!(
            inject_gate_for_herdr("unknown", true),
            InjectGate::Digest(_)
        ));
        // Verdict path agrees with the herdr-string path by construction.
        assert_eq!(inject_gate(PresenceVerdict::Busy, true), InjectGate::Inject);
        assert!(matches!(
            inject_gate(PresenceVerdict::Busy, false),
            InjectGate::Digest(_)
        ));
    }

    #[test]
    fn fact_promotion_prefers_captain_over_wheelhouse() {
        // bd-z5ou: precedence + line shape shared by both hosts.
        let mut roles = BTreeMap::new();
        assert_eq!(fact_captain_pin(&roles), None);
        roles.insert("wheelhouse".into(), "PinW".into());
        assert_eq!(fact_captain_pin(&roles).as_deref(), Some("PinW"));
        roles.insert("captain".into(), "PinC".into());
        assert_eq!(fact_captain_pin(&roles).as_deref(), Some("PinC"));
        assert_eq!(fact_prompt_line("deploy"), "[doorbell] deploy");
        assert!(
            is_urgent_line("[reclassify] vehicle: none (topology=open) — miss"),
            "GS-048 reclassify announce must deliver"
        );
        assert!(!is_urgent_line(
            "[doorbell] kind 9. Pull: toron channel read"
        ));
    }

    #[test]
    fn redelivery_floor_is_now_minus_24h() {
        // bd-5d0v: the cursor-less backfill floor is exactly the documented
        // 24h lookback, RFC3339 seconds, parseable by the same parser the
        // high-water uses.
        let now = 1_700_000_000u64;
        let floor = redelivery_floor_rfc3339(now);
        let us = parse_rfc3339_micros(&floor).expect("floor parses");
        assert_eq!(us, (now - 24 * 3600) as i64 * 1_000_000, "{floor}");
        // Saturates instead of underflowing near epoch.
        assert!(parse_rfc3339_micros(&redelivery_floor_rfc3339(10)).is_ok());
    }

    #[test]
    fn doorbell_tick_is_pure_and_deterministic() {
        let model = model_with("BlueRidge");
        let msg = DoorbellMsg::WatchDied;
        let (m1, a1) = tick(&model, &msg);
        let (m2, a2) = tick(&model, &msg);
        assert_eq!(a1, a2);
        assert_eq!(m1.queues.len(), m2.queues.len());
    }

    #[test]
    fn doorbell_tick_watch_died_produces_restart() {
        let model = DoorbellModel::default();
        let (_, actions) = tick(&model, &DoorbellMsg::WatchDied);
        assert!(actions.contains(&DoorbellAction::RestartWatch));
    }

    #[test]
    fn doorbell_tick_watch_line_prompts_idle_pin() {
        let model = model_with("BlueRidge");
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#;
        let (next, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            DoorbellAction::Prompt { pin, line } => {
                assert_eq!(pin, "BlueRidge");
                assert!(line.contains("[doorbell]"));
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
        assert!(next.queues.is_empty(), "idle pin must not queue");
    }

    #[test]
    fn doorbell_tick_watch_line_queues_working_pin_then_drains() {
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "working".into());
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#;
        let (next, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert_eq!(actions.len(), 1, "queue transition leaves one audit");
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "queued"),
            "queued ring must be journal-visible: {:?}",
            actions[0]
        );
        assert_eq!(next.queues.get("BlueRidge").map(Vec::len), Some(1));
        let (next2, actions2) = tick(
            &next,
            &DoorbellMsg::HerdrStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert_eq!(actions2.len(), 1);
        assert!(actions2[0].to_label().contains("BlueRidge"));
        assert!(
            next2
                .queues
                .get("BlueRidge")
                .is_none_or(std::vec::Vec::is_empty)
        );
    }

    #[test]
    fn queue_audit_fires_once_per_nonempty_stretch() {
        // bd-ztek: bounded noise — the second enqueue while queued stays
        // quiet; a new stretch after a drain audits again.
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "working".into());
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#;
        let msg = DoorbellMsg::WatchNdjson { line: line.into() };
        let (next, first) = tick(&model, &msg);
        assert_eq!(first.len(), 1);
        let (next2, second) = tick(&next, &msg);
        assert!(second.is_empty(), "no audit storm while queued");
        assert_eq!(next2.queues.get("BlueRidge").map(Vec::len), Some(2));
    }

    #[test]
    fn unroutable_pin_wake_goes_to_digest_not_queue() {
        // bd-bk9t: a subscribed identity with no spawn row has no pane, so
        // its status can never leave "unknown" and the per-pin queue can
        // never drain. The wake must reach the digest instead of parking in
        // RAM with a delivery intent it can never honour.
        let mut model = model_with("QuietHeron");
        model.statuses.remove("QuietHeron");
        model.unroutable.insert("QuietHeron".into());
        let line = r#"{"kind":9,"id":"ab","tags":[["p","QuietHeron"],["h","ch-1"]],"content":"@QuietHeron hi"}"#;
        let (next, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert_eq!(actions.len(), 1, "one digest-bound wake");
        match &actions[0] {
            DoorbellAction::Digest { pin, line, why } => {
                assert_eq!(pin, "QuietHeron");
                assert!(line.contains("[doorbell]"), "the wake line itself: {line}");
                assert_eq!(why, "no_route", "the skip must name its reason");
            }
            other => panic!("expected Digest, got {other:?}"),
        }
        assert!(
            next.queues.is_empty(),
            "a queueless pin must never accumulate RAM entries: {:?}",
            next.queues
        );
    }

    #[test]
    fn unroutable_pin_still_prompts_when_injectable() {
        // Defensive: routability is about the *pane*, injectability about
        // the *status*. If a pane ever turns up for a previously pane-less
        // pin and it is idle, the prompt gate still wins.
        let mut model = model_with("QuietHeron");
        model.unroutable.insert("QuietHeron".into());
        let line = r#"{"kind":9,"id":"ab","tags":[["p","QuietHeron"],["h","ch-1"]],"content":"@QuietHeron hi"}"#;
        let (next, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert!(
            matches!(&actions[0], DoorbellAction::Prompt { .. }),
            "idle stays injectable: {:?}",
            actions[0]
        );
        assert!(next.queues.is_empty());
    }

    #[test]
    fn routable_unknown_pin_still_queues_not_digests() {
        // Regression: a spawned pin whose status has not arrived yet is a
        // *transient* unknown — it will drain when herdr reports. Only the
        // index-marked pane-less kind is digest-bound.
        let mut model = model_with("BlueRidge");
        model.statuses.remove("BlueRidge");
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#;
        let (next, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "queued"),
            "a routable unknown pin still queues: {:?}",
            actions[0]
        );
        assert_eq!(next.queues.get("BlueRidge").map(Vec::len), Some(1));
    }

    #[test]
    fn mail_delivered_is_routine_mail_not_urgent() {
        // bd-v8vi: a sealed envelope's urgency is unknowable on the watch
        // path — every delivery labeled urgent manufactured phantom
        // urgents (5 QuietHeron digests, zero urgent mail). Delivery rings
        // a routine inbox-pull; urgency comes from the inbox fold.
        let index = SelfIndex::from_pins(["BlueRidge"]);
        let line = r#"{"kind":9,"id":"aa","tags":[["p","BlueRidge"],["toron-kind","mail-delivered"],["h","t-1"]],"content":"sealed"}"#;
        let (class, hint) = parse_watch_ndjson(line, &index).expect("mail-delivered must ring");
        assert_eq!(class, EventClass::PlanePMail);
        assert_ne!(class, EventClass::Channel);
        assert!(
            hint.contains("toron mail inbox"),
            "hint must pull inbox: {hint}"
        );
        let rendered = doorbell_line(class, &hint);
        assert!(
            !is_urgent_line(&rendered),
            "delivery ring must not read urgent: {rendered}"
        );
    }

    #[test]
    fn ack_sla_expired_stays_urgent() {
        // Unacked-corr expiry IS urgent by nature (the ack never came) —
        // only routine deliveries downgrade, never the SLA ring.
        let index = SelfIndex::from_pins(["BlueRidge"]);
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["toron-kind","ack-sla-expired"]],"content":"sealed"}"#;
        let (class, _) = parse_watch_ndjson(line, &index).expect("ack-sla-expired must ring");
        assert_eq!(class, EventClass::PlanePUrgent);
    }

    #[test]
    fn doorbell_tick_mail_delivered_pulls_inbox_not_channel() {
        let model = model_with("BlueRidge");
        let line = r#"{"kind":9,"id":"aa","tags":[["p","BlueRidge"],["toron-kind","mail-delivered"],["h","t-1"]],"content":"sealed"}"#;
        let (_, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            DoorbellAction::Prompt { line, .. } => {
                assert!(line.contains("inbox"), "mail wake must pull inbox: {line}");
                assert!(
                    !line.contains("channel read"),
                    "mail wake must not channel read: {line}"
                );
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn doorbell_tick_room_chatter_is_silent() {
        let mut model = model_with("BlueRidge");
        model.index.insert_pin("LilacChapel".into(), None);
        let line = r#"{"kind":9,"id":"cc","tags":[["h","ch-1"]],"content":"room talk"}"#;
        let (next, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert!(actions.is_empty(), "unaddressed chatter must not ring");
        assert!(next.queues.is_empty());
    }

    #[test]
    fn doorbell_tick_inbox_messages_wrapper_parses() {
        // fetch_inbox also emits {"messages":[...]} — must expand, not treat
        // the wrapper as one unparseable row.
        let model = model_with("BlueRidge");
        let raw = r#"{"messages":[{"importance":"urgent"},{"importance":"high"},{"importance":"normal"}]}"#;
        let (_, actions) = tick(
            &model,
            &DoorbellMsg::InboxItems {
                raw: raw.into(),
                pin: "BlueRidge".into(),
            },
        );
        assert_eq!(actions.len(), 2, "urgent+high ring, normal stays quiet");
    }

    #[test]
    fn doorbell_tick_inbox_rows_prompts_idle_pin() {
        let model = model_with("BlueRidge");
        let rows = vec![
            serde_json::json!({"importance":"urgent"}),
            serde_json::json!({"importance":"high"}),
            serde_json::json!({"importance":"normal"}),
        ];
        let (_, actions) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows,
                pin: "BlueRidge".into(),
            },
        );
        assert_eq!(actions.len(), 2, "urgent+high ring, normal stays quiet");
        for a in &actions {
            assert!(a.to_label().contains("BlueRidge"));
        }
    }

    #[test]
    fn doorbell_tick_bad_inbox_json_is_loud_audit() {
        let model = model_with("BlueRidge");
        let (_, actions) = tick(
            &model,
            &DoorbellMsg::InboxItems {
                raw: "{not json".into(),
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "inbox_parse_failure"),
            "parse failure must be a loud audit, got {:?}",
            actions
        );
    }

    #[test]
    fn three_per_pin_per_window_then_cut() {
        use std::collections::BTreeMap;
        let mut m: BTreeMap<String, super::PinBudget> = BTreeMap::new();
        let one = |pin: &str| vec![(pin.to_string(), "x".into())];
        // 3 prompts for one pin pass; the 4th inside the window drops.
        for _ in 0..3 {
            assert_eq!(super::take_allowed(one("p"), &mut m, 1000).len(), 1);
        }
        assert!(super::take_allowed(one("p"), &mut m, 1001).is_empty());
        // A different pin is unaffected (per-pin buckets).
        assert_eq!(super::take_allowed(one("q"), &mut m, 1001).len(), 1);
        // Window slides: 30 min later the pin passes again.
        assert_eq!(super::take_allowed(one("p"), &mut m, 1000 + 1800).len(), 1);
    }
}

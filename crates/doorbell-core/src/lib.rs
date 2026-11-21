// governed-by: ADR-0012
//! Doorbell pure core (ADR-0012 Phase 0): ring rules, queue coalesce, inject
//! gate, watch/inbox parsing, and the Elm tick. Moved verbatim from
//! `src/doorbell.rs` (pure fns, no signature changes) and `src/doorbell_tick.rs`
//! (verbatim). No I/O, no `&mut` across the API, no tokio/toron/hc/resonate.
//! The effect layer (`cmd_internal_doorbell`, pid files, supervision) stays in
//! flywheel. Tests assert `same Msg log -> byte-identical Action sequence`.

use std::collections::{BTreeMap, BTreeSet};

/// The ONE owner of agent-facing wake text (bd-ofa1s).
pub mod agent_text;
/// Fenced wake kernel (bd-0nem): Model/Msg/Effect/tick live here.
pub mod wake_core;
/// Compat re-exports: decision vocabulary moved into `wake_core` (single
/// home); these paths keep existing users resolving while call sites
/// migrate to `wake_core::`.
pub use wake_core::{
    GateDecision, InjectGate, PresenceVerdict, RouteInput, inject_gate, inject_gate_for_plane,
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
    /// Peer directive (bd-ncgyf): a `p2p:<verb>` mail rings the peer line,
    /// never the system-notice digest.
    P2p,
}

pub mod observe;

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
    if ev.addressed_to_self && ev.kind == 9 && ev.toron_kind_ack_sla {
        return Some(EventClass::PlanePUrgent);
    }
    if ev.addressed_to_self && ev.kind == 9 && ev.toron_kind_mail_delivered {
        return Some(EventClass::PlanePMail);
    }
    if ev.addressed_to_self && ev.kind == 9 {
        return Some(EventClass::Channel);
    }
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
/// MUST use `inject_gate` / `inject_gate_for_plane` instead.
// governed-by: bd-zawk
pub fn should_inject(plane_status: &str) -> bool {
    // "blocked" is a dialog, never a typing target (bd-6qae0).
    matches!(plane_status, "idle" | "done")
}

/// Drain when leaving a non-injectable state for an injectable one.
/// Working and blocked are non-injectable; idle|done are injectable.
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

/// bd-b2b3-serialize: `SelfIndex` is reachable from `WorkspaceModel`, which the
/// run-state checkpoint log must serialize. Both maps are pin -> npub strings;
/// npubs are public keys, so a local snapshot carries no secret.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
        let lower = token.to_lowercase();
        if let Some(pin) = self.npubs.get(&lower) {
            return Some(pin.clone());
        }
        self.pins
            .keys()
            .find(|pin| pin.eq_ignore_ascii_case(token))
            .cloned()
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
    format!("{} {summary}", agent_text::WAKE_PREFIX)
}

pub fn watch_pubkey(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    v.get("pubkey")
        .and_then(|p| p.as_str())
        .map(str::to_lowercase)
}

/// Event id on a watch NDJSON line (gap 17 trust: hint carries proof).
pub fn watch_id(line: &str) -> Option<String> {
    // bd-ofa1s: the event id is typed data on the NDJSON line. Wake text is
    // plain language now and never carries a machine id to parse back out.
    let v = serde_json::from_str::<serde_json::Value>(line).ok()?;
    v.get("id")?.as_str().map(str::to_string)
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
        // ADR-0041 per-message door (bd-next-envelope-ingest-yl0cx made it
        // real: next returns every live straggler; ack advances one). The
        // high-water `mail inbox --since last` can skip messages after the
        // one an agent answers, so the wake points at `next`.
        EventClass::RequestRelease
        | EventClass::PlanePUrgent
        | EventClass::PlanePMail
        | EventClass::P2p => "toron next".into(),
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
    let subject = v.get("subject").and_then(|x| x.as_str()).unwrap_or("");
    // A peer directive is a wake even with an EMPTY subject (live rows exist):
    // the body carries the structured verb and path.
    let body_json = v
        .get("body_md")
        .or_else(|| v.get("body"))
        .and_then(|b| b.as_str())
        .and_then(|b| serde_json::from_str::<serde_json::Value>(b).ok());
    let p2p = subject
        .strip_prefix("p2p:")
        .map(|rest| {
            let mut parts = rest.splitn(2, ' ');
            (
                parts.next().unwrap_or("directive").to_string(),
                parts.next().unwrap_or("").to_string(),
            )
        })
        .or_else(|| {
            body_json.as_ref().and_then(|j| {
                j.get("verb").and_then(|x| x.as_str()).map(|verb| {
                    (
                        verb.to_string(),
                        j.get("path")
                            .and_then(|p| p.as_str())
                            .unwrap_or("")
                            .to_string(),
                    )
                })
            })
        });
    let class = if let Some((verb, path)) = p2p {
        let sender = v.get("sender").and_then(|x| x.as_str()).unwrap_or("peer");
        return Some((
            EventClass::P2p,
            format!("{verb}{P2P_HINT_SEP}{sender}{P2P_HINT_SEP}{path}"),
        ));
    } else if request_release {
        EventClass::RequestRelease
    } else if matches!(importance, "high" | "urgent") {
        EventClass::PlanePUrgent
    } else if is_system_notice_row(v) {
        // A reservation-release notice is machine chatter: it never rings, and
        // it says so in the journal instead of vanishing.
        return Some((EventClass::PlanePMail, SYSTEM_NOTICE_HINT.to_string()));
    } else {
        // bd-0uzrq.8: routine mail used to be dropped HERE with no audit, which
        // is how a captain brief disappeared while a peer wake rang. Ordinary
        // mail is a wake; it queues and rings at the next prompt.
        EventClass::PlanePMail
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

/// One agent-facing wake line (bd-ofa1s). All text comes from `agent_text`.
pub fn doorbell_line(class: EventClass, hint: &str) -> String {
    if class == EventClass::P2p {
        return p2p_line_from_hint(hint);
    }
    agent_text::render(
        class,
        &agent_text::WakeCtx {
            command: hint,
            ..Default::default()
        },
    )
}

/// The hint that marks a system notice: a `PlanePMail` line carrying it is
/// journaled as silent and never rings (bd-0uzrq.1).
pub const SYSTEM_NOTICE_HINT: &str = "system-notice";

/// Is this inbox row the doorbell's own machine notice (empty subject from a
/// 64-hex sender)? One predicate with the wave layer's `is_system_notice`.
pub fn is_system_notice_row(v: &serde_json::Value) -> bool {
    let subject = v.get("subject").and_then(|s| s.as_str()).unwrap_or("");
    let sender = v.get("sender").and_then(|s| s.as_str()).unwrap_or("");
    subject.trim().is_empty() && sender.len() == 64 && sender.chars().all(|c| c.is_ascii_hexdigit())
}

/// What a queued ring line carries, parsed back out of the line itself so the
/// queue stays plain strings across the journal. Order is the ring's priority:
/// a captain-side wake is never hidden behind a peer or machine line.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum WakeKind {
    /// Machine chatter: journaled, never rung on its own.
    Notice,
    /// Routine mail / digest: rides the next real wake.
    Digest,
    /// Peer directive.
    Peer,
    /// Act at idle: a mail wake. Outranks a peer line, so a captain brief is
    /// never buried under peer chatter.
    AtIdle,
    /// Act now (urgent mail, release request).
    ActNow,
}

impl WakeKind {
    /// The word a composed ring uses for this kind.
    pub fn label(self) -> &'static str {
        match self {
            Self::Notice => "system notice",
            Self::Digest => "digest",
            Self::Peer => "peer request",
            Self::AtIdle => "brief",
            Self::ActNow => "urgent",
        }
    }
}

/// Classify a composed or raw ring line.
pub fn wake_kind_of_line(line: &str) -> WakeKind {
    if line.contains(SYSTEM_NOTICE_HINT) {
        return WakeKind::Notice;
    }
    agent_text::classify(line)
}

/// The identity a row's wake carries into the queue.
///
/// It is the RENDERED line's core, not the row id, because only one of the two
/// producers has an id: the inbox row does, the CLI's refused-handoff line does
/// not. Keying on the line is what makes one directive one ring (bd-x3pqw).
/// When the envelope carries a message id to both producers (bd-0uzrq.1), that
/// id becomes the key and this fn goes away.
/// The key a message supersedes on: its thread when it names one, else its
/// `corr` — which IS populated on crew mail (a `crew prompt --bead bd-x` rides
/// `corr=bd-x`), so a captain's correction about the same bead replaces the
/// queued brief instead of ringing beside it (bd-0uzrq.11). Without the
/// fallback the supersede path never fires on real rows: `thread_id` is null
/// everywhere today.
fn row_thread(row: &serde_json::Value) -> Option<String> {
    let field = |key: &str| {
        row.get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    field("thread_id").or_else(|| field("corr"))
}

fn row_key(row: &serde_json::Value, _line: &str) -> String {
    row.get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

/// The `hint` a composed line ends with: every class it carries, most urgent
/// first, then the single pull the pane should run.
pub fn composed_ring(queue: &[QueuedWake]) -> String {
    let mut by_kind: Vec<&QueuedWake> = queue
        .iter()
        .filter(|w| w.kind != WakeKind::Notice)
        .collect();
    if by_kind.is_empty() {
        return String::new();
    }
    // Most urgent class first: a captain-side wake is never hidden behind a
    // peer or machine line.
    by_kind.sort_by_key(|w| std::cmp::Reverse(w.kind));
    let head = by_kind[0]
        .line
        .split(". Read it with:")
        .next()
        .unwrap_or(&by_kind[0].line)
        .trim_end()
        .to_string();
    let rest: Vec<String> = by_kind
        .iter()
        .skip(1)
        .map(|w| format!("{} {}", w.count, w.kind.label()))
        .collect();
    let pull = "Read it with: toron next --project <project> --as <pin>".to_string();
    if rest.is_empty() {
        return format!("{head}. {pull}");
    }
    format!("{head} + {}. {pull}", rest.join(" + "))
}

/// The separator `parse_inbox_item` uses to carry a peer directive's verb,
/// sender and path through the `(class, hint)` shape.
pub const P2P_HINT_SEP: char = '\u{1f}';

/// The peer line a `p2p:` mail rings: the same shape the CLI types, minus the
/// mail watermark (the doorbell has no cursor for the recipient).
pub fn p2p_line_from_hint(hint: &str) -> String {
    let mut parts = hint.split(P2P_HINT_SEP);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(verb), Some(sender), Some(path)) => agent_text::render(
            EventClass::P2p,
            &agent_text::WakeCtx {
                sender,
                path,
                command: verb,
                ..Default::default()
            },
        ),
        _ => agent_text::render(EventClass::P2p, &agent_text::WakeCtx::default()),
    }
}

/// A peer directive wake line. The fold keeps these (they are a direct
/// directive, not channel chatter).
pub fn is_p2p_wake(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("asks you to release")
        || lower.contains("a request about")
        || lower.contains("answered your request")
        || lower.contains("asks you to review")
        || lower.contains("direct message from")
}

/// Parse `--status pin=idle` pairs for the hc-mock (`__doorbell --once`).
pub fn parse_status_pair(s: &str) -> Option<(String, String)> {
    let (pin, st) = s.split_once('=')?;
    if pin.is_empty() || st.is_empty() {
        return None;
    }
    Some((pin.to_string(), st.to_string()))
}

/// One queued wake: a wake CLASS, the newest line of that class, and how many
/// of the class merged into it. Coalescing keeps the queue bounded; the count
/// and the journal rows keep it honest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueuedWake {
    pub kind: WakeKind,
    /// The typed event id this wake came from; rides to Effect::Prompt so a
    /// deferred/boot drain still claims the same marker (bd-ofa1s).
    #[serde(default)]
    pub event_id: Option<String>,
    /// The message identity the producer could give: the inbox row's mail id,
    /// or the event id for a watch line. Empty when the producer has none — the
    /// CLI's refused-handoff line carries only text (bd-x3pqw).
    #[serde(default)]
    pub key: String,
    /// The rendered line's core. An id-less producer folds onto a queued wake
    /// whose core matches, so one directive from two producers is one ring;
    /// an id-bearing producer NEVER matches on the core, so two different
    /// briefs that render the same generic line stay two wakes (bd-x3pqw
    /// review).
    #[serde(default)]
    pub core: String,
    /// The message's thread, when the row carries one. A newer message of the
    /// same thread SUPERSEDES an older queued one (bd-0uzrq.1) — a correction
    /// replaces what it corrects instead of ringing beside it. `None` today:
    /// toron's projection leaves `thread_id` null, so this path is live code
    /// with no live traffic yet.
    #[serde(default)]
    pub thread: Option<String>,
    pub line: String,
    pub count: u32,
}

impl QueuedWake {
    #[must_use]
    pub fn new(kind: WakeKind, key: impl Into<String>, line: impl Into<String>) -> Self {
        Self::with_thread(kind, key, None, line, None)
    }

    #[must_use]
    pub fn with_thread(
        kind: WakeKind,
        key: impl Into<String>,
        thread: Option<String>,
        line: impl Into<String>,
        event_id: Option<String>,
    ) -> Self {
        let line = line.into();
        let core = wake_key(&line);
        Self {
            kind,
            key: key.into(),
            core,
            thread,
            line,
            event_id,
            count: 1,
        }
    }
}

/// What folding one line into the queue did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Queued {
    /// A new class joined the queue.
    Fresh,
    /// A different message of a class already queued: the count rises.
    Merged,
    /// The SAME message arrived again (a second producer): nothing rises, and
    /// the newest text wins. One directive must ring once, never twice.
    Duplicate,
    /// A NEWER message of a thread already queued: it replaces that entry
    /// (bd-0uzrq.1). A correction rings instead of ringing beside the brief it
    /// corrects.
    Superseded,
}

/// The identity of a ring line when no message id is available: the plain
/// text up to the command. Machine identity is typed data (bd-tdpix), never
/// parsed out of this prose.
#[must_use]
pub fn wake_key(line: &str) -> String {
    line.split(". Read it with:")
        .next()
        .unwrap_or(line)
        .trim_end()
        .to_string()
}

/// Fold one line into the queue: same class merges (newest line wins, count
/// bumps), a new class appends. Returns true when it merged.
pub fn queue_line(
    queue: &mut Vec<QueuedWake>,
    key: String,
    thread: Option<String>,
    line: String,
    event_id: Option<String>,
) -> Queued {
    let kind = wake_kind_of_line(&line);
    let core = wake_key(&line);
    let duplicate = queue.iter().position(|w| {
        if !key.is_empty() {
            // An identified message matches only its own identity: two briefs
            // that render the same generic line are two messages.
            !w.key.is_empty() && w.key == key
        } else {
            // The id-less producer (the CLI's handoff) folds onto the message
            // its text names.
            w.core == core
        }
    });
    if let Some(at) = duplicate {
        let w = &mut queue[at];
        w.line = line;
        if event_id.is_some() {
            w.event_id = event_id;
        }
        if !key.is_empty() && w.key.is_empty() {
            w.key = key;
        }
        return Queued::Duplicate;
    }
    // A newer message of an already-queued thread supersedes it: the queue
    // keeps ONE wake per thread, carrying the newest text.
    if let Some(thread) = thread.as_deref().filter(|t| !t.is_empty())
        && let Some(at) = queue
            .iter()
            .position(|w| w.thread.as_deref() == Some(thread))
    {
        let w = &mut queue[at];
        w.kind = kind;
        w.line = line;
        w.event_id = event_id;
        w.key = key;
        w.core = core;
        return Queued::Superseded;
    }
    match queue.iter_mut().find(|w| w.kind == kind) {
        Some(w) => {
            w.line = line;
            w.count = w.count.saturating_add(1);
            Queued::Merged
        }
        None => {
            queue.push(QueuedWake::with_thread(kind, key, thread, line, event_id));
            Queued::Fresh
        }
    }
}

/// One doorbell message. Every bus/inbox/status edge is a Msg; nothing
/// happens outside `tick`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DoorbellMsg {
    /// Plane-S watch NDJSON line (`{kind,id,tags,content}`).
    WatchNdjson { line: String },
    /// A mail.addressed hint from the toron events source, carrying the bus
    /// event id used as the shared dedupe key across wake.sock/events (U8).
    EventsHint {
        event_id: String,
        pin: String,
        line: String,
    },
    /// The events source lost track: one digest wake, not a prompt storm.
    EventsGap { pin: String },
    /// Plane-P inbox rows for one pin (raw `fetch_inbox` JSON).
    InboxItems { raw: String, pin: String },
    /// Plane-P sweep path: rows already parsed (no serde round-trip).
    InboxRows {
        rows: Vec<serde_json::Value>,
        pin: String,
    },
    /// Hc status change for one pin.
    PlaneStatus { pin: String, status: String },
    /// The pane refused a Prompt because it is mid-turn ("not at a prompt").
    /// bd-9mjla: a busy refusal is a typed DEFERRAL, not a delivery failure —
    /// the wake stays in the per-pin queue (one entry, coalesced) and rings
    /// once when the plane next reports at-prompt.
    PromptRefused {
        pin: String,
        line: String,
        event_id: Option<String>,
    },
    /// Captain-bound fact (no alias info here — honest FactMiss).
    Fact { summary: String },
    /// Periodic tick (60s cadence in the live dispatcher); drives the
    /// Ack-SLA scan effect.
    AckSlaTick { now: u64 },
    /// Re-fold of a parked mail line after its toron answer landed
    /// (bd-cursor-reask): identical to WatchNdjson except a stale or
    /// missing read DECIDES by max(host, toron) instead of parking again —
    /// the answer in hand is the freshest possible read, so a re-ask could
    /// never learn more. Never parked, never asked.
    WatchDecideOnly { line: String },
    /// The toron pull-cursor read re-enters here (pipe pattern): the
    /// interpreter ran `toron inbox cursor` for the pin and answers with
    /// the committed cursor (None when toron could not answer). `read_at`
    /// is the answer's wall-clock stamp, journaled so the 5s reuse window
    /// replays deterministically (bd-yc6jf).
    ToronCursor {
        pin: String,
        cursor_us: Option<i64>,
        read_at_secs: u64,
    },
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
        /// Typed bus event id carried from the Watch line (bd-ofa1s/bd-tdpix).
        #[serde(default)]
        event_id: Option<String>,
    },
    /// Fetch signal only (bd-z5ou): per-pin cursors live in the brain, so
    /// this carries no payload — rows re-enter as InboxRows/InboxFetched.
    FetchInbox,
    /// Effect: run `check_ack_sla_expired` for the project (toron mailbox).
    /// Overdue wakes re-enter as WatchNdjson Msgs via plane S.
    CheckAckSla,
    /// Effect: read the pin's committed toron pull cursor (`toron inbox
    /// cursor`); the answer re-enters as `ToronCursor` (bd-yc6jf). Emitted
    /// at most once per parked burst — a coalesced re-ask needs a fresh
    /// stale verdict, never a second action for the same wait.
    ReadToronCursor {
        pin: String,
    },
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

/// bd-b2b3-serialize: reachable from `WorkspaceModel`; see the run-state
/// checkpoint log in toron-core. Serialization is a CAPABILITY here, not a
/// behaviour change: every field is a plain map/set of primitives, so a
/// round trip is exact. The reducer stays pure and untouched.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DoorbellModel {
    /// One entry per wake CLASS per pin (working panes hold their wake here).
    /// Coalescing is by class, never silent: a merge bumps `count` and journals
    /// `merged`, and the ring names every class it carries (bd-0uzrq.8).
    pub queues: BTreeMap<String, Vec<QueuedWake>>,
    /// Message ids already rung (or digested) for this dispatcher. The idle
    /// edge re-reads the inbox (bd-0uzrq.2), so the fold must know what it has
    /// already delivered or a still-unread row would ring on every transition.
    #[serde(default)]
    pub rung: std::collections::BTreeSet<String>,
    /// Last-known hc status per pin.
    pub statuses: BTreeMap<String, String>,
    /// Pins currently held behind a dialog, with the kind we last saw. One
    /// plain-language notice per episode; cleared on the next status away
    /// from blocked (bd-6qae0).
    #[serde(default)]
    pub blocked_episode: BTreeMap<String, String>,
    /// Pins (+npubs) this dispatcher owns.
    pub index: SelfIndex,
    /// Per-pin prompt budgets (D4.5, bd-asr2.9): durable model state,
    /// rebuilt exactly by journal refold (budget rows). The gate stays in
    /// the effect layer (take_allowed below) — threading it into tick
    /// would reshape decided rows and break the .6 parity gate.
    pub budgets: BTreeMap<String, PinBudget>,
    /// Pins the index knows but cannot route to: subscribed identities with
    /// no spawn row, so no pane and no hc target (bd-bk9t). Their wakes
    /// must NOT park in `queues` — that map only drains on a
    /// `working -> injectable` [`is_drain_transition`], and a pane-less pin
    /// never produces one, so the entry would sit in RAM forever and die
    /// with the process. `route_or_queue` sends them to the digest instead.
    pub unroutable: BTreeSet<String>,
    /// Per-pin plane-P pull cursor, in unix micros — the one notion of
    /// consumed (bd-yc6jf). Fed from the host's `WakeState.since_by_pin`. A
    /// mail wake whose event time is at or below this cursor was already
    /// pulled by `toron next` and must not ring an empty wake.
    #[serde(default)]
    pub pulled_us: BTreeMap<String, i64>,
    /// Pins with an unanswered toron-cursor read (bd-yc6jf): a burst of
    /// mail for one of these parks without a second ask.
    #[serde(default)]
    pub cursor_inflight: std::collections::BTreeSet<String>,
    /// The latest answered toron pull-cursor read per pin (bd-yc6jf).
    /// Freshness is judged against each mail's own event time, so the
    /// 5s reuse window replays deterministically.
    #[serde(default)]
    pub toron_cursor: BTreeMap<String, ToronRead>,
    /// Mail ring candidates parked while their pin's toron read is in
    /// flight (bd-yc6jf). Re-folded as watch lines when the answer lands.
    #[serde(default)]
    pub parked_mail: Vec<ParkedMail>,
}

/// A mail event at or below the pin's pull cursor is consumed, not a wake
/// (bd-yc6jf). Pure so one message log can prove a pulled message never rings.
pub fn already_pulled(pulled_us: &BTreeMap<String, i64>, pin: &str, created_us: i64) -> bool {
    pulled_us
        .get(pin)
        .is_some_and(|cursor| created_us <= *cursor)
}

/// One answered toron pull-cursor read (bd-yc6jf). `cursor_us` is None
/// when toron could not answer — cached like a value so a failing toron
/// costs one read per window, not one per mail. `read_at_secs` is the
/// answer's stamp; freshness is judged against each mail's own event
/// time, keeping the reuse window replay-deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToronRead {
    pub cursor_us: Option<i64>,
    pub read_at_secs: u64,
}

/// A mail ring candidate parked while its pin's toron read is in flight
/// (bd-yc6jf). Re-folded as a watch line when the answer lands, so the
/// parked path and the fresh path share one decision.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ParkedMail {
    pub pin: String,
    pub line: String,
}

/// A cached toron read is fresh for a mail when the read answered no
/// more than 5s before the mail's own event time (bd-yc6jf). Both
/// stamps are journaled, so the verdict replays exactly.
pub fn toron_read_is_fresh(read: &ToronRead, created_secs: u64) -> bool {
    read.read_at_secs.saturating_add(5) >= created_secs
}

/// Per-pin 5/30/3 budget (D1 item 2, D4.5 home).
/// Rolling token bucket: 3 prompts per 30-min window, refilled continuously.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
/// A mail wake: the router's plane-P mail class or the mail-wake host's line.
/// These always reach a pane — a done mail that never rings is a lost
/// hand-off (bd-346jl); the afk batch only applies to routine channel traffic.
pub fn is_mail_wake(line: &str) -> bool {
    line.contains("new mail")
        || line.contains("New mail")
        || line.starts_with("Doorbell: mail from")
}

/// May this line PREEMPT a pane that is already occupied? Only an explicit
/// act-now does: the captain's urgent mail and an urgent steer.
///
/// A peer request is a queue-and-wait event, not a preemption. Treating it as
/// urgent typed it into a WORKING pane — which the harness swallows, so the
/// crew never saw it — and the preempt arm recorded no key, so the CLI's own
/// deferred copy rang the same message a second time at the drain
/// (bd-0uzrq.13, the peer Prompts at 05:29:41, 09:24:43 and 10:58:15Z).
pub fn is_act_now_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("urgent mail") || lower.contains("urgent steer")
}

/// Does this line carry urgency worth classifying as such (wake class, digest
/// escalation)? Broader than [`is_act_now_line`] on purpose: a peer request is
/// urgent enough to name, never urgent enough to interrupt.
pub fn is_urgent_line(line: &str) -> bool {
    matches!(
        agent_text::classify(line),
        WakeKind::ActNow | WakeKind::Peer
    ) || line.contains("URGENT steer")
        || line.contains("ack-sla-expired")
        || line.contains("unacked for")
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

/// Pure, total, deterministic: no I/O, no &mut, no hc/toron calls.
pub fn tick(model: &DoorbellModel, msg: &DoorbellMsg) -> (DoorbellModel, Vec<DoorbellAction>) {
    let mut next = model.clone();
    let mut actions = Vec::new();
    match msg {
        DoorbellMsg::WatchNdjson { line } | DoorbellMsg::WatchDecideOnly { line } => {
            // A decide-only re-fold (bd-cursor-reask) decides with the answer
            // in hand; it never parks and never asks again.
            let decide_only = matches!(msg, DoorbellMsg::WatchDecideOnly { .. });
            let Some((class, hint)) = parse_watch_ndjson(line, &next.index) else {
                return (next, actions);
            };
            let v = serde_json::from_str::<serde_json::Value>(line).ok();
            let event_id = v
                .as_ref()
                .and_then(|v| {
                    v.get("id")
                        .and_then(serde_json::Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let tags = v
                .as_ref()
                .and_then(|v| v.get("tags"))
                .and_then(|t| t.as_array());
            let thread = tags.and_then(|tags| {
                tags.iter().find_map(|t| {
                    let a = t.as_array()?;
                    if a.first()?.as_str()? == "h" {
                        a.get(1)?
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    } else {
                        None
                    }
                })
            });
            let message_id = tags.and_then(|tags| {
                tags.iter().find_map(|t| {
                    let a = t.as_array()?;
                    let k = a.first()?.as_str()?;
                    if matches!(k, "mid" | "message-id" | "msg") {
                        a.get(1)?
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                    } else {
                        None
                    }
                })
            });
            // Only a MAIL DELIVERY has an inbox row behind it. An ack-SLA or
            // other urgent kind-9 rings on its own event and must not be
            // skipped with it.
            let mail = matches!(class, EventClass::PlanePMail);
            // A mail wake without a message id (replayed pre-mid events) has no
            // identity the inbox row can match, so a ring built here could only
            // double-ring the same mail later. The fetch carries the id; it owns
            // that delivery.
            let unidentified_mail = mail && message_id.is_none();
            let (key, queue_thread) = if unidentified_mail {
                (String::new(), None)
            } else if mail {
                (message_id.unwrap_or_else(|| event_id.clone()), thread)
            } else {
                (event_id.clone(), None)
            };
            let line_out = doorbell_line(class, &hint);
            let created_us = watch_created_at(line).map(|s| (s as i64).saturating_mul(1_000_000));
            for pin in watch_targets(line, &next.index) {
                // bd-yc6jf: the pull cursor is the one notion of consumed. A
                // mail event at or below it was already pulled by toron next —
                // no wake for an empty inbox. Ack-SLA and other urgent kinds
                // have no inbox row and are never skipped here.
                if mail && created_us.is_some_and(|us| already_pulled(&next.pulled_us, &pin, us)) {
                    actions.push(DoorbellAction::Audit {
                        kind: "already-pulled".into(),
                        pin: pin.clone(),
                        detail: format!("mail at {created_us:?} is at or below the pull cursor"),
                    });
                    continue;
                }
                // bd-yc6jf: the toron cursor is the second half of the one
                // notion. A fresh read decides by max(host, toron); a
                // missing or stale read parks the candidate and asks toron
                // once per burst. A line without event time cannot be
                // compared, so it rings as today (fail toward delivery).
                // bd-cursor-reask: a decide-only re-fold never parks — the
                // answer in hand is the freshest possible read, so it
                // decides by max(host, toron) even when stale.
                if mail && let Some(us) = created_us {
                    let created_secs = (us / 1_000_000).max(0) as u64;
                    let fresh = next
                        .toron_cursor
                        .get(&pin)
                        .is_some_and(|t| toron_read_is_fresh(t, created_secs));
                    if fresh || decide_only {
                        match next.toron_cursor.get(&pin).and_then(|t| t.cursor_us) {
                            Some(tc) if us <= tc => {
                                actions.push(DoorbellAction::Audit {
                                    kind: "already-pulled".into(),
                                    pin: pin.clone(),
                                    detail: format!(
                                        "mail at {created_us:?} is at or below the toron cursor"
                                    ),
                                });
                                continue;
                            }
                            None => {
                                actions.push(DoorbellAction::Audit {
                                    kind: "toron-cursor-unavailable".into(),
                                    pin: pin.clone(),
                                    detail: "toron could not answer; ringing toward delivery"
                                        .into(),
                                });
                            }
                            _ => {}
                        }
                    } else {
                        if next.cursor_inflight.insert(pin.clone()) {
                            actions.push(DoorbellAction::ReadToronCursor { pin: pin.clone() });
                        }
                        next.parked_mail.push(ParkedMail {
                            pin: pin.clone(),
                            line: line.clone(),
                        });
                        continue;
                    }
                }
                if unidentified_mail {
                    actions.push(DoorbellAction::Audit {
                        kind: "unidentified_mail_wake".into(),
                        pin: pin.clone(),
                        detail: format!(
                            "event {event_id} carries no message id; the inbox fetch owns this mail"
                        ),
                    });
                    continue;
                }
                route_or_queue(
                    &mut next,
                    &mut actions,
                    &pin,
                    key.clone(),
                    queue_thread.clone(),
                    line_out.clone(),
                    watch_id(line),
                );
            }
        }
        // bd-l5vb2: a harness with no inbound door is reached through the
        // toron events source. The bus event id is the SAME dedupe key the
        // wake.sock arm already journals in `rung`, so a seat reached by
        // both rings gets exactly one prompt.
        DoorbellMsg::EventsHint {
            event_id,
            pin,
            line,
        } => {
            route_or_queue(
                &mut next,
                &mut actions,
                pin,
                event_id.clone(),
                None,
                line.clone(),
                Some(event_id.clone()),
            );
        }
        DoorbellMsg::EventsGap { pin } => {
            // A gap is ONE digest wake, never a prompt storm (U8).
            actions.push(DoorbellAction::Digest {
                pin: pin.clone(),
                line: doorbell_line(EventClass::PlanePMail, "events may have been missed"),
                why: "events-gap".into(),
            });
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
                let key = row_key(item, &line_out);
                let thread = row_thread(item);
                route_or_queue(&mut next, &mut actions, pin, key, thread, line_out, None);
            }
        }
        DoorbellMsg::InboxRows { rows, pin } => {
            for item in rows {
                let Some((class, hint)) = parse_inbox_item(item) else {
                    continue;
                };
                let line_out = doorbell_line(class, &hint);
                let key = row_key(item, &line_out);
                let thread = row_thread(item);
                route_or_queue(&mut next, &mut actions, pin, key, thread, line_out, None);
            }
        }
        DoorbellMsg::PlaneStatus { pin, status } => {
            let from = next
                .statuses
                .get(pin)
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            if status != "blocked" {
                next.blocked_episode.remove(pin);
            }
            if is_drain_transition(&from, status) {
                let deferred = next
                    .queues
                    .get_mut(pin)
                    .map(std::mem::take)
                    .unwrap_or_default();
                let total: u32 = deferred.iter().map(|w| w.count).sum();
                if deferred.is_empty() {
                    // bd-0uzrq.2: the pane is at a prompt and the queue knows
                    // nothing. The wake may never have been queued (mail sent
                    // while this dispatcher was down, or lost between the
                    // journal append and the file rewrite), so the doorbell
                    // asks for the inbox itself. Already-rung ids are refused
                    // by the fold, so a re-read is free.
                    actions.push(DoorbellAction::FetchInbox);
                } else if deferred.iter().all(|w| w.kind == WakeKind::Notice) {
                    // Machine notices never ring, even alone.
                    actions.push(DoorbellAction::Audit {
                        kind: "silent".into(),
                        pin: pin.clone(),
                        detail: format!("{total} system notice(s) dropped at prompt"),
                    });
                } else {
                    for w in &deferred {
                        if !w.key.is_empty() {
                            next.rung.insert(w.key.clone());
                        }
                    }
                    let ring = composed_ring(&deferred);
                    actions.push(DoorbellAction::Audit {
                        kind: "drained".into(),
                        pin: pin.clone(),
                        detail: format!("{total} deferred wake(s) at prompt: {ring}"),
                    });
                    let event_id = deferred.iter().find_map(|w| w.event_id.clone());
                    actions.push(DoorbellAction::Prompt {
                        pin: pin.clone(),
                        line: ring,
                        event_id,
                    });
                }
            }
            next.statuses.insert(pin.clone(), status.clone());
        }
        DoorbellMsg::PromptRefused {
            pin,
            line,
            event_id,
        } => {
            next.statuses.insert(pin.clone(), "working".into());
            let queue = next.queues.entry(pin.clone()).or_default();
            let fold = queue_line(queue, String::new(), None, line.clone(), event_id.clone());
            actions.push(DoorbellAction::Audit {
                kind: match fold {
                    Queued::Fresh => "deferred",
                    Queued::Merged => "merged",
                    Queued::Duplicate => "dedup",
                    Queued::Superseded => "superseded",
                }
                .into(),
                pin: pin.clone(),
                detail: format!("{line} (queue {} class(es))", queue.len()),
            });
        }
        DoorbellMsg::Fact { summary } => {
            actions.push(DoorbellAction::FactMiss {
                why: format!("fact without captain alias: {}", summary),
            });
        }
        DoorbellMsg::AckSlaTick { .. } => {
            actions.push(DoorbellAction::CheckAckSla);
            actions.push(DoorbellAction::FetchInbox);
        }
        DoorbellMsg::ToronCursor {
            pin,
            cursor_us,
            read_at_secs,
        } => {
            // bd-yc6jf: record the answer, then re-fold every parked
            // candidate for this pin as decide-only — the parked path and
            // the fresh path share one decision, and a stale answer
            // decides instead of parking behind a second ask
            // (bd-cursor-reask).
            next.toron_cursor.insert(
                pin.clone(),
                ToronRead {
                    cursor_us: *cursor_us,
                    read_at_secs: *read_at_secs,
                },
            );
            next.cursor_inflight.remove(pin);
            let parked: Vec<String> = next
                .parked_mail
                .extract_if(.., |p| p.pin == *pin)
                .map(|p| p.line)
                .collect();
            for line in parked {
                let (m, a) = tick(&next, &DoorbellMsg::WatchDecideOnly { line });
                next = m;
                actions.extend(a);
            }
        }
        DoorbellMsg::WatchDied => {
            actions.push(DoorbellAction::RestartWatch);
        }
    }
    (next, actions)
}

/// Idle/done panes get a Prompt now; blocked+act-now prompts while
/// blocked+routine digests with a reason (bd-zawk G6); working panes queue
/// (drained on the next working -> idle|done|blocked PlaneStatus).
fn route_or_queue(
    next: &mut DoorbellModel,
    actions: &mut Vec<DoorbellAction>,
    pin: &str,
    key: String,
    thread: Option<String>,
    line: String,
    event_id: Option<String>,
) {
    let status = next
        .statuses
        .get(pin)
        .map(String::as_str)
        .unwrap_or("unknown");
    if !key.is_empty() && next.rung.contains(&key) {
        actions.push(DoorbellAction::Audit {
            kind: "already_rung".into(),
            pin: pin.to_string(),
            detail: line,
        });
        return;
    }
    if should_inject(status) {
        match inject_gate_for_plane(status, is_act_now_line(&line)) {
            InjectGate::Inject => {
                if !key.is_empty() {
                    next.rung.insert(key);
                }
                actions.push(DoorbellAction::Prompt {
                    pin: pin.to_string(),
                    line,
                    event_id: event_id.clone(),
                });
            }
            InjectGate::Digest(reason) => {
                if !key.is_empty() {
                    next.rung.insert(key);
                }
                actions.push(DoorbellAction::Digest {
                    pin: pin.to_string(),
                    line: format!("{line} [{reason}]"),
                    why: reason.to_string(),
                });
            }
        }
    } else if status == "blocked" {
        // One plain-language notice per blocked episode: the captain learns
        // which seat is held and on what, without a notice on every ring.
        let kind = agent_text::blocked_kind(&line).to_string();
        if !next.blocked_episode.contains_key(pin) {
            next.blocked_episode.insert(pin.to_string(), kind.clone());
            actions.push(DoorbellAction::Audit {
                kind: "blocked_notice".into(),
                pin: pin.to_string(),
                detail: agent_text::blocked_notice(pin, &kind),
            });
        }
        let queue = next.queues.entry(pin.to_string()).or_default();
        queue_line(queue, key, thread, line.clone(), event_id.clone());
    } else if next.unroutable.contains(pin) {
        actions.push(DoorbellAction::Digest {
            pin: pin.to_string(),
            line,
            why: "no_route".into(),
        });
    } else {
        if matches!(
            inject_gate_for_plane(status, is_act_now_line(&line)),
            InjectGate::Inject
        ) {
            if !key.is_empty() {
                next.rung.insert(key);
            }
            actions.push(DoorbellAction::Prompt {
                pin: pin.to_string(),
                line,
                event_id: event_id.clone(),
            });
            return;
        }
        let queue = next.queues.entry(pin.to_string()).or_default();
        let fold = queue_line(queue, key, thread, line.clone(), event_id.clone());
        actions.push(DoorbellAction::Audit {
            kind: match fold {
                Queued::Fresh => "queued",
                Queued::Merged => "merged",
                Queued::Duplicate => "dedup",
                Queued::Superseded => "superseded",
            }
            .into(),
            pin: pin.to_string(),
            detail: format!("{line} (queue {} class(es))", queue.len()),
        });
    }
}

impl DoorbellAction {
    /// Test/debug label (also keeps the enum honest in logs).
    pub fn to_label(&self) -> String {
        match self {
            Self::Prompt { pin, .. } => format!("prompt {pin}"),
            Self::FetchInbox => "fetch-inbox".into(),
            Self::CheckAckSla => "check-ack-sla".into(),
            Self::ReadToronCursor { pin } => format!("read-toron-cursor {pin}"),
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

    /// bd-yc6jf: over ONE message log, once the pin's pull cursor has passed a
    /// mail event, the doorbell must not ring it again — the same consumed
    /// fact the captain watch uses. Ack-SLA (non-mail) has no inbox row and is
    /// never suppressed by the cursor.
    #[test]
    fn a_pulled_mail_event_never_re_rings() {
        let npub = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let mut index = SelfIndex::default();
        index.insert_pin("BlueRidge".into(), Some(npub.into()));
        let watch = format!(
            r#"{{"kind":9,"id":"wake-pulled-1","pubkey":"senderpk","created_at":1700000000,"tags":[["p","{npub}"],["t","flywheel"],["h","thread-1"],["toron-kind","mail-delivered"],["thread","thread-1"],["importance","normal"],["from","SlateHarbor"],["mid","msg-pulled-1"]],"content":"mail"}}"#
        );
        let mut fresh = DoorbellModel {
            index: index.clone(),
            ..Default::default()
        };
        fresh.statuses.insert("BlueRidge".into(), "idle".into());
        // bd-yc6jf: a ring candidate asks toron first — no Prompt yet.
        let (asked, ask_actions) = tick(
            &fresh,
            &DoorbellMsg::WatchNdjson {
                line: watch.clone(),
            },
        );
        assert!(
            ask_actions.iter().any(
                |a| matches!(a, DoorbellAction::ReadToronCursor { pin } if pin == "BlueRidge")
            ),
            "a ring candidate asks toron: {ask_actions:?}"
        );
        assert!(
            !ask_actions
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { pin, .. } if pin == "BlueRidge")),
            "nothing rings before the answer: {ask_actions:?}"
        );
        // The toron answer is behind the mail: the parked ring fires.
        let (_, fresh_actions) = tick(
            &asked,
            &DoorbellMsg::ToronCursor {
                pin: "BlueRidge".into(),
                cursor_us: Some(0),
                read_at_secs: 1_700_000_000,
            },
        );
        assert!(
            fresh_actions
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { pin, .. } if pin == "BlueRidge")),
            "unpulled mail rings once toron answers behind it: {fresh_actions:?}"
        );

        let mut pulled = fresh.clone();
        // created_at 1700000000 secs -> 1700000000000000 micros; the pull
        // cursor at or past that time means the mail was already consumed.
        pulled
            .pulled_us
            .insert("BlueRidge".into(), 1_700_000_000_000_000);
        let (_, pulled_actions) = tick(&pulled, &DoorbellMsg::WatchNdjson { line: watch });
        assert!(
            !pulled_actions
                .iter()
                .any(|a| matches!(a, DoorbellAction::ReadToronCursor { .. })),
            "the host cursor short-circuits before any ask: {pulled_actions:?}"
        );
        assert!(
            !pulled_actions
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { pin, .. } if pin == "BlueRidge")),
            "a pulled message must not re-ring: {pulled_actions:?}"
        );
        assert!(
            pulled_actions.iter().any(
                |a| matches!(a, DoorbellAction::Audit { kind, .. } if kind == "already-pulled")
            ),
            "the skipped wake is audited, never silent"
        );
    }

    /// bd-yc6jf: ONE message log. A mail watch with no usable toron read
    /// parks the ring and asks toron exactly once per burst; the answered
    /// read then decides by max(host cursor, toron cursor): covered mail
    /// drops with an audit, uncovered mail rings, an unanswered toron
    /// rings anyway with an audit. A read older than 5s by event time
    /// re-asks; a dateless line rings as today (no ask without event time).
    #[test]
    fn a_cli_pull_at_or_after_created_at_suppresses_the_next_ring() {
        let npub = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let mut index = SelfIndex::default();
        index.insert_pin("BlueRidge".into(), Some(npub.into()));
        let watch = format!(
            r#"{{"kind":9,"id":"wake-cursor-1","pubkey":"senderpk","created_at":1700000000,"tags":[["p","{npub}"],["t","flywheel"],["h","thread-1"],["toron-kind","mail-delivered"],["thread","thread-1"],["importance","normal"],["from","SlateHarbor"],["mid","msg-cursor-1"]],"content":"mail"}}"#
        );
        let edge = || DoorbellMsg::WatchNdjson {
            line: watch.clone(),
        };
        let model0 = DoorbellModel {
            index: index.clone(),
            statuses: [("BlueRidge".into(), "idle".into())].into(),
            ..DoorbellModel::default()
        };
        // No toron read yet: park the candidate and ask exactly once.
        let (m1, a1) = tick(&model0, &edge());
        assert!(
            a1.iter().any(
                |a| matches!(a, DoorbellAction::ReadToronCursor { pin } if pin == "BlueRidge")
            ),
            "a ring candidate asks toron: {a1:?}"
        );
        assert!(
            !a1.iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { pin, .. } if pin == "BlueRidge")),
            "nothing rings before the answer: {a1:?}"
        );
        // A burst while the read is in flight coalesces: still one ask.
        let (m2, a2) = tick(&m1, &edge());
        assert_eq!(
            a2.iter()
                .filter(|a| matches!(a, DoorbellAction::ReadToronCursor { .. }))
                .count(),
            0,
            "a burst asks once: {a2:?}"
        );
        // The CLI pull covered the mail: the parked ring drops, audited.
        let (m3, a3) = tick(
            &m2,
            &DoorbellMsg::ToronCursor {
                pin: "BlueRidge".into(),
                cursor_us: Some(1_700_000_000_000_000),
                read_at_secs: 1_700_000_000,
            },
        );
        assert!(
            !a3.iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { pin, .. } if pin == "BlueRidge")),
            "covered mail never rings: {a3:?}"
        );
        assert!(
            a3.iter().any(
                |a| matches!(a, DoorbellAction::Audit { kind, .. } if kind == "already-pulled")
            ),
            "the drop is audited: {a3:?}"
        );
        // A later mail inside the 5s window decides from the cache: no ask.
        let watch2 = format!(
            r#"{{"kind":9,"id":"wake-cursor-1b","pubkey":"senderpk","created_at":1700000000,"tags":[["p","{npub}"],["t","flywheel"],["h","thread-1"],["toron-kind","mail-delivered"],["thread","thread-1"],["importance","normal"],["from","SlateHarbor"],["mid","msg-cursor-1b"]],"content":"mail"}}"#
        );
        let (m4, a4) = tick(&m3, &DoorbellMsg::WatchNdjson { line: watch2 });
        assert!(
            !a4.iter()
                .any(|a| matches!(a, DoorbellAction::ReadToronCursor { .. })),
            "a fresh read is reused: {a4:?}"
        );
        assert!(
            !a4.iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { pin, .. } if pin == "BlueRidge")),
            "still covered: {a4:?}"
        );
        let _ = m4;
    }

    /// bd-yc6jf: an unanswered toron rings anyway (fail toward delivery),
    /// and a read older than 5s by event time re-asks.
    #[test]
    fn an_unanswered_toron_rings_anyway_and_a_stale_read_reasks() {
        let npub = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let mut index = SelfIndex::default();
        index.insert_pin("BlueRidge".into(), Some(npub.into()));
        let watch = format!(
            r#"{{"kind":9,"id":"wake-cursor-2","pubkey":"senderpk","created_at":1700000000,"tags":[["p","{npub}"],["t","flywheel"],["h","thread-1"],["toron-kind","mail-delivered"],["thread","thread-1"],["importance","normal"],["from","SlateHarbor"],["mid","msg-cursor-2"]],"content":"mail"}}"#
        );
        let edge = || DoorbellMsg::WatchNdjson {
            line: watch.clone(),
        };
        let model0 = DoorbellModel {
            index: index.clone(),
            statuses: [("BlueRidge".into(), "idle".into())].into(),
            ..DoorbellModel::default()
        };
        let (m1, _) = tick(&model0, &edge());
        let (_, a_fail) = tick(
            &m1,
            &DoorbellMsg::ToronCursor {
                pin: "BlueRidge".into(),
                cursor_us: None,
                read_at_secs: 1_700_000_000,
            },
        );
        assert!(
            a_fail
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { pin, .. } if pin == "BlueRidge")),
            "no answer still rings: {a_fail:?}"
        );
        assert!(
            a_fail.iter().any(|a| matches!(a, DoorbellAction::Audit { kind, .. } if kind == "toron-cursor-unavailable")),
            "the fallback is audited: {a_fail:?}"
        );
        // A read answered long before the mail is stale: the answer still
        // decides (fail-open) instead of parking behind a second ask that
        // the deliver path would drop — that wedge is bd-cursor-reask.
        let (m2, _) = tick(&model0, &edge());
        let (_, a_stale) = tick(
            &m2,
            &DoorbellMsg::ToronCursor {
                pin: "BlueRidge".into(),
                cursor_us: Some(1),
                read_at_secs: 1_699_999_000,
            },
        );
        assert!(
            !a_stale
                .iter()
                .any(|a| matches!(a, DoorbellAction::ReadToronCursor { .. })),
            "a stale answer decides; it never re-asks: {a_stale:?}"
        );
        assert!(
            a_stale
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { pin, .. } if pin == "BlueRidge")),
            "uncovered mail rings on the stale answer: {a_stale:?}"
        );
    }

    /// bd-cursor-reask-dropped-15ail: TWO rounds under sender clock skew.
    /// The mail is future-dated (created far ahead of the toron answer),
    /// so the answer is stale on arrival. Round one parks and asks once;
    /// round two must RING (fail-open), never park behind a second ask
    /// that no interpreter executes — that wedge suppressed the pin forever.
    #[test]
    fn toron_cursor_stale_answer_rings_instead_of_reasking() {
        let npub = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let mut index = SelfIndex::default();
        index.insert_pin("BlueRidge".into(), Some(npub.into()));
        // created_at far ahead of any honest toron read: sender clock skew.
        let watch = format!(
            r#"{{"kind":9,"id":"wake-skew-1","pubkey":"senderpk","created_at":1800000000,"tags":[["p","{npub}"],["t","flywheel"],["h","thread-1"],["toron-kind","mail-delivered"],["thread","thread-1"],["importance","normal"],["from","SlateHarbor"],["mid","msg-skew-1"]],"content":"mail"}}"#
        );
        let model0 = DoorbellModel {
            index: index.clone(),
            statuses: [("BlueRidge".into(), "idle".into())].into(),
            ..DoorbellModel::default()
        };
        // Round one: park the candidate and ask exactly once.
        let (m1, a1) = tick(&model0, &DoorbellMsg::WatchNdjson { line: watch });
        assert_eq!(
            a1.iter()
                .filter(|a| matches!(a, DoorbellAction::ReadToronCursor { .. }))
                .count(),
            1,
            "one ask per burst: {a1:?}"
        );
        assert!(
            !a1.iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { pin, .. } if pin == "BlueRidge")),
            "nothing rings before the answer: {a1:?}"
        );
        // Round two: the answer is stale (read long before created), so it
        // must decide — ring — instead of emitting a second ask.
        let (_, a2) = tick(
            &m1,
            &DoorbellMsg::ToronCursor {
                pin: "BlueRidge".into(),
                cursor_us: Some(1_700_000_000_000_000),
                read_at_secs: 1_700_000_000,
            },
        );
        assert!(
            !a2.iter()
                .any(|a| matches!(a, DoorbellAction::ReadToronCursor { .. })),
            "no second ask to drop: {a2:?}"
        );
        assert!(
            a2.iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { pin, .. } if pin == "BlueRidge")),
            "future-dated mail rings on the stale answer: {a2:?}"
        );
    }

    #[test]
    fn presence_word_map_is_single_sourced() {
        for w in ["idle", "done", " IDLE "] {
            assert_eq!(parse_presence_word(w), PresenceVerdict::Idle);
        }
        for w in ["working", "busy"] {
            assert_eq!(parse_presence_word(w), PresenceVerdict::Busy);
        }
        assert_eq!(parse_presence_word("blocked"), PresenceVerdict::Blocked);
        for w in ["dead", "endpoint-dead", "endpoint"] {
            assert_eq!(parse_presence_word(w), PresenceVerdict::Dead);
        }
        for w in ["", "mystery", "null"] {
            assert_eq!(parse_presence_word(w), PresenceVerdict::Unknown);
        }
    }

    #[test]
    fn a_blocked_episode_notices_once_then_goes_quiet() {
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "blocked".into());
        let line = "[wake] mail. permission prompt is open";
        let mut first = Vec::new();
        route_or_queue(
            &mut model,
            &mut first,
            "BlueRidge",
            wake_key(line),
            None,
            line.to_string(),
            None,
        );
        let notices = |actions: &[DoorbellAction]| {
            actions
                .iter()
                .filter(
                    |a| matches!(a, DoorbellAction::Audit { kind, .. } if kind == "blocked_notice"),
                )
                .count()
        };
        assert_eq!(notices(&first), 1, "one notice for the episode: {first:?}");
        assert!(
            first.iter().any(|a| matches!(
                a,
                DoorbellAction::Audit { detail, .. }
                    if detail.contains("BlueRidge") && detail.contains("permission prompt")
            )),
            "the notice must name the seat and the dialog: {first:?}"
        );
        let mut second = Vec::new();
        route_or_queue(
            &mut model,
            &mut second,
            "BlueRidge",
            wake_key("[wake] more mail"),
            None,
            "[wake] more mail".to_string(),
            None,
        );
        assert_eq!(notices(&second), 0, "no notice on every ring: {second:?}");

        // A new episode (after blocked -> idle) gets one fresh notice.
        let (next, _) = tick(
            &model,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        model = next;
        model.statuses.insert("BlueRidge".into(), "blocked".into());
        let mut third = Vec::new();
        route_or_queue(
            &mut model,
            &mut third,
            "BlueRidge",
            wake_key("[wake] new episode"),
            None,
            "[wake] new episode".to_string(),
            None,
        );
        assert_eq!(notices(&third), 1, "a new episode notices again: {third:?}");
    }

    #[test]
    fn blocked_to_idle_still_drains_the_queue() {
        assert!(is_drain_transition("blocked", "idle"));
        assert!(!is_drain_transition("blocked", "working"));
        assert!(!should_inject("blocked"));
    }

    #[test]
    fn inject_gate_blocked_never_types_even_when_urgent() {
        assert!(matches!(
            inject_gate_for_plane("blocked", true),
            InjectGate::Digest(_)
        ));
        assert!(matches!(
            inject_gate_for_plane("blocked", false),
            InjectGate::Digest(_)
        ));
        assert_eq!(inject_gate_for_plane("idle", false), InjectGate::Inject);
        assert_eq!(inject_gate_for_plane("done", false), InjectGate::Inject);
        assert!(matches!(
            inject_gate_for_plane("working", false),
            InjectGate::Digest(_)
        ));
        assert!(matches!(
            inject_gate_for_plane("unknown", true),
            InjectGate::Digest(_)
        ));
        assert_eq!(inject_gate(PresenceVerdict::Busy, true), InjectGate::Inject);
        assert!(matches!(
            inject_gate(PresenceVerdict::Busy, false),
            InjectGate::Digest(_)
        ));
    }

    #[test]
    fn fact_promotion_prefers_captain_over_wheelhouse() {
        let mut roles = BTreeMap::new();
        assert_eq!(fact_captain_pin(&roles), None);
        roles.insert("wheelhouse".into(), "PinW".into());
        assert_eq!(fact_captain_pin(&roles).as_deref(), Some("PinW"));
        roles.insert("captain".into(), "PinC".into());
        assert_eq!(fact_captain_pin(&roles).as_deref(), Some("PinC"));
        assert_eq!(fact_prompt_line("deploy"), "[wake] deploy");
        assert!(
            is_urgent_line("[reclassify] vehicle: none (topology=open) — miss"),
            "GS-048 reclassify announce must deliver"
        );
        assert!(!is_urgent_line(
            "[wake] New message in the team room. Read it with: toron channel read"
        ));
    }

    #[test]
    fn redelivery_floor_is_now_minus_24h() {
        let now = 1_700_000_000u64;
        let floor = redelivery_floor_rfc3339(now);
        let us = parse_rfc3339_micros(&floor).expect("floor parses");
        assert_eq!(us, (now - 24 * 3600) as i64 * 1_000_000, "{floor}");
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

    /// The real `toron mail inbox --json` row shape (not the fixture's
    /// reduced `{"importance":"urgent"}`): a high-importance tasked row
    /// must still ring an idle/done pin. bd-wave17 live probe.
    #[test]
    fn doorbell_tick_real_inbox_row_rings_done_pin() {
        let mut model = model_with("SwiftBay");
        model.statuses.insert("SwiftBay".into(), "done".into());
        let row = serde_json::json!({
            "ack_required": false,
            "audience": null,
            "body": "wake test",
            "body_md": "wake test",
            "corr": null,
            "created_ts_us": 1_793_982_765_215_302_i64,
            "duplicate_of": null,
            "id": "936914ddb50aed6c0bf5da95afc4f8b73f61aaf2c88b61c05a9ccab29aa456b0",
            "importance": "high",
            "priority": null,
            "read_ts_us": null,
            "result_path": null,
            "sender": "AmberHarbor",
            "subject": "[wake-test] live",
            "thread_id": null,
            "verdict": null,
            "wake": "tasked"
        });
        let (_, actions) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows: vec![row],
                pin: "SwiftBay".into(),
            },
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
            "a real high-importance inbox row must ring a done pin, got {actions:?}"
        );
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
            DoorbellAction::Prompt { pin, line, .. } => {
                assert_eq!(pin, "BlueRidge");
                assert!(line.contains("[wake]"));
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
        assert!(next.queues.is_empty(), "idle pin must not queue");
    }

    /// bd-0uzrq.2: the idle edge rings by ITSELF - exactly once per drained
    /// window, coalescing several queued classes - and a second idle with no
    /// new mail rings nothing at all (it re-reads the inbox instead, and the
    /// already-rung ids are refused by the fold).
    #[test]
    fn an_idle_edge_rings_exactly_once_and_a_second_idle_rings_nothing() {
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "working".into());
        let w1 = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#;
        let w2 = r#"{"kind":9,"id":"cd","tags":[["p","BlueRidge"],["h","ch-2"]],"content":"@BlueRidge yo"}"#;
        let (m1, a1) = tick(&model, &DoorbellMsg::WatchNdjson { line: w1.into() });
        let (m2, a2) = tick(&m1, &DoorbellMsg::WatchNdjson { line: w2.into() });
        for actions in [&a1, &a2] {
            assert!(
                !actions
                    .iter()
                    .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
                "nothing rings while the pane is working: {actions:?}"
            );
        }
        let (m3, drain) = tick(
            &m2,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert_eq!(
            drain
                .iter()
                .filter(|a| matches!(a, DoorbellAction::Prompt { .. }))
                .count(),
            1,
            "two queued classes ring ONCE on the idle edge: {drain:?}"
        );
        let (_, again) = tick(
            &m3,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert_eq!(
            again
                .iter()
                .filter(|a| matches!(a, DoorbellAction::Prompt { .. }))
                .count(),
            0,
            "a second idle with no new mail rings nothing: {again:?}"
        );
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
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert_eq!(
            actions2
                .iter()
                .filter(|a| matches!(a, DoorbellAction::Prompt { .. }))
                .count(),
            1,
            "exactly one ring on the drain: {actions2:?}"
        );
        assert!(
            actions2
                .iter()
                .any(|a| matches!(a, DoorbellAction::Audit { kind, .. } if kind == "drained")),
            "the drain is audited: {actions2:?}"
        );
        assert!(actions2.iter().any(|a| a.to_label().contains("BlueRidge")));
        assert!(
            next2
                .queues
                .get("BlueRidge")
                .is_none_or(std::vec::Vec::is_empty)
        );
    }

    /// bd-0uzrq.23: a drained deferred wake already rang the mail, so the inbox
    /// fetch that returns the SAME message must not prompt again — the drain's
    /// key has to be in the rung set the inbox path consults.
    #[test]
    fn drained_deferred_wake_does_not_ring_again_from_the_inbox_row() {
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "working".into());
        let line = r#"{"kind":9,"id":"ev1","tags":[["p","BlueRidge"],["toron-kind","mail-delivered"],["mid","msg-1"],["h","t-1"]],"content":"sealed"}"#;
        let (queued, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "queued"),
            "{actions:?}"
        );
        let (rung, drain) = tick(
            &queued,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert!(
            drain
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
            "the drain rings: {drain:?}"
        );
        let row = serde_json::json!({
            "id": "msg-1",
            "thread_id": "t-1",
            "from": "SlateHarbor",
            "subject": "hi",
            "tags": [["toron-kind", "mail-delivered"]],
        });
        let (_, again) = tick(
            &rung,
            &DoorbellMsg::InboxRows {
                rows: vec![row],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            !again
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
            "the consumed mail must not ring again: {again:?}"
        );
    }

    /// bd-0uzrq.23: a replayed pre-mid wake carries no message id, so its only
    /// key would be the event id — a value the inbox row can never match. It
    /// must not queue a ring the fetch cannot dedup: the fetch (which has real
    /// ids) delivers that mail exactly once.
    #[test]
    fn an_unidentified_mail_wake_is_left_to_the_inbox_fetch() {
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "idle".into());
        let line = r#"{"kind":9,"id":"ev-old","tags":[["p","BlueRidge"],["toron-kind","mail-delivered"],["h","t-1"]],"content":"sealed"}"#;
        let (queued, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
            "an unidentified mail wake must not ring: {actions:?}"
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, DoorbellAction::Audit { kind, .. } if kind == "unidentified_mail_wake")),
            "the skip is audited, never silent: {actions:?}"
        );
        assert!(
            queued
                .queues
                .get("BlueRidge")
                .is_none_or(std::vec::Vec::is_empty),
            "an unidentified mail wake must not sit in the queue: {actions:?}"
        );
        let row = serde_json::json!({
            "id": "msg-1",
            "thread_id": "t-1",
            "from": "SlateHarbor",
            "subject": "hi",
        });
        let (after, first) = tick(
            &queued,
            &DoorbellMsg::InboxRows {
                rows: vec![row.clone()],
                pin: "BlueRidge".into(),
            },
        );
        assert_eq!(
            first
                .iter()
                .filter(|a| matches!(a, DoorbellAction::Prompt { .. }))
                .count(),
            1,
            "the fetch rings the mail once: {first:?}"
        );
        let (_, again) = tick(
            &after,
            &DoorbellMsg::InboxRows {
                rows: vec![row],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            !again
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
            "the same fetch must not ring twice: {again:?}"
        );
    }

    #[test]
    fn queue_audit_journals_every_merge_not_just_the_first() {
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "working".into());
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge hi"}"#;
        let msg = DoorbellMsg::WatchNdjson { line: line.into() };
        let (next, first) = tick(&model, &msg);
        assert_eq!(first.len(), 1);
        assert!(
            matches!(&first[0], DoorbellAction::Audit { kind, .. } if kind == "queued"),
            "the first queue is journaled: {first:?}"
        );
        // The SAME message redelivered is a DEDUP: one directive must ring once,
        // however many producers or redeliveries carry it (bd-x3pqw).
        let (next2, second) = tick(&next, &msg);
        assert!(
            matches!(&second[0], DoorbellAction::Audit { kind, .. } if kind == "dedup"),
            "a redelivery of one message is a dedup, journaled: {second:?}"
        );
        assert_eq!(
            next2.queues.get("BlueRidge").map(Vec::len),
            Some(1),
            "one entry"
        );
        assert_eq!(
            next2.queues["BlueRidge"][0].count, 1,
            "a dedup never inflates the count the ring reports"
        );

        // A DIFFERENT message of the same class MERGES: the count rises and the
        // merge is journaled, so the ring can say how many it carries.
        let other = r#"{"kind":9,"id":"cd","tags":[["p","BlueRidge"],["h","ch-1"]],"content":"@BlueRidge again"}"#;
        let (next3, third) = tick(&next2, &DoorbellMsg::WatchNdjson { line: other.into() });
        assert!(
            matches!(&third[0], DoorbellAction::Audit { kind, .. } if kind == "merged"),
            "a second message of the class merges visibly: {third:?}"
        );
        assert_eq!(
            next3.queues["BlueRidge"][0].count, 2,
            "two distinct messages, one ring entry"
        );
    }

    /// bd-9mjla: a busy refusal is a deferral, not a lost wake. The wake
    /// keeps its per-pin key, N refusals coalesce into the one queued entry,
    /// and the next at-prompt status rings exactly once; a later status
    /// emits nothing (terminal).
    #[test]
    fn prompt_refused_defers_then_rings_once_at_prompt() {
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "idle".into());
        let refused = DoorbellMsg::PromptRefused {
            pin: "BlueRidge".into(),
            line: "wake A".into(),
            event_id: None,
        };
        let (next, actions) = tick(&model, &refused);
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "deferred"),
            "the deferral is journal-visible: {actions:?}"
        );
        assert_eq!(next.queues.get("BlueRidge").map(Vec::len), Some(1));

        let (next2, actions2) = tick(
            &next,
            &DoorbellMsg::PromptRefused {
                pin: "BlueRidge".into(),
                line: "wake B".into(),
                event_id: None,
            },
        );
        assert!(
            matches!(&actions2[0], DoorbellAction::Audit { kind, .. } if kind == "merged"),
            "the second refusal is a visible merge: {actions2:?}"
        );
        assert_eq!(
            next2.queues.get("BlueRidge").map(Vec::len),
            Some(1),
            "two deferred mails of one class share one ring"
        );
        assert_eq!(
            next2.queues["BlueRidge"][0].count, 2,
            "and the ring can name how many it carries"
        );

        let (next3, actions3) = tick(
            &next2,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        let rings: Vec<&DoorbellAction> = actions3
            .iter()
            .filter(|a| matches!(a, DoorbellAction::Prompt { .. }))
            .collect();
        assert_eq!(rings.len(), 1, "exactly one ring: {actions3:?}");
        match rings[0] {
            DoorbellAction::Prompt { pin, line, .. } => {
                assert_eq!(pin, "BlueRidge");
                assert_eq!(
                    line, "wake B. Read it with: toron next --project <project> --as <pin>",
                    "one composed ring, and the NEWEST line of the class wins: a \
                     correction replaces the brief it corrects (bd-0uzrq.1)"
                );
            }
            other => panic!("expected the deferred Prompt, got {other:?}"),
        }
        assert!(
            actions3
                .iter()
                .any(|a| matches!(a, DoorbellAction::Audit { kind, .. } if kind == "drained")),
            "the deferred drain is audited: {actions3:?}"
        );
        assert!(
            next3
                .queues
                .get("BlueRidge")
                .is_none_or(std::vec::Vec::is_empty),
            "the deferral is spent"
        );

        let (_, actions4) = tick(
            &next3,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert!(actions4.is_empty(), "terminal state emits nothing");
    }

    /// bd-0uzrq.8, the live incident: a captain brief and a peer request land
    /// in ONE fetch for a working pane. The peer wake must not hide the brief —
    /// the queue keeps both classes and the ring names both.
    #[test]
    fn a_peer_wake_never_hides_a_captain_brief() {
        let model = working_model("BlueRidge");
        let rows = vec![
            serde_json::json!({
                "subject": "p2p:classify src/elm/inbox.rs",
                "sender": "PurpleGorge",
                "importance": "normal",
            }),
            serde_json::json!({
                "subject": "crew prompt for StormyDesert",
                "sender": "SlateHarbor",
                "importance": "normal",
            }),
        ];
        let (next, actions) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows,
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
            "a working pane queues instead of typing"
        );
        assert_eq!(
            next.queues.get("BlueRidge").map(Vec::len),
            Some(2),
            "one entry per class: neither wake is swallowed: {:?}",
            next.queues.get("BlueRidge")
        );

        let (_, drained) = tick(
            &next,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        let ring = drained
            .iter()
            .find_map(|a| match a {
                DoorbellAction::Prompt { line, .. } => Some(line.clone()),
                _ => None,
            })
            .expect("the handoff rings once");
        assert!(
            ring.starts_with("[wake] Peer message from") && ring.contains("new mail"),
            "the captain brief leads, never buried under the peer line: {ring}"
        );
        assert!(
            ring.contains("1 peer request"),
            "and the peer wake is named, not dropped: {ring}"
        );
        assert_eq!(
            drained
                .iter()
                .filter(|a| matches!(a, DoorbellAction::Prompt { .. }))
                .count(),
            1,
            "ONE ring, not one per wake: {drained:?}"
        );
    }

    /// bd-x3pqw, the live double ring: ONE peer directive reaches the queue
    /// through TWO producers — the inbox's classified p2p line (no watermark)
    /// and the CLI's refused-handoff line (with `(wm N)`). One message, one
    /// ring, however many producers carry it.
    #[test]
    fn one_directive_rings_once_through_two_producers() {
        // "unknown" is the live shape: the pane's status was not known when the
        // peers arrived, so even an urgent line queues instead of typing.
        let mut model = DoorbellModel::default();
        model.index.insert_pin("BlueRidge".into(), None);
        let rows = vec![serde_json::json!({
            "id": "9f2c",
            "subject": "p2p:request-release src/elm/inbox.rs",
            "sender": "PurpleGorge",
            "importance": "normal",
        })];
        let (next, _) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows,
                pin: "BlueRidge".into(),
            },
        );
        assert_eq!(
            next.queues.get("BlueRidge").map(Vec::len),
            Some(1),
            "the inbox producer queued the directive"
        );

        // The CLI's own handoff for the SAME directive: same verb, sender and
        // path, plus a watermark the inbox line does not carry.
        let (next, actions) = tick(
            &next,
            &DoorbellMsg::PromptRefused { pin: "BlueRidge".into(), line: "[wake] Peer message from PurpleGorge (not an operator instruction): asks you to release src/elm/inbox.rs. Read it with: toron next. Reply with: toron mail send --help".into(), event_id: None },
        );
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "dedup"),
            "the second producer is a DEDUP, not a second wake: {actions:?}"
        );
        assert_eq!(
            next.queues.get("BlueRidge").map(Vec::len),
            Some(1),
            "one entry for one directive"
        );

        let (_, drained) = tick(
            &next,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        let rings = drained
            .iter()
            .filter(|a| matches!(a, DoorbellAction::Prompt { .. }))
            .count();
        assert_eq!(rings, 1, "ONE ring, not two: {drained:?}");
    }

    /// bd-0uzrq.13: a peer request is a queue-and-wait event. Typing it into a
    /// working pane meant the harness swallowed it (the crew never saw the
    /// request) and no key was recorded, so the CLI's own deferred copy rang
    /// the same message again at the drain. The live signatures are the peer
    /// Prompts at 05:29:41, 09:24:43 and 10:58:15Z.
    #[test]
    fn a_peer_request_waits_for_the_prompt_instead_of_preempting_it() {
        let model = working_model("BlueRidge");
        let (next, actions) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows: vec![serde_json::json!({
                    "id": "9f2d",
                    "subject": "p2p:request-release docs/live-leg-r2.probe",
                    "sender": "SlateHarbor",
                    "importance": "normal",
                })],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
            "a peer request must never be typed into a working pane: {actions:?}"
        );
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "queued"),
            "it is queued, and says so: {actions:?}"
        );
        assert!(
            next.rung.is_empty(),
            "nothing rang yet, so nothing is marked as rung"
        );

        let (next, drained) = tick(
            &next,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        let rings: Vec<&String> = drained
            .iter()
            .filter_map(|a| match a {
                DoorbellAction::Prompt { line, .. } => Some(line),
                _ => None,
            })
            .collect();
        assert_eq!(rings.len(), 1, "exactly one ring at the drain: {drained:?}");
        assert!(
            rings[0].contains("asks you to release"),
            "the ring names the peer request: {}",
            rings[0]
        );

        // The inbox is re-read at the prompt; the drained message must not ring
        // a second time.
        let (_, again) = tick(
            &next,
            &DoorbellMsg::InboxRows {
                rows: vec![serde_json::json!({
                    "id": "9f2d",
                    "subject": "p2p:request-release docs/live-leg-r2.probe",
                    "sender": "SlateHarbor",
                    "importance": "normal",
                })],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&again[0], DoorbellAction::Audit { kind, .. } if kind == "already_rung"),
            "the re-read is refused, not re-rung: {again:?}"
        );
    }

    /// bd-0uzrq.13: an explicit act-now MAY preempt a working pane, and a
    /// preempt records its key — otherwise the panic-line and the inbox would
    /// each ring the same message.
    #[test]
    fn an_act_now_preempt_records_its_key_so_the_reread_is_silent() {
        let model = working_model("BlueRidge");
        let (next, actions) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows: vec![serde_json::json!({
                    "id": "9f2e",
                    "subject": "URGENT steer",
                    "sender": "SlateHarbor",
                    "importance": "urgent",
                })],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&actions[0], DoorbellAction::Prompt { line, .. } if line.to_ascii_lowercase().contains("urgent mail")),
            "act-now preempts the working pane: {actions:?}"
        );
        assert!(
            next.rung.contains("9f2e"),
            "the preempt is recorded as rung: {:?}",
            next.rung
        );

        let (_, again) = tick(
            &next,
            &DoorbellMsg::InboxRows {
                rows: vec![serde_json::json!({
                    "id": "9f2e",
                    "subject": "URGENT steer",
                    "sender": "SlateHarbor",
                    "importance": "urgent",
                })],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&again[0], DoorbellAction::Audit { kind, .. } if kind == "already_rung"),
            "the same message does not ring twice: {again:?}"
        );
    }

    /// bd-0uzrq.1: a NEWER message of a queued thread replaces the older one —
    /// a correction rings instead of ringing beside the brief it corrects. No
    /// live rows carry `thread_id` today, so this test is the only traffic the
    /// path has.
    #[test]
    fn a_thread_correction_supersedes_the_queued_brief() {
        let model = working_model("BlueRidge");
        let row = |id: &str, subject: &str| {
            serde_json::json!({
                "id": id,
                "subject": subject,
                "sender": "SlateHarbor",
                "importance": "normal",
                "thread_id": "thread-7",
            })
        };
        let (next, _) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows: vec![row("m1", "crew prompt")],
                pin: "BlueRidge".into(),
            },
        );
        let (next, actions) = tick(
            &next,
            &DoorbellMsg::InboxRows {
                rows: vec![row("m2", "crew prompt (correction)")],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "superseded"),
            "the correction supersedes, journalled: {actions:?}"
        );
        assert_eq!(
            next.queues.get("BlueRidge").map(Vec::len),
            Some(1),
            "one wake for the thread, not two"
        );
        assert_eq!(
            next.queues["BlueRidge"][0].count, 1,
            "a supersede is not a merge: the thread holds one live message"
        );

        // The RING text for mail is generic, so identity is what proves the
        // supersede: the queued entry now belongs to the newer message.
        assert_eq!(
            next.queues["BlueRidge"][0].key, "m2",
            "the thread entry carries the newest message's identity"
        );
        let (_, drained) = tick(
            &next,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert!(
            drained
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
            "the thread rings once: {drained:?}"
        );
    }

    /// bd-0uzrq.11: `thread_id` is null on every real row, so the supersede
    /// key falls back to `corr` — which crew mail carries (a `crew prompt
    /// --bead bd-x` rides corr=bd-x). Two briefs about one bead are ONE wake;
    /// two about different beads merge.
    #[test]
    fn a_corr_supersedes_when_the_row_has_no_thread() {
        let model = working_model("BlueRidge");
        let row = |id: &str, corr: &str| {
            serde_json::json!({
                "id": id,
                "subject": "crew prompt",
                "sender": "SlateHarbor",
                "importance": "normal",
                "thread_id": serde_json::Value::Null,
                "corr": corr,
            })
        };
        let (next, _) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows: vec![row("m1", "bd-alpha")],
                pin: "BlueRidge".into(),
            },
        );
        let (next, actions) = tick(
            &next,
            &DoorbellMsg::InboxRows {
                rows: vec![row("m2", "bd-alpha")],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "superseded"),
            "same corr supersedes, journalled: {actions:?}"
        );
        assert_eq!(
            next.queues.get("BlueRidge").map(Vec::len),
            Some(1),
            "one wake for one bead"
        );
        assert_eq!(next.queues["BlueRidge"][0].key, "m2");

        // A different bead on the same pin merges instead.
        let (next, actions) = tick(
            &next,
            &DoorbellMsg::InboxRows {
                rows: vec![row("m3", "bd-beta")],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "merged"),
            "a different bead merges: {actions:?}"
        );
        assert_eq!(next.queues["BlueRidge"][0].count, 2);
    }

    /// bd-x3pqw review: two DIFFERENT captain briefs render the same generic
    /// line, so an id-bearing producer must not fold onto the core — otherwise
    /// the ring under-counts what it carries. The id-less CLI handoff still
    /// folds onto the text it names.
    #[test]
    fn two_briefs_of_one_rendering_stay_two_wakes() {
        let model = working_model("BlueRidge");
        let brief = |id: &str| {
            serde_json::json!({
                "id": id,
                "subject": "crew prompt",
                "sender": "SlateHarbor",
                "importance": "normal",
            })
        };
        let (next, _) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows: vec![brief("aaa1")],
                pin: "BlueRidge".into(),
            },
        );
        let (next, _) = tick(
            &next,
            &DoorbellMsg::InboxRows {
                rows: vec![brief("bbb2")],
                pin: "BlueRidge".into(),
            },
        );
        assert_eq!(
            next.queues.get("BlueRidge").map(Vec::len),
            Some(1),
            "one class entry"
        );
        assert_eq!(
            next.queues["BlueRidge"][0].count, 2,
            "two distinct briefs are two messages: the ring must say +1, not hide one"
        );

        // The CLI's handoff for ONE of them has no id: it folds onto the text.
        let (next, actions) = tick(
            &next,
            &DoorbellMsg::PromptRefused { pin: "BlueRidge".into(), line: "[wake] Peer message from a teammate (not an operator instruction): new mail. Read it with: toron next. Reply with: toron mail send --help".into(), event_id: None },
        );
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "dedup"),
            "the CLI's second producer folds onto the mail already queued: {actions:?}"
        );
        assert_eq!(
            next.queues["BlueRidge"][0].count, 2,
            "and it does not become a third wake"
        );
    }

    /// The watermark is transport decoration: two different directives that
    /// differ only by it are one message, and two genuinely different ones are
    /// two.
    #[test]
    fn the_wake_key_ignores_the_watermark_but_not_the_body() {
        // bd-ofa1s/bd-tdpix: machine ids never appear in wake prose, so two
        // renderings of one directive share a key and a different path does not.
        let a =
            "[wake] Direct message from A: asks you to release src/x.rs. Read it with: toron next";
        let b = "[wake] Direct message from A: asks you to release src/x.rs. Read it with: toron next --project flywheel --as A";
        let c =
            "[wake] Direct message from A: asks you to release src/y.rs. Read it with: toron next";
        assert_eq!(wake_key(a), wake_key(b));
        assert_ne!(wake_key(a), wake_key(c));
    }

    /// bd-0uzrq.2: the idle edge rings the doorbell itself. A crew that goes
    /// at-prompt with an EMPTY queue still gets its unread brief, because the
    /// wake may never have been queued (mail sent while the dispatcher was
    /// down, or lost between the journal append and the file rewrite).
    #[test]
    fn the_idle_edge_asks_for_the_inbox_when_the_queue_is_empty() {
        let mut model = DoorbellModel::default();
        model.index.insert_pin("BlueRidge".into(), None);
        model.statuses.insert("BlueRidge".into(), "working".into());
        let (next, actions) = tick(
            &model,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert!(
            matches!(actions.as_slice(), [DoorbellAction::FetchInbox]),
            "an idle edge with nothing queued asks for the inbox: {actions:?}"
        );

        // The inbox answers with a brief this dispatcher never queued. The pin
        // is idle, so it rings now.
        let rows = vec![serde_json::json!({
            "id": "c0ffee",
            "subject": "crew prompt",
            "sender": "SlateHarbor",
            "importance": "normal",
        })];
        let (next, actions) = tick(
            &next,
            &DoorbellMsg::InboxRows {
                rows: rows.clone(),
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(actions.as_slice(), [DoorbellAction::Prompt { .. }]),
            "the unread brief rings at the idle edge: {actions:?}"
        );

        // A second idle with no NEW mail gives nothing: the re-read comes back
        // with the same still-unread row, and the fold refuses what it already
        // rung.
        let (next, actions) = tick(
            &next,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert!(
            actions.is_empty(),
            "no transition, no fetch, no ring: {actions:?}"
        );
        let (_, actions) = tick(
            &next,
            &DoorbellMsg::InboxRows {
                rows,
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "already_rung"),
            "a re-read of an already-rung message is journaled, never rung again: {actions:?}"
        );
        assert_eq!(actions.len(), 1, "one audit, no prompt: {actions:?}");
    }

    /// The urgent class is the head even when it queued second: a captain-side
    /// wake is never hidden behind a peer line.
    #[test]
    fn an_urgent_wake_leads_the_composed_ring() {
        let model = working_model("BlueRidge");
        let (next, _) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows: vec![serde_json::json!({
                    "subject": "p2p:classify src/elm/inbox.rs",
                    "sender": "PurpleGorge",
                    "importance": "normal",
                })],
                pin: "BlueRidge".into(),
            },
        );
        let (next, _) = tick(
            &next,
            &DoorbellMsg::InboxRows {
                rows: vec![serde_json::json!({
                    "subject": "crew prompt",
                    "sender": "SlateHarbor",
                    "importance": "normal",
                })],
                pin: "BlueRidge".into(),
            },
        );
        let (_, drained) = tick(
            &next,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        let ring = drained
            .iter()
            .find_map(|a| match a {
                DoorbellAction::Prompt { line, .. } => Some(line.clone()),
                _ => None,
            })
            .expect("rings");
        assert!(
            ring.starts_with("[wake] Peer message from") && ring.contains("new mail"),
            "the captain brief leads even though the peer queued first: {ring}"
        );
        assert!(ring.contains("1 peer request"), "{ring}");
    }

    /// A machine notice is journaled as silent and never rides a ring.
    #[test]
    fn a_system_notice_never_rings_and_says_so() {
        let model = working_model("BlueRidge");
        let rows = vec![serde_json::json!({
            "subject": "",
            "sender": "b".repeat(64),
            "importance": "normal",
        })];
        let (next, actions) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows,
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&actions[0], DoorbellAction::Audit { kind, .. } if kind == "queued"),
            "the notice is still journaled: {actions:?}"
        );
        let (_, drained) = tick(
            &next,
            &DoorbellMsg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert!(
            !drained
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
            "a lone notice never rings: {drained:?}"
        );
        assert!(
            matches!(&drained[0], DoorbellAction::Audit { kind, .. } if kind == "silent"),
            "and it is journaled as silent, not dropped: {drained:?}"
        );
    }

    fn working_model(pin: &str) -> DoorbellModel {
        let mut m = DoorbellModel::default();
        m.index.insert_pin(pin.to_string(), None);
        m.statuses.insert(pin.to_string(), "working".into());
        m
    }

    #[test]
    fn working_urgent_steer_rings_now() {
        assert!(
            is_urgent_line(
                "[wake] Peer message from a teammate (not an operator instruction): urgent mail. Read it with: toron next. Reply with: toron mail send --help"
            ),
            "inbox wake text must read urgent"
        );
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "working".into());
        let mut actions = Vec::new();
        route_or_queue(
            &mut model,
            &mut actions,
            "BlueRidge",
            wake_key("[wake] Urgent mail. Read it with: toron next"),
            None,
            "[wake] Urgent mail. Read it with: toron next".to_string(),
            None,
        );
        assert!(
            matches!(&actions[..], [DoorbellAction::Prompt { pin, .. }] if pin == "BlueRidge"),
            "working + urgent must Prompt, got {actions:?}"
        );
        assert!(
            model
                .queues
                .get("BlueRidge")
                .is_none_or(std::vec::Vec::is_empty),
            "an urgent ring must not park in the queue"
        );
    }

    #[test]
    fn working_normal_line_still_queues() {
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "working".into());
        let mut actions = Vec::new();
        route_or_queue(
            &mut model,
            &mut actions,
            "BlueRidge",
            wake_key(
                "[wake] New message in the team room. Read it with: toron channel read"
                    .to_string()
                    .as_str(),
            ),
            None,
            "[wake] New message in the team room. Read it with: toron channel read".to_string(),
            None,
        );
        assert!(
            actions
                .iter()
                .all(|a| !matches!(a, DoorbellAction::Prompt { .. })),
            "working + normal must not Prompt, got {actions:?}"
        );
        assert_eq!(model.queues.get("BlueRidge").map(Vec::len), Some(1));
    }

    #[test]
    fn blocked_never_types_even_when_urgent() {
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "blocked".into());
        let mut urgent = Vec::new();
        route_or_queue(
            &mut model,
            &mut urgent,
            "BlueRidge",
            wake_key("[wake] Urgent mail. Read it with: toron next"),
            None,
            "[wake] Urgent mail. Read it with: toron next".to_string(),
            None,
        );
        assert!(
            !urgent
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
            "blocked + urgent must never type into the dialog: {urgent:?}"
        );
        let mut routine = Vec::new();
        route_or_queue(
            &mut model,
            &mut routine,
            "BlueRidge",
            wake_key(
                "[wake] New message in the team room. Read it with: toron channel read"
                    .to_string()
                    .as_str(),
            ),
            None,
            "[wake] New message in the team room. Read it with: toron channel read".to_string(),
            None,
        );
        assert!(
            !routine
                .iter()
                .any(|a| matches!(a, DoorbellAction::Prompt { .. })),
            "blocked + routine must never type into the dialog: {routine:?}"
        );
    }

    #[test]
    fn unroutable_pin_wake_goes_to_digest_not_queue() {
        let mut model = model_with("QuietHeron");
        model.statuses.remove("QuietHeron");
        model.unroutable.insert("QuietHeron".into());
        let line = r#"{"kind":9,"id":"ab","tags":[["p","QuietHeron"],["h","ch-1"]],"content":"@QuietHeron hi"}"#;
        let (next, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert_eq!(actions.len(), 1, "one digest-bound wake");
        match &actions[0] {
            DoorbellAction::Digest { pin, line, why } => {
                assert_eq!(pin, "QuietHeron");
                assert!(line.contains("[wake]"), "the wake line itself: {line}");
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
        let index = SelfIndex::from_pins(["BlueRidge"]);
        let line = r#"{"kind":9,"id":"aa","tags":[["p","BlueRidge"],["toron-kind","mail-delivered"],["h","t-1"]],"content":"sealed"}"#;
        let (class, hint) = parse_watch_ndjson(line, &index).expect("mail-delivered must ring");
        assert_eq!(class, EventClass::PlanePMail);
        assert_ne!(class, EventClass::Channel);
        assert!(hint.contains("toron next"), "hint must pull inbox: {hint}");
        let rendered = doorbell_line(class, &hint);
        assert!(
            !is_urgent_line(&rendered),
            "delivery ring must not read urgent: {rendered}"
        );
    }

    #[test]
    fn ack_sla_expired_stays_urgent() {
        let index = SelfIndex::from_pins(["BlueRidge"]);
        let line = r#"{"kind":9,"id":"ab","tags":[["p","BlueRidge"],["toron-kind","ack-sla-expired"]],"content":"sealed"}"#;
        let (class, _) = parse_watch_ndjson(line, &index).expect("ack-sla-expired must ring");
        assert_eq!(class, EventClass::PlanePUrgent);
    }

    #[test]
    fn doorbell_tick_mail_delivered_pulls_inbox_not_channel() {
        let model = model_with("BlueRidge");
        let line = r#"{"kind":9,"id":"aa","tags":[["p","BlueRidge"],["toron-kind","mail-delivered"],["h","t-1"],["mid","msg-1"]],"content":"sealed"}"#;
        let (_, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: line.into() });
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            DoorbellAction::Prompt { line, .. } => {
                assert!(
                    line.contains("toron next"),
                    "mail wake must pull inbox: {line}"
                );
                assert!(
                    !line.contains("channel read"),
                    "mail wake must not channel read: {line}"
                );
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    /// bd-0uzrq.18: a mail-delivered push event rings an idle pin at once; a
    /// later inbox poll of the same message must not ring again.
    #[test]
    fn mail_delivered_watch_rings_once_then_poll_dedups() {
        let model = model_with("BlueRidge");
        let watch = r#"{"kind":9,"id":"wake-1","tags":[["p","BlueRidge"],["toron-kind","mail-delivered"],["h","t-1"],["mid","msg-1"]],"content":"sealed"}"#;
        let (next, actions) = tick(&model, &DoorbellMsg::WatchNdjson { line: watch.into() });
        assert!(
            matches!(&actions[..], [DoorbellAction::Prompt { pin, line, .. }] if pin == "BlueRidge" && line.contains("toron next")),
            "push path must ring the idle pin immediately: {actions:?}"
        );
        assert!(next.rung.contains("msg-1"));

        let row = serde_json::json!({
            "id": "msg-1",
            "thread_id": "t-1",
            "subject": "crew prompt",
            "sender": "SlateHarbor",
            "importance": "normal",
        });
        let (_, again) = tick(
            &next,
            &DoorbellMsg::InboxRows {
                rows: vec![row],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            again
                .iter()
                .all(|a| !matches!(a, DoorbellAction::Prompt { .. })),
            "poll of the same message must not ring again: {again:?}"
        );
        assert!(
            again
                .iter()
                .any(|a| matches!(a, DoorbellAction::Audit { kind, .. } if kind == "already_rung")),
            "poll of the same message must be audibly deduped: {again:?}"
        );
    }

    /// bd-0uzrq.18: the in-process wake bus delivers mail-delivered events with
    /// the recipient in the `p` tag as a hex npub and the message id in `mid`;
    /// an index that knows the pin's npub must resolve the target and ring.
    #[test]
    fn mail_delivered_watch_with_hex_npub_p_tag_rings_idle_pin() {
        let npub = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let mut index = SelfIndex::default();
        index.insert_pin("BlueRidge".into(), Some(npub.into()));
        let mut model = DoorbellModel {
            index,
            ..Default::default()
        };
        model.statuses.insert("BlueRidge".into(), "idle".into());
        let watch = format!(
            r#"{{"kind":9,"id":"wake-hex-1","pubkey":"senderpk","created_at":1700000000,"tags":[["p","{npub}"],["t","flywheel"],["h","thread-1"],["toron-kind","mail-delivered"],["thread","thread-1"],["importance","normal"],["from","SlateHarbor"],["mid","msg-hex-1"]],"content":"[doorbell] mail from SlateHarbor thread thread-1 importance normal"}}"#
        );
        let (class, hint) = parse_watch_ndjson(&watch, &model.index).expect("must classify");
        assert_eq!(class, EventClass::PlanePMail);
        assert!(hint.contains("toron next"));
        assert_eq!(
            watch_targets(&watch, &model.index),
            vec!["BlueRidge".to_string()]
        );
        let (next, actions) = tick(
            &model,
            &DoorbellMsg::WatchNdjson {
                line: watch.clone(),
            },
        );
        assert!(
            matches!(&actions[..], [DoorbellAction::ReadToronCursor { pin }] if pin == "BlueRidge"),
            "a ring candidate asks toron before ringing: {actions:?}"
        );
        let (next, actions) = tick(
            &next,
            &DoorbellMsg::ToronCursor {
                pin: "BlueRidge".into(),
                cursor_us: Some(0),
                read_at_secs: 1_700_000_000,
            },
        );
        assert!(
            matches!(&actions[..], [DoorbellAction::Prompt { pin, line, .. }] if pin == "BlueRidge" && line.contains("toron next")),
            "hex-npub p tag with mid must ring idle pin once toron answers behind it: {actions:?}"
        );
        assert!(next.rung.contains("msg-hex-1"));
    }

    /// bd-0uzrq.18: two mails on one thread are two messages and must both ring.
    #[test]
    fn two_mails_on_one_thread_to_idle_pin_give_two_rings() {
        let model = model_with("BlueRidge");
        let first = serde_json::json!({
            "id": "msg-1",
            "thread_id": "thread-9",
            "subject": "first",
            "sender": "SlateHarbor",
            "importance": "normal",
        });
        let second = serde_json::json!({
            "id": "msg-2",
            "thread_id": "thread-9",
            "subject": "second",
            "sender": "SlateHarbor",
            "importance": "normal",
        });
        let (next, a1) = tick(
            &model,
            &DoorbellMsg::InboxRows {
                rows: vec![first],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&a1[..], [DoorbellAction::Prompt { .. }]),
            "first mail must ring: {a1:?}"
        );
        let (_, a2) = tick(
            &next,
            &DoorbellMsg::InboxRows {
                rows: vec![second],
                pin: "BlueRidge".into(),
            },
        );
        assert!(
            matches!(&a2[..], [DoorbellAction::Prompt { .. }]),
            "second mail on same thread must ring: {a2:?}"
        );
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
        let model = model_with("BlueRidge");
        let raw = r#"{"messages":[{"importance":"urgent"},{"importance":"high"},{"importance":"normal"}]}"#;
        let (_, actions) = tick(
            &model,
            &DoorbellMsg::InboxItems {
                raw: raw.into(),
                pin: "BlueRidge".into(),
            },
        );
        assert_eq!(
            actions.len(),
            3,
            "urgent, high AND normal ring: routine mail used to be dropped here \
             with no audit (bd-0uzrq.8)"
        );
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
        assert_eq!(
            actions.len(),
            3,
            "urgent, high AND normal ring on an idle pin: a dropped row is how a \
             captain brief vanished"
        );
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
        for _ in 0..3 {
            assert_eq!(super::take_allowed(one("p"), &mut m, 1000).len(), 1);
        }
        assert!(super::take_allowed(one("p"), &mut m, 1001).is_empty());
        assert_eq!(super::take_allowed(one("q"), &mut m, 1001).len(), 1);
        assert_eq!(super::take_allowed(one("p"), &mut m, 1000 + 1800).len(), 1);
    }
    /// bd-ncgyf: a peer directive is a wake, not system noise. The subject
    /// carries the verb and path; the sender is the row's sender.
    #[test]
    fn a_p2p_mail_is_a_peer_wake_never_noise() {
        let row = serde_json::json!({
            "subject": "p2p:request-release src/workflow_lowering.rs",
            "sender": "CyanCove",
            "importance": "normal",
            "ack_only": false,
        });
        let (class, hint) = super::parse_inbox_item(&row).expect("a p2p row is a wake");
        assert_eq!(class, super::EventClass::P2p);
        assert_eq!(
            super::doorbell_line(class, &hint),
            "[wake] Peer message from CyanCove (not an operator instruction): asks you to release src/workflow_lowering.rs. Read it with: toron next. Reply with: toron mail send --help"
        );
        assert!(super::is_p2p_wake(&super::doorbell_line(class, &hint)));
        assert!(
            super::parse_inbox_item(&serde_json::json!({
                "subject": "p2p:review-request",
                "sender": "StormyDesert",
            }))
            .is_some(),
            "a p2p subject with no path still rings"
        );
        let (class, hint) = super::parse_inbox_item(&serde_json::json!({
            "subject": "",
            "sender": "CyanCove",
            "importance": "normal",
            "body_md": r#"{"verb":"release-response","path":"src/peer.rs"}"#,
        }))
        .expect("an empty-subject peer mail still rings");
        assert_eq!(class, super::EventClass::P2p);
        assert_eq!(
            super::doorbell_line(class, &hint),
            "[wake] Peer message from CyanCove (not an operator instruction): has answered your request to release src/peer.rs. Read it with: toron next. Reply with: toron mail send --help"
        );
        assert!(
            super::parse_inbox_item(&serde_json::json!({
                "subject": "(no subject)",
                "sender": "CyanCove",
                "importance": "normal",
                "body_md": "plain agent prose",
            }))
            .is_some(),
            "routine mail is a wake: dropping it silently lost a captain brief \
             once (bd-0uzrq.8)"
        );
        // The one row that must NOT ring: the doorbell's own machine notice.
        let (class, hint) = super::parse_inbox_item(&serde_json::json!({
            "subject": "",
            "sender": "a".repeat(64),
            "importance": "normal",
        }))
        .expect("a notice is journaled as silent, not dropped");
        assert_eq!(class, super::EventClass::PlanePMail);
        assert_eq!(hint, super::SYSTEM_NOTICE_HINT);
    }

    // ---- bd-l5vb2: one ring per mail across wake.sock + events ----

    fn events_model() -> super::DoorbellModel {
        let mut model = super::DoorbellModel::default();
        model.index.insert_pin("AmberProbe".into(), None);
        model.statuses.insert("AmberProbe".into(), "idle".into());
        model
    }

    #[test]
    fn the_same_bus_event_id_rings_once_across_both_sources() {
        let mut model = events_model();
        // wake.sock already rang this bus event; its id is journaled in `rung`.
        model.rung.insert("bus-e1".into());
        let line = super::doorbell_line(super::EventClass::PlanePMail, "toron next");
        let (_, actions) = super::tick(
            &model,
            &super::DoorbellMsg::EventsHint {
                event_id: "bus-e1".into(),
                pin: "AmberProbe".into(),
                line,
            },
        );
        assert!(
            actions
                .iter()
                .all(|a| !matches!(a, super::DoorbellAction::Prompt { .. })),
            "a seat reached by both rings must not ring twice: {actions:?}"
        );
    }

    #[test]
    fn an_events_gap_is_one_digest_not_a_prompt() {
        let model = events_model();
        let (_, actions) = super::tick(
            &model,
            &super::DoorbellMsg::EventsGap {
                pin: "AmberProbe".into(),
            },
        );
        assert_eq!(actions.len(), 1, "exactly one digest: {actions:?}");
        assert!(
            matches!(actions[0], super::DoorbellAction::Digest { .. }),
            "{actions:?}"
        );
    }
}

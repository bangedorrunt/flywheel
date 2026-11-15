// governed-by: ADR-0012
//! Doorbell pure core (ADR-0012 Phase 0): ring rules, queue coalesce, inject
//! gate, watch/inbox parsing, and the Elm tick. Moved verbatim from
//! `src/doorbell.rs` (pure fns, no signature changes) and `src/doorbell_tick.rs`
//! (verbatim). No I/O, no `&mut` across the API, no tokio/toron/hc/resonate.
//! The effect layer (`cmd_internal_doorbell`, pid files, supervision) stays in
//! flywheel. Tests assert `same Msg log -> byte-identical Action sequence`.

use std::collections::{BTreeMap, BTreeSet};

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
    matches!(plane_status, "idle" | "done" | "blocked")
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

/// One `[doorbell]` line. Body stays pull.
pub fn doorbell_line(class: EventClass, hint: &str) -> String {
    let kind = match class {
        EventClass::Channel => "kind 9",
        EventClass::Job => "kind 43001",
        EventClass::RequestRelease => "request-release",
        EventClass::ReservationConflict => "kind 30031",
        EventClass::PlanePUrgent => "plane-P urgent",
        EventClass::PlanePMail => "plane-P mail",
        EventClass::P2p => return p2p_line_from_hint(hint),
    };
    format!("[doorbell] {kind}. Pull: {hint}")
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
    /// Act now (plane-P urgent, request-release).
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
    if line.starts_with("⟦p2p") {
        return WakeKind::Peer;
    }
    if line.contains("plane-P urgent") || line.contains("request-release") {
        return WakeKind::ActNow;
    }
    if line.contains(SYSTEM_NOTICE_HINT) {
        return WakeKind::Notice;
    }
    if line.contains("plane-P mail") || line.contains("kind 43001") || line.contains("kind 9") {
        return WakeKind::AtIdle;
    }
    WakeKind::Digest
}

/// The identity a row's wake carries into the queue.
///
/// It is the RENDERED line's core, not the row id, because only one of the two
/// producers has an id: the inbox row does, the CLI's refused-handoff line does
/// not. Keying on the line is what makes one directive one ring (bd-x3pqw).
/// When the envelope carries a message id to both producers (bd-0uzrq.1), that
/// id becomes the key and this fn goes away.
fn row_key(_row: &serde_json::Value, line: &str) -> String {
    wake_key(line)
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
        .split(" — run: ")
        .next()
        .unwrap_or(&by_kind[0].line)
        .trim_end()
        .to_string();
    let rest: Vec<String> = by_kind
        .iter()
        .skip(1)
        .map(|w| format!("{} {}", w.count, w.kind.label()))
        .collect();
    if rest.is_empty() {
        return format!("{head} — run: toron next");
    }
    format!("{head} + {} — run: toron next", rest.join(" + "))
}

/// The separator `parse_inbox_item` uses to carry a peer directive's verb,
/// sender and path through the `(class, hint)` shape.
pub const P2P_HINT_SEP: char = '\u{1f}';

/// The peer line a `p2p:` mail rings: the same shape the CLI types, minus the
/// mail watermark (the doorbell has no cursor for the recipient).
pub fn p2p_line_from_hint(hint: &str) -> String {
    let mut parts = hint.split(P2P_HINT_SEP);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(verb), Some(sender), Some(path)) => {
            format!("⟦p2p:{verb}⟧ from {sender} for {path} — run: toron next")
        }
        _ => "⟦p2p⟧ peer directive — run: toron next".to_string(),
    }
}

/// A peer directive wake line. The fold keeps these (they are a direct
/// directive, not channel chatter).
pub fn is_p2p_wake(line: &str) -> bool {
    line.starts_with("⟦p2p:")
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
            EventClass::P2p => return p2p_line_from_hint(hint),
        };
        format!("[doorbell] {kind} event={event_id}. Pull: {hint}")
    }
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
    /// The message identity this wake belongs to. Two producers exist for one
    /// directive (the inbox's classified mail line and the CLI's handoff line),
    /// so the key is the mail id when the row carries one and the line's
    /// normalised core otherwise (bd-x3pqw).
    #[serde(default)]
    pub key: String,
    pub line: String,
    pub count: u32,
}

impl QueuedWake {
    #[must_use]
    pub fn new(kind: WakeKind, key: impl Into<String>, line: impl Into<String>) -> Self {
        Self {
            kind,
            key: key.into(),
            line: line.into(),
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
}

/// The identity of a ring line when no message id is available: the line with
/// its transport decorations stripped, so the inbox producer
/// (`⟦p2p:request-release⟧ from A for b — run: toron next`) and the CLI
/// producer (`⟦p2p:request-release⟧ from A for b (wm 382) — run: toron next`)
/// resolve to the same wake.
#[must_use]
pub fn wake_key(line: &str) -> String {
    let core = line.split(" — run: ").next().unwrap_or(line).trim_end();
    let core = match core.rfind(" (wm ") {
        Some(at) if core.ends_with(')') => &core[..at],
        _ => core,
    };
    core.trim_end().to_string()
}

/// Fold one line into the queue: same class merges (newest line wins, count
/// bumps), a new class appends. Returns true when it merged.
pub fn queue_line(queue: &mut Vec<QueuedWake>, key: String, line: String) -> Queued {
    let kind = wake_kind_of_line(&line);
    if let Some(w) = queue.iter_mut().find(|w| !key.is_empty() && w.key == key) {
        // The same message through a second producer: replace the text, do not
        // inflate the count, and never ring a third time.
        w.line = line;
        return Queued::Duplicate;
    }
    match queue.iter_mut().find(|w| w.kind == kind) {
        Some(w) => {
            w.line = line;
            w.count = w.count.saturating_add(1);
            Queued::Merged
        }
        None => {
            queue.push(QueuedWake::new(kind, key, line));
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
    PromptRefused { pin: String, line: String },
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
    /// Last-known hc status per pin.
    pub statuses: BTreeMap<String, String>,
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
/// A doorbell line that must ring now, never batch: plane-P urgent,
/// request-release, reservation conflicts, ack-SLA expiry (including the
/// re-ring vehicle `doorbell_mail_line`, which carries "unacked for").
/// Moved verbatim from flywheel `workspace.rs` (bd-ene2): one definition —
/// the daemon FileHost presence filter and the embedded path share it.
/// A mail wake: the router's plane-P mail class or the mail-wake host's line.
/// These always reach a pane — a done mail that never rings is a lost
/// hand-off (bd-346jl); the afk batch only applies to routine channel traffic.
pub fn is_mail_wake(line: &str) -> bool {
    line.contains("plane-P mail") || line.starts_with("Doorbell: mail from")
}

pub fn is_urgent_line(line: &str) -> bool {
    line.contains("plane-P urgent")
        || line.contains("URGENT steer")
        || line.contains("request-release")
        || line.contains("kind 30031")
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
        DoorbellMsg::WatchNdjson { line } => {
            let Some((class, hint)) = parse_watch_ndjson(line, &next.index) else {
                return (next, actions);
            };
            // A watch line renders with its EVENT id, so two distinct events of
            // one class are two messages (they merge) and a redelivery of one is
            // the same message (it dedups). The inbox/CLI producers have no id
            // in their text, so their identity is the line itself.
            let event_id = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| {
                    v.get("id")
                        .and_then(serde_json::Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let line_out = doorbell_line(class, &hint);
            // Watch events carry an id and the inbox/CLI producers do not, so a
            // watch key is the line core PLUS the event: two events of a class
            // merge, a redelivery of one dedups.
            let key = if event_id.is_empty() {
                wake_key(&line_out)
            } else {
                format!("{}#{event_id}", wake_key(&line_out))
            };
            for pin in watch_targets(line, &next.index) {
                route_or_queue(&mut next, &mut actions, &pin, key.clone(), line_out.clone());
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
                let key = row_key(item, &line_out);
                route_or_queue(&mut next, &mut actions, pin, key, line_out);
            }
        }
        DoorbellMsg::InboxRows { rows, pin } => {
            for item in rows {
                let Some((class, hint)) = parse_inbox_item(item) else {
                    continue;
                };
                let line_out = doorbell_line(class, &hint);
                let key = row_key(item, &line_out);
                route_or_queue(&mut next, &mut actions, pin, key, line_out);
            }
        }
        DoorbellMsg::PlaneStatus { pin, status } => {
            let from = next
                .statuses
                .get(pin)
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            if is_drain_transition(&from, status)
                && let Some(q) = next.queues.get_mut(pin)
            {
                let deferred = std::mem::take(q);
                let total: u32 = deferred.iter().map(|w| w.count).sum();
                if deferred.is_empty() {
                    // nothing queued: fall through to the status update
                } else if deferred.iter().all(|w| w.kind == WakeKind::Notice) {
                    // Machine notices never ring, even alone.
                    actions.push(DoorbellAction::Audit {
                        kind: "silent".into(),
                        pin: pin.clone(),
                        detail: format!("{total} system notice(s) dropped at prompt"),
                    });
                } else {
                    let ring = composed_ring(&deferred);
                    actions.push(DoorbellAction::Audit {
                        kind: "drained".into(),
                        pin: pin.clone(),
                        detail: format!("{total} deferred wake(s) at prompt: {ring}"),
                    });
                    actions.push(DoorbellAction::Prompt {
                        pin: pin.clone(),
                        line: ring,
                    });
                }
            }
            next.statuses.insert(pin.clone(), status.clone());
        }
        DoorbellMsg::PromptRefused { pin, line } => {
            next.statuses.insert(pin.clone(), "working".into());
            let queue = next.queues.entry(pin.clone()).or_default();
            let fold = queue_line(queue, wake_key(line), line.clone());
            actions.push(DoorbellAction::Audit {
                kind: match fold {
                    Queued::Fresh => "deferred",
                    Queued::Merged => "merged",
                    Queued::Duplicate => "dedup",
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
        DoorbellMsg::WatchDied => {
            actions.push(DoorbellAction::RestartWatch);
        }
    }
    (next, actions)
}

/// Idle/done panes get a Prompt now; blocked+urgent prompts while
/// blocked+routine digests with a reason (bd-zawk G6); working panes queue
/// (drained on the next working -> idle|done|blocked PlaneStatus).
fn route_or_queue(
    next: &mut DoorbellModel,
    actions: &mut Vec<DoorbellAction>,
    pin: &str,
    key: String,
    line: String,
) {
    let status = next
        .statuses
        .get(pin)
        .map(String::as_str)
        .unwrap_or("unknown");
    // governed-by: bd-zawk — per-wake verdict via the typed InjectGate so the
    if should_inject(status) {
        match inject_gate_for_plane(status, is_urgent_line(&line)) {
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
        actions.push(DoorbellAction::Digest {
            pin: pin.to_string(),
            line,
            why: "no_route".into(),
        });
    } else {
        if matches!(
            inject_gate_for_plane(status, is_urgent_line(&line)),
            InjectGate::Inject
        ) {
            actions.push(DoorbellAction::Prompt {
                pin: pin.to_string(),
                line,
            });
            return;
        }
        let queue = next.queues.entry(pin.to_string()).or_default();
        // bd-0uzrq.8/.x3pqw: one identity per message. The same message from a
        // second producer is a DEDUP (one ring); a different message of a
        // class MERGES visibly. Silently coalescing is how a captain brief
        // vanished behind a peer wake.
        let fold = queue_line(queue, key, line.clone());
        actions.push(DoorbellAction::Audit {
            kind: match fold {
                Queued::Fresh => "queued",
                Queued::Merged => "merged",
                Queued::Duplicate => "dedup",
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
        assert_eq!(inject_gate_for_plane("blocked", true), InjectGate::Inject);
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
            DoorbellAction::Prompt { pin, line } => {
                assert_eq!(pin, "BlueRidge");
                assert_eq!(
                    line, "wake B — run: toron next",
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
            ring.starts_with("[doorbell] plane-P mail"),
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
            &DoorbellMsg::PromptRefused {
                pin: "BlueRidge".into(),
                line: "⟦p2p:request-release⟧ from PurpleGorge for src/elm/inbox.rs (wm 382) — run: toron next".into(),
            },
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

    /// The watermark is transport decoration: two different directives that
    /// differ only by it are one message, and two genuinely different ones are
    /// two.
    #[test]
    fn the_wake_key_ignores_the_watermark_but_not_the_body() {
        let a = "⟦p2p:request-release⟧ from A for src/x.rs — run: toron next";
        let b = "⟦p2p:request-release⟧ from A for src/x.rs (wm 382) — run: toron next";
        let c = "⟦p2p:request-release⟧ from A for src/y.rs — run: toron next";
        assert_eq!(wake_key(a), wake_key(b));
        assert_ne!(wake_key(a), wake_key(c));
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
            ring.starts_with("[doorbell] plane-P mail"),
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
            is_urgent_line("⟦toron⟧ URGENT steer (wm 7) — run: toron next"),
            "inbox wake text must read urgent"
        );
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "working".into());
        let mut actions = Vec::new();
        route_or_queue(
            &mut model,
            &mut actions,
            "BlueRidge",
            wake_key("⟦toron⟧ URGENT steer (wm 7) — run: toron next"),
            "⟦toron⟧ URGENT steer (wm 7) — run: toron next".to_string(),
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
                "[doorbell] kind 9. Pull: toron channel read"
                    .to_string()
                    .as_str(),
            ),
            "[doorbell] kind 9. Pull: toron channel read".to_string(),
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
    fn blocked_routing_unchanged() {
        let mut model = model_with("BlueRidge");
        model.statuses.insert("BlueRidge".into(), "blocked".into());
        let mut urgent = Vec::new();
        route_or_queue(
            &mut model,
            &mut urgent,
            "BlueRidge",
            wake_key("⟦toron⟧ URGENT steer (wm 7) — run: toron next"),
            "⟦toron⟧ URGENT steer (wm 7) — run: toron next".to_string(),
        );
        assert!(
            matches!(&urgent[..], [DoorbellAction::Prompt { .. }]),
            "blocked + urgent must Prompt, got {urgent:?}"
        );
        let mut routine = Vec::new();
        route_or_queue(
            &mut model,
            &mut routine,
            "BlueRidge",
            wake_key(
                "[doorbell] kind 9. Pull: toron channel read"
                    .to_string()
                    .as_str(),
            ),
            "[doorbell] kind 9. Pull: toron channel read".to_string(),
        );
        assert!(
            matches!(&routine[..], [DoorbellAction::Digest { .. }]),
            "blocked + routine must Digest, got {routine:?}"
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
        let line = r#"{"kind":9,"id":"aa","tags":[["p","BlueRidge"],["toron-kind","mail-delivered"],["h","t-1"]],"content":"sealed"}"#;
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
            "⟦p2p:request-release⟧ from CyanCove for src/workflow_lowering.rs — run: toron next"
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
            "⟦p2p:release-response⟧ from CyanCove for src/peer.rs — run: toron next"
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
}

//! SEP-3415 events-client core for harness bridges (ADR-0039 D8, bd-si4b5).
//!
//! ONE reusable client is shared by every bridge (Claude channel now; pi/omp
//! later; opencode uses its native client). It polls/streams a toron events
//! endpoint through a pluggable transport, persists a cursor per subscription,
//! dedupes by `eventId`, and handles gap/truncated + heartbeat liveness +
//! jittered reconnect. The mapping to a harness's inbound door is pure: it
//! emits only the D2 hint ("mail pending, run `toron next`"), never the body.
//!
//! Effect law: every decision is a pure function; the transport and the sink
//! are ports the caller implements. Unit tests use a fake server.

use std::collections::{HashSet, VecDeque};

/// The D2 hint. Only this text crosses into the harness — never the body.
pub const HINT: &str = "mail pending, run toron next";

/// Minimum event shape a bridge needs. `payload` is intentionally opaque: the
/// mapper must not read or forward it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeEvent {
    pub id: String,
    pub kind: String,
    pub payload: String,
}

/// What a bridge sends into the harness inbound door. No body field exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelNotice {
    pub hint: String,
    /// The bus event id (or a synthetic `gap:<cursor>`), so the doorbell/bridge
    /// pair can dedupe a seat reached by both (ADR-0039 D8).
    pub event_id: String,
    /// The toron catalogue event name, used as `meta.event_name`.
    pub event_name: String,
}

/// Toron catalogue names that wake an agent (ADR-0039 D8) — not the invented
/// `mail|mention|...` set. `heartbeat`/`active`/`terminated` are notifications.
pub fn is_wake_event(event: &BridgeEvent) -> bool {
    matches!(
        event.kind.as_str(),
        "mail.addressed"
            | "reservation.conflict"
            | "ack_sla_expired"
            | "role.changed"
            | "policy.changed"
            | "gap"
    )
}

/// Pure event -> notice mapping. The hint is fixed; the payload/body is never
/// inspected or forwarded.
pub fn event_to_notice(event: &BridgeEvent) -> Option<ChannelNotice> {
    if !is_wake_event(event) || event.id.is_empty() {
        return None;
    }
    Some(ChannelNotice {
        hint: HINT.to_string(),
        event_id: event.id.clone(),
        event_name: event.kind.clone(),
    })
}

/// One synthetic gap notice: the server reset and handed back a fresh servable
/// cursor, so the agent must re-read authoritative state via `toron next`.
pub fn gap_notice(next_cursor: &str) -> ChannelNotice {
    ChannelNotice {
        hint: HINT.to_string(),
        event_id: format!("gap:{next_cursor}"),
        event_name: "gap".to_string(),
    }
}

/// Bounded eventId dedupe as a FIFO: at cap, evict the OLDEST id, not the whole
/// set. Clearing everything made a reconnect right after a clear ring
/// duplicates.
#[derive(Debug, Default)]
pub struct Deduper {
    order: VecDeque<String>,
    seen: HashSet<String>,
    cap: usize,
}

impl Deduper {
    pub fn new(cap: usize) -> Self {
        Self {
            order: VecDeque::new(),
            seen: HashSet::new(),
            cap: cap.max(1),
        }
    }

    /// True only the first time an id is seen.
    pub fn is_new(&mut self, id: &str) -> bool {
        if self.seen.contains(id) {
            return false;
        }
        if self.order.len() >= self.cap
            && let Some(oldest) = self.order.pop_front()
        {
            self.seen.remove(&oldest);
        }
        self.order.push_back(id.to_string());
        self.seen.insert(id.to_string());
        true
    }
}

/// One poll result. `gap`/`truncated` are NOT errors: SEP-3415 says the server
/// already reset and returned a fresh servable cursor in `next_cursor`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub events: Vec<BridgeEvent>,
    pub next_cursor: String,
    pub gap: bool,
    pub truncated: bool,
}

impl Page {
    pub fn needs_resync(&self) -> bool {
        self.gap || self.truncated
    }
}

/// A command the pure layer emits; the interpreter performs it. Hints are
/// emitted BEFORE the cursor is journaled: for a wake hint that is at-least-once
/// delivery — a crash between them replays from the old cursor and the
/// eventId deduper absorbs the duplicate. (Opposite of an exactly-once ledger.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeCmd {
    Emit(ChannelNotice),
    JournalCursor(String),
}

/// Pure fold of one page into commands + the next cursor. Dedupe is applied
/// here, so the interpreter has no decision logic. A gap/truncated page still
/// delivers its events AND emits one gap hint, then journals the fresh cursor.
pub fn apply_page(deduper: &mut Deduper, cursor: &mut String, page: &Page) -> Vec<BridgeCmd> {
    let mut cmds = Vec::new();
    if page.needs_resync() && deduper.is_new(&format!("gap:{}", page.next_cursor)) {
        cmds.push(BridgeCmd::Emit(gap_notice(&page.next_cursor)));
    }
    for event in &page.events {
        if deduper.is_new(&event.id)
            && let Some(notice) = event_to_notice(event)
        {
            cmds.push(BridgeCmd::Emit(notice));
        }
    }
    if !page.next_cursor.is_empty() {
        *cursor = page.next_cursor.clone();
        cmds.push(BridgeCmd::JournalCursor(page.next_cursor.clone()));
    }
    cmds
}

/// The notices carried by a command list (for callers that only need the
/// harness-visible half).
pub fn notices_of(cmds: &[BridgeCmd]) -> Vec<ChannelNotice> {
    cmds.iter()
        .filter_map(|c| match c {
            BridgeCmd::Emit(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// Jittered reconnect delay in millis (exponential backoff + bounded jitter).
/// Pure: the jitter comes from `rand_unit` in `[0, 1)`, supplied by the caller.
pub fn reconnect_delay_ms(attempt: u32, base_ms: u64, cap_ms: u64, rand_unit: f64) -> u64 {
    let shift = attempt.min(16);
    let backoff = base_ms.saturating_mul(1u64 << shift).min(cap_ms);
    let jitter = (rand_unit.clamp(0.0, 0.9999) * backoff as f64) as u64;
    (backoff / 2 + jitter).min(cap_ms)
}

/// Heartbeat liveness: a stream is dead after 3 missed heartbeat intervals.
pub fn heartbeat_dead(last_hb_ms: u64, now_ms: u64, interval_ms: u64) -> bool {
    now_ms.saturating_sub(last_hb_ms) > interval_ms.saturating_mul(3)
}

/// The transport port. Phase A tests and the stdio seam use a fake; Phase B
/// wires toron's `events/poll` / `events/stream` through the same trait.
pub trait EventsTransport {
    fn poll(&self, subscription: &str, cursor: &str) -> Result<Page, String>;
}

/// One interpreter tick: poll once and fold the page into commands. Pure except
/// for the injected transport; no clock, env, or global state.
pub fn tick<T: EventsTransport>(
    transport: &T,
    subscription: &str,
    deduper: &mut Deduper,
    cursor: &mut String,
) -> Result<Vec<BridgeCmd>, String> {
    let page = transport.poll(subscription, cursor)?;
    Ok(apply_page(deduper, cursor, &page))
}

/// The MCP method a bridge declares at `initialize`.
pub const CHANNEL_CAPABILITY: &str = "claude/channel";

/// Protocol revisions. Claude Code skips a channel negotiated on a modern
/// revision ("negotiated a modern protocol revision with no unsolicited
/// notification path"), so a bridge must answer with a pre-2026-07-28 date.
const MODERN_REVISION: &str = "2026-07-28";
const FALLBACK_REVISION: &str = "2025-11-25";

/// Pure JSON-RPC `initialize` result. Claude reads the capability under
/// `capabilities.experimental["claude/channel"]`; a top-level key is ignored
/// ("server did not declare claude/channel capability"). The version echoes a
/// client below the modern threshold, otherwise pins to a safe pre-threshold
/// date. No tool list, no body surface — only the channel door.
pub fn initialize_result(client_version: Option<&str>) -> serde_json::Value {
    let negotiated = match client_version {
        Some(v) if !v.is_empty() && v < MODERN_REVISION => v.to_string(),
        _ => FALLBACK_REVISION.to_string(),
    };
    serde_json::json!({
        "protocolVersion": negotiated,
        "capabilities": { "experimental": { CHANNEL_CAPABILITY: {} } },
        "serverInfo": { "name": "flywheel-events-bridge", "version": "1" }
    })
}

/// Pure mapping: one channel notice -> a JSON-RPC notification. Claude
/// validates `params` as `{content: string, meta?: Record<string,string>}` —
/// content is the hint STRING (not an array) and meta carries the ids. Never
/// the event body.
pub fn notice_to_rpc(notice: &ChannelNotice) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": notice.hint,
            "meta": {
                "event_id": notice.event_id,
                "event_name": notice.event_name
            }
        }
    })
}

/// The result of one interpreter step the stdio server performs.
pub struct BridgeStep {
    pub notifications: Vec<serde_json::Value>,
    pub reconnect_ms: Option<u64>,
    pub dead: bool,
}

/// Timing inputs for a step, bundled so the interpreter call stays small.
pub struct BridgeTiming {
    pub last_hb_ms: u64,
    pub now_ms: u64,
    pub interval_ms: u64,
    pub attempt: u32,
    pub rand_unit: f64,
}

/// One full interpreter step: liveness + reconnect timing + a poll + the RPC
/// notifications. Pure except for the injected transport. This is what the
/// `mcp-events` loop calls, so every core decision is wired into production.
pub fn bridge_step<T: EventsTransport>(
    transport: &T,
    subscription: &str,
    deduper: &mut Deduper,
    cursor: &mut String,
    timing: &BridgeTiming,
) -> Result<BridgeStep, String> {
    let dead = heartbeat_dead(timing.last_hb_ms, timing.now_ms, timing.interval_ms);
    let reconnect_ms =
        dead.then(|| reconnect_delay_ms(timing.attempt, 500, 30_000, timing.rand_unit));
    let cmds = tick(transport, subscription, deduper, cursor)?;
    let notifications = notices_of(&cmds).iter().map(notice_to_rpc).collect();
    Ok(BridgeStep {
        notifications,
        reconnect_ms,
        dead,
    })
}

/// A no-op transport for Phase A's stdio seam: no events until Phase B (bd-o4bgx)
/// wires the real toron client. Keeps the one core compiled and exercised.
pub struct NoopTransport;
impl EventsTransport for NoopTransport {
    fn poll(&self, _sub: &str, _cursor: &str) -> Result<Page, String> {
        Ok(Page {
            events: vec![],
            next_cursor: String::new(),
            gap: false,
            truncated: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, kind: &str) -> BridgeEvent {
        BridgeEvent {
            id: id.into(),
            kind: kind.into(),
            payload: "SECRET BODY MUST NOT LEAK".into(),
        }
    }

    #[test]
    fn notice_never_carries_the_body() {
        let n = event_to_notice(&ev("e1", "mail.addressed")).expect("wake event maps");
        assert_eq!(n.hint, HINT);
        assert_eq!(n.event_name, "mail.addressed");
        assert!(!format!("{n:?}").contains("SECRET BODY"), "{n:?}");
        assert!(event_to_notice(&ev("e2", "heartbeat")).is_none());
        assert!(
            event_to_notice(&ev("e3", "mail")).is_none(),
            "old kind rejected"
        );
    }

    #[test]
    fn dedupe_suppresses_repeated_event_ids() {
        let mut d = Deduper::new(16);
        assert!(d.is_new("e1"));
        assert!(!d.is_new("e1"));
        assert!(d.is_new("e2"));
    }

    #[test]
    fn dedupe_evicts_oldest_not_the_whole_set() {
        let mut d = Deduper::new(2);
        assert!(d.is_new("a"));
        assert!(d.is_new("b"));
        assert!(d.is_new("c")); // evicts a
        assert!(d.is_new("a")); // a is new again
        assert!(!d.is_new("c")); // b and c still remembered
    }

    #[test]
    fn gap_delivers_events_and_advances_cursor_with_gap_hint() {
        let mut d = Deduper::new(16);
        let mut c = "c0".to_string();
        let page = Page {
            events: vec![ev("e1", "mail.addressed")],
            next_cursor: "c1".into(),
            gap: true,
            truncated: false,
        };
        let cmds = apply_page(&mut d, &mut c, &page);
        assert_eq!(c, "c1", "gap advances to the fresh cursor");
        let notices = notices_of(&cmds);
        assert_eq!(notices.len(), 2);
        assert!(notices.iter().any(|n| n.event_name == "gap"));
        assert!(notices.iter().any(|n| n.event_id == "e1"));
    }

    #[test]
    fn truncated_page_also_advances_and_emits_gap_hint() {
        let mut d = Deduper::new(16);
        let mut c = "c0".to_string();
        let cmds = apply_page(
            &mut d,
            &mut c,
            &Page {
                events: vec![],
                next_cursor: "c2".into(),
                gap: false,
                truncated: true,
            },
        );
        assert_eq!(c, "c2");
        assert_eq!(notices_of(&cmds).len(), 1);
    }

    #[test]
    fn emits_before_journal_cursor() {
        let mut d = Deduper::new(16);
        let mut c = "c0".to_string();
        let cmds = apply_page(
            &mut d,
            &mut c,
            &Page {
                events: vec![ev("e1", "mail.addressed")],
                next_cursor: "c7".into(),
                gap: false,
                truncated: false,
            },
        );
        assert!(matches!(cmds.first(), Some(BridgeCmd::Emit(_))), "{cmds:?}");
        assert!(matches!(cmds.last(), Some(BridgeCmd::JournalCursor(x)) if x == "c7"));
    }

    #[test]
    fn reconnect_backoff_is_jittered_and_bounded() {
        let a = reconnect_delay_ms(0, 100, 5000, 0.0);
        let b = reconnect_delay_ms(0, 100, 5000, 0.9);
        assert!(b > a, "jitter spreads retries: {a} {b}");
        assert_eq!(reconnect_delay_ms(20, 100, 5000, 0.5), 5000);
    }

    #[test]
    fn heartbeat_dead_after_three_intervals() {
        assert!(!heartbeat_dead(1000, 3000, 1000));
        assert!(!heartbeat_dead(1000, 3999, 1000));
        assert!(heartbeat_dead(1000, 4001, 1000));
    }

    struct FakeServer {
        pages: Vec<Page>,
    }
    impl EventsTransport for FakeServer {
        fn poll(&self, _sub: &str, _cursor: &str) -> Result<Page, String> {
            Ok(self.pages.first().cloned().unwrap_or(Page {
                events: vec![],
                next_cursor: String::new(),
                gap: false,
                truncated: false,
            }))
        }
    }

    #[test]
    fn tick_uses_the_transport_and_folds_purely() {
        let server = FakeServer {
            pages: vec![Page {
                events: vec![ev("x1", "mail.addressed")],
                next_cursor: "c9".into(),
                gap: false,
                truncated: false,
            }],
        };
        let mut d = Deduper::new(8);
        let mut c = "c0".into();
        let cmds = tick(&server, "sub-1", &mut d, &mut c).expect("tick");
        assert_eq!(notices_of(&cmds).len(), 1);
        assert_eq!(c, "c9");
    }

    #[test]
    fn bridge_step_wires_liveness_and_notifications() {
        let server = FakeServer {
            pages: vec![Page {
                events: vec![ev("x1", "mail.addressed")],
                next_cursor: "c9".into(),
                gap: false,
                truncated: false,
            }],
        };
        let mut d = Deduper::new(8);
        let mut c = "c0".into();
        let step = bridge_step(
            &server,
            "sub-1",
            &mut d,
            &mut c,
            &BridgeTiming {
                last_hb_ms: 1000,
                now_ms: 5000,
                interval_ms: 1000,
                attempt: 1,
                rand_unit: 0.5,
            },
        )
        .expect("step");
        assert!(step.dead, "4 intervals without heartbeat is dead");
        assert!(
            step.reconnect_ms.is_some(),
            "dead stream schedules reconnect"
        );
        assert_eq!(step.notifications.len(), 1);
    }

    #[test]
    fn initialize_capability_is_under_experimental() {
        let init = initialize_result(None);
        assert!(
            init["capabilities"]["experimental"][CHANNEL_CAPABILITY].is_object(),
            "must declare under capabilities.experimental: {init}"
        );
        assert!(
            init["capabilities"][CHANNEL_CAPABILITY].is_null(),
            "top-level key must be absent: {init}"
        );
    }

    #[test]
    fn initialize_echoes_old_version_and_falls_back_for_modern() {
        let old = initialize_result(Some("2024-11-05"));
        assert_eq!(old["protocolVersion"], "2024-11-05");
        let modern = initialize_result(Some("2026-07-28"));
        assert_eq!(modern["protocolVersion"], FALLBACK_REVISION);
        let none = initialize_result(None);
        assert_eq!(none["protocolVersion"], FALLBACK_REVISION);
    }

    #[test]
    fn notice_rpc_schema_is_string_content_plus_meta() {
        let n = event_to_notice(&ev("e1", "mail.addressed")).unwrap();
        let rpc = notice_to_rpc(&n);
        assert_eq!(rpc["params"]["content"], HINT);
        assert!(
            rpc["params"]["content"].is_string(),
            "content is a string: {rpc}"
        );
        assert_eq!(rpc["params"]["meta"]["event_id"], "e1");
        assert_eq!(rpc["params"]["meta"]["event_name"], "mail.addressed");
        assert!(rpc.get("eventId").is_none(), "no top-level eventId: {rpc}");
        assert!(!rpc.to_string().contains("SECRET BODY"), "{rpc}");
    }
}

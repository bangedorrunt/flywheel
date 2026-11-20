//! SEP-3415 events-client core for harness bridges (ADR-0039 D8, bd-si4b5).
//!
//! ONE reusable client is shared by every bridge (Claude channel now; pi/omp
//! and opencode later). It polls/streams a toron events endpoint through a
//! pluggable transport, persists a cursor per subscription, dedupes by
//! `eventId`, and handles gap/truncated + heartbeat liveness + jittered
//! reconnect. The mapping to a harness's inbound door is pure: it emits only
//! the D2 hint ("mail pending, run `toron next`"), NEVER the event body.
//!
//! Effect law: every decision here is a pure function; the transport and the
//! persistence are ports the caller implements. Unit tests use a fake server.
//!
//! Phase A ships the reusable core and its tests; the live transport wiring
//! lands in Phase B (bd-o4bgx), so these items are exercised by tests and by
//! the Phase B caller, not yet by the Phase A stdio protocol seam.
#![allow(dead_code)]

use std::collections::BTreeSet;

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
    /// The bus event id, so the doorbell/bridge pair can dedupe a seat that is
    /// reached by both (ADR-0039 D8).
    pub event_id: String,
}

/// Events that wake an agent. Others (heartbeat, active, terminated) do not
/// produce a channel notice. This is a pure classifier.
pub fn is_wake_event(event: &BridgeEvent) -> bool {
    matches!(
        event.kind.as_str(),
        "mail" | "mention" | "reservation" | "job" | "wake" | "urgent"
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
    })
}

/// Bounded eventId dedupe. Older ids are forgotten so a long-lived bridge does
/// not grow without limit; the window is far larger than a reconnect gap.
#[derive(Debug, Default)]
pub struct Deduper {
    seen: BTreeSet<String>,
    cap: usize,
}

impl Deduper {
    pub fn new(cap: usize) -> Self {
        Self {
            seen: BTreeSet::new(),
            cap: cap.max(1),
        }
    }

    /// True only the first time an id is seen.
    pub fn is_new(&mut self, id: &str) -> bool {
        if self.seen.contains(id) {
            return false;
        }
        if self.seen.len() >= self.cap {
            self.seen.clear();
        }
        self.seen.insert(id.to_string());
        true
    }
}

/// One poll result. `gap`/`truncated` mean the client cannot prove no event was
/// lost: the caller must resync from the last durable cursor rather than emit a
/// false "all caught up".
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

/// A command the pure layer emits; the interpreter performs it. Journalling the
/// cursor precedes emitting a notice so a crash after emit replays from the
/// last durable cursor, deduped by eventId (effect law 7 + 8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeCmd {
    JournalCursor(String),
    Emit(ChannelNotice),
}

/// Pure fold of one page into commands + the next cursor. Dedupe is applied
/// here, so the interpreter has no decision logic. A page needing resync emits
/// NO commands: replaying a partial page would deliver duplicates or miss one.
pub fn apply_page(deduper: &mut Deduper, cursor: &mut String, page: &Page) -> Vec<BridgeCmd> {
    if page.needs_resync() {
        return Vec::new();
    }
    let mut cmds = Vec::new();
    for event in &page.events {
        if deduper.is_new(&event.id)
            && let Some(notice) = event_to_notice(event)
        {
            cmds.push(BridgeCmd::Emit(notice));
        }
    }
    if !page.next_cursor.is_empty() {
        *cursor = page.next_cursor.clone();
        cmds.insert(0, BridgeCmd::JournalCursor(page.next_cursor.clone()));
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
/// Pure: the jitter comes from `rand_unit` in `[0, 1)`, supplied by the caller,
/// so the schedule is testable without a real RNG or clock.
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

/// The transport port. Phase A tests use a fake; Phase B wires toron's
/// `events/poll` / `events/stream` through the same trait.
pub trait EventsTransport {
    fn poll(&self, subscription: &str, cursor: &str) -> Result<Page, String>;
}

/// One interpreter tick: poll once, fold the page, journal the cursor + notices
/// BEFORE handing them to the caller (effect law). The caller owns the sink.
/// Pure except for the injected transport; no clock, env, or global state.
pub fn tick<T: EventsTransport>(
    transport: &T,
    subscription: &str,
    deduper: &mut Deduper,
    cursor: &mut String,
) -> Result<Vec<BridgeCmd>, String> {
    let page = transport.poll(subscription, cursor)?;
    Ok(apply_page(deduper, cursor, &page))
}

/// The MCP method a bridge declares at `initialize`. Kept as data so the
/// capability advertisement is pure and testable.
pub const CHANNEL_CAPABILITY: &str = "claude/channel";

/// Pure JSON-RPC `initialize` result declaring the claude/channel capability.
/// No tool list, no body surface — only the channel door.
pub fn initialize_result() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { CHANNEL_CAPABILITY: {} },
        "serverInfo": { "name": "flywheel-events-bridge", "version": "1" }
    })
}

/// Pure mapping: one channel notice -> a JSON-RPC notification. The params carry
/// ONLY the hint text and event id — never the event body.
pub fn notice_to_rpc(notice: &ChannelNotice) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": [{ "type": "text", "text": notice.hint }],
            "eventId": notice.event_id
        }
    })
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
        let n = event_to_notice(&ev("e1", "mail")).expect("wake event maps");
        assert_eq!(n.hint, HINT);
        assert!(!format!("{n:?}").contains("SECRET BODY"), "{n:?}");
        assert!(event_to_notice(&ev("e2", "heartbeat")).is_none());
    }

    #[test]
    fn dedupe_suppresses_repeated_event_ids() {
        let mut d = Deduper::new(16);
        assert!(d.is_new("e1"));
        assert!(!d.is_new("e1"));
        assert!(d.is_new("e2"));
    }

    #[test]
    fn gap_or_truncated_emits_nothing_and_does_not_advance_cursor() {
        let mut d = Deduper::new(16);
        let mut c = "c0".to_string();
        for page in [
            Page {
                events: vec![ev("e1", "mail")],
                next_cursor: "c1".into(),
                gap: true,
                truncated: false,
            },
            Page {
                events: vec![ev("e2", "mail")],
                next_cursor: "c2".into(),
                gap: false,
                truncated: true,
            },
        ] {
            assert!(apply_page(&mut d, &mut c, &page).is_empty());
            assert_eq!(c, "c0");
        }
    }

    #[test]
    fn apply_page_dedupes_and_advances_cursor() {
        let mut d = Deduper::new(16);
        let mut c = "c0".to_string();
        let page = Page {
            events: vec![
                ev("e1", "mail"),
                ev("e1", "mail"),
                ev("e3", "mention"),
                ev("hb", "heartbeat"),
            ],
            next_cursor: "c7".into(),
            gap: false,
            truncated: false,
        };
        let cmds = apply_page(&mut d, &mut c, &page);
        assert!(matches!(cmds.first(), Some(BridgeCmd::JournalCursor(x)) if x == "c7"));
        let notices = notices_of(&cmds);
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[0].event_id, "e1");
        assert_eq!(notices[1].event_id, "e3");
        assert_eq!(c, "c7");
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
                events: vec![ev("x1", "mail")],
                next_cursor: "c9".into(),
                gap: false,
                truncated: false,
            }],
        };
        let mut d = Deduper::new(8);
        let mut c = "c0".into();
        let cmds = tick(&server, "sub-1", &mut d, &mut c).expect("tick");
        assert_eq!(notices_of(&cmds).len(), 1);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, BridgeCmd::JournalCursor(x) if x == "c9"))
        );
        assert_eq!(c, "c9");
    }

    #[test]
    fn initialize_declares_channel_and_notice_rpc_never_carries_body() {
        let init = initialize_result();
        assert!(
            init["capabilities"][CHANNEL_CAPABILITY].is_object(),
            "must declare claude/channel: {init}"
        );
        let n = event_to_notice(&ev("e1", "mail")).unwrap();
        let rpc = notice_to_rpc(&n).to_string();
        assert!(rpc.contains(HINT), "{rpc}");
        assert!(!rpc.contains("SECRET BODY"), "{rpc}");
    }
}

// governed-by: ADR-0039
#![forbid(unsafe_code)]

//! bd-l5vb2 U8: pure events -> doorbell mapping for harnesses with no inbound
//! door. The events-kernel page is folded here into typed brain messages; the
//! bus event id is the ONE dedupe key shared with wake.sock, and `gated()` in
//! the brain is the single budget gate. No clock, socket, env or journal here.

use crate::events_bridge::{self, BridgeCmd};
use doorbell_rt::BrainMsg;

/// Canonical pull line. `toron next` classifies as `AtIdle` (a real mail
/// wake); `SYSTEM_NOTICE_HINT` would be a silent notice that never rings.
pub fn mail_line() -> String {
    // bd-ofa1s: the event id rides as typed data on BrainMsg::EventsHint.
    doorbell_core::doorbell_line(doorbell_core::EventClass::PlanePMail, "toron next")
}

/// Pure: one events-kernel command -> the brain input that feeds the existing
/// fold. `JournalCursor` is an effect the interpreter performs, not a brain
/// input. A gap collapses to ONE digest msg, never a prompt storm.
pub fn cmd_to_brain(pin: &str, cmd: &BridgeCmd) -> Option<BrainMsg> {
    match cmd {
        BridgeCmd::JournalCursor(_) => None,
        BridgeCmd::Emit(notice) => {
            if notice.event_name == "gap" || notice.event_id.starts_with("gap:") {
                Some(BrainMsg::EventsGap {
                    pin: pin.to_string(),
                })
            } else {
                Some(BrainMsg::EventsHint {
                    event_id: notice.event_id.clone(),
                    pin: pin.to_string(),
                    line: mail_line(),
                })
            }
        }
    }
}

/// Pure fold of one page through the events kernel. The deduper keyed by bus
/// event id is what makes a seat reached by both wake.sock and events ring
/// once; the returned cursor is journaled AFTER the msgs are emitted.
pub fn fold_page(
    deduper: &mut events_bridge::Deduper,
    cursor: &mut String,
    page: &events_bridge::Page,
    pin: &str,
) -> (Vec<BrainMsg>, Option<String>) {
    let cmds = events_bridge::apply_page(deduper, cursor, page);
    let msgs = cmds.iter().filter_map(|c| cmd_to_brain(pin, c)).collect();
    let journal = cmds.iter().find_map(|c| match c {
        BridgeCmd::JournalCursor(c) => Some(c.clone()),
        _ => None,
    });
    (msgs, journal)
}

/// Harnesses whose only inbound door is toron events (ADR-0039 D8). Canonical
/// names; the operator list is gemini, antigravity, grok, codex until each
/// gains a native door.
pub fn events_only_harness(kind: &str) -> bool {
    matches!(
        crate::harness_registry::canonical(kind).as_str(),
        "gemini" | "antigravity" | "grok" | "codex"
    )
}

/// A journal port for one pump tick: the host appends a file row, a test
/// records the order in memory.
pub trait PumpJournal {
    fn row(&mut self, op: &str, cursor: Option<&str>);
}

/// A sink for the brain msgs one pump tick emits.
pub trait PumpSink {
    fn send(&mut self, msg: BrainMsg);
}

/// One fetch tick. The journal row is written BEFORE the RPC invoke, and the
/// emitted msgs go BEFORE the cursor journal (at-least-once replay). Pure
/// except for the injected ports: no clock, socket or env here.
pub fn pump_once<R: crate::events_client::EventsRpc, J: PumpJournal, S: PumpSink>(
    client: &crate::events_client::ToronEventsClient<R>,
    cursor: &mut String,
    deduper: &mut events_bridge::Deduper,
    pin: &str,
    journal: &mut J,
    sink: &mut S,
) -> Result<(), String> {
    journal.row("fetch", None);
    let page = client.fetch(cursor)?;
    let (msgs, next) = fold_page(deduper, cursor, &page, pin);
    for msg in msgs {
        sink.send(msg);
    }
    if let Some(next) = next {
        journal.row("cursor", Some(&next));
    }
    Ok(())
}

/// Pure seat selection. Flag off returns nothing (byte-identical); a seat is
/// pumped only when its harness has no inbound door, has a mail pin and has a
/// provisioned token file.
pub fn seats_to_pump(enabled: bool, seats: &[(String, Option<String>, bool)]) -> Vec<String> {
    if !enabled {
        return Vec::new();
    }
    seats
        .iter()
        .filter_map(|(harness, pin, has_token)| {
            (events_only_harness(harness) && *has_token)
                .then(|| pin.clone())
                .flatten()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! bd-l5vb2: the pure events fold that feeds the single budget gate.
    //! No socket, clock, env or live daemon — a fake Page drives every case.
    use super::*;
    use crate::events_bridge::{BridgeEvent, ChannelNotice, Page};
    use doorbell_core::{DoorbellAction, DoorbellModel};

    fn page(id: &str, cursor: &str) -> Page {
        Page {
            events: vec![BridgeEvent {
                id: id.into(),
                kind: "mail.addressed".into(),
                payload: String::new(),
            }],
            next_cursor: cursor.into(),
            gap: false,
            truncated: false,
        }
    }

    type Log = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    struct FakeRpc {
        poll: serde_json::Value,
        log: Log,
    }

    impl crate::events_client::EventsRpc for FakeRpc {
        fn call(
            &self,
            method: &str,
            _params: serde_json::Value,
            _last: Option<&str>,
        ) -> Result<serde_json::Value, String> {
            self.log.lock().unwrap().push(format!("rpc:{method}"));
            match method {
                "events/subscribe" => Ok(serde_json::json!({
                    "id": "s1", "expiresAt": 9_000_000_000i64, "cursor": "0"
                })),
                "events/poll" => Ok(self.poll.clone()),
                other => Err(format!("unexpected {other}")),
            }
        }

        fn stream(
            &self,
            _params: serde_json::Value,
            _last: Option<&str>,
            _on_data: &mut dyn FnMut(&str),
        ) -> Result<(), String> {
            self.log.lock().unwrap().push("stream".into());
            Err("stream down".into())
        }
    }

    struct Recorder {
        log: Log,
    }

    impl PumpJournal for Recorder {
        fn row(&mut self, op: &str, cursor: Option<&str>) {
            self.log
                .lock()
                .unwrap()
                .push(format!("journal:{op}:{}", cursor.unwrap_or("")));
        }
    }

    impl PumpSink for Recorder {
        fn send(&mut self, _msg: BrainMsg) {
            self.log.lock().unwrap().push("emit".into());
        }
    }

    fn pump_client(log: Log) -> crate::events_client::ToronEventsClient<FakeRpc> {
        let poll = serde_json::json!({
            "events":[{"event_id":"p1","name":"mail.addressed","body":"SECRET"}],
            "cursor":"7", "truncated": false
        });
        let mut client = crate::events_client::ToronEventsClient::new(
            FakeRpc {
                poll,
                log: log.clone(),
            },
            "mail.addressed",
            3600,
        );
        client.subscribe().expect("subscribe");
        log.lock().unwrap().clear(); // only the pump tick is under test
        client
    }

    #[test]
    fn the_events_line_is_a_real_mail_wake_not_a_silent_notice() {
        let line = mail_line();
        assert_eq!(
            doorbell_core::wake_kind_of_line(&line),
            doorbell_core::WakeKind::AtIdle,
            "an events hint must ring like a mail: {line}"
        );
    }

    #[test]
    fn the_watch_line_carries_the_same_bus_event_id() {
        let msg = cmd_to_brain(
            "P",
            &BridgeCmd::Emit(events_bridge::ChannelNotice {
                event_id: "bus-evt-777".into(),
                event_name: "mail.addressed".into(),
                hint: "hint".into(),
                count: 1,
            }),
        )
        .expect("a hint msg");
        let BrainMsg::EventsHint { event_id, line, .. } = msg else {
            panic!("expected EventsHint: {msg:?}");
        };
        assert_eq!(event_id, "bus-evt-777");
        assert_eq!(
            event_id.as_str(),
            "bus-evt-777",
            "the id is typed data on EventsHint, never parsed from the line"
        );
        assert!(
            doorbell_core::watch_id(&line).is_none(),
            "the plain-language wake carries no event id in prose"
        );
    }

    #[test]
    fn the_same_bus_event_id_rings_once_across_wake_sock_and_events() {
        let mut deduper = events_bridge::Deduper::new(1024);
        let mut cursor = String::new();
        let (first, c1) = fold_page(&mut deduper, &mut cursor, &page("bus-e1", "c1"), "P");
        assert_eq!(c1.as_deref(), Some("c1"));
        assert_eq!(first.len(), 1);
        assert!(matches!(
            &first[0],
            BrainMsg::EventsHint { event_id, pin, .. }
                if event_id == "bus-e1" && pin == "P"
        ));
        // wake.sock carried the same id: the shared deduper absorbs it.
        let (second, _) = fold_page(&mut deduper, &mut cursor, &page("bus-e1", "c2"), "P");
        assert!(
            second.is_empty(),
            "one mail must not ring twice: {second:?}"
        );
    }

    #[test]
    fn a_gap_collapses_to_one_digest_msg() {
        let mut deduper = events_bridge::Deduper::new(1024);
        let mut cursor = String::new();
        let gap = Page {
            events: Vec::new(),
            next_cursor: "fresh".into(),
            gap: true,
            truncated: false,
        };
        let (msgs, next) = fold_page(&mut deduper, &mut cursor, &gap, "P");
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], BrainMsg::EventsGap { .. }));
        assert_eq!(next.as_deref(), Some("fresh"));
        // In the core fold that msg becomes exactly one Digest action.
        let model = DoorbellModel::default();
        let (_, actions) = doorbell_core::tick(
            &model,
            &doorbell_core::DoorbellMsg::EventsGap { pin: "P".into() },
        );
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], DoorbellAction::Digest { .. }));
    }

    #[test]
    fn a_cursor_command_is_an_effect_not_a_brain_msg() {
        assert!(cmd_to_brain("P", &BridgeCmd::JournalCursor("c9".into())).is_none());
        let mail = BridgeCmd::Emit(ChannelNotice {
            hint: events_bridge::HINT.into(),
            event_id: "e9".into(),
            event_name: "mail.addressed".into(),
            count: 1,
        });
        assert!(matches!(
            cmd_to_brain("P", &mail),
            Some(BrainMsg::EventsHint { .. })
        ));
    }

    #[test]
    fn the_fetch_journal_lands_before_the_rpc_invoke() {
        let log: Log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = pump_client(log.clone());
        let mut cursor = String::new();
        let mut deduper = events_bridge::Deduper::new(1024);
        let mut journal = Recorder { log: log.clone() };
        let mut sink = Recorder { log: log.clone() };
        pump_once(
            &client,
            &mut cursor,
            &mut deduper,
            "P",
            &mut journal,
            &mut sink,
        )
        .expect("pump tick");
        let order = log.lock().unwrap().clone();
        assert_eq!(
            order[0], "journal:fetch:",
            "the fetch intent must be journaled BEFORE the invoke: {order:?}"
        );
        assert_eq!(cursor, "7");
    }

    #[test]
    fn a_poll_page_after_a_stream_failure_still_emits_one_hint() {
        let log: Log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let client = pump_client(log.clone());
        let mut cursor = String::new();
        let mut deduper = events_bridge::Deduper::new(1024);
        let mut journal = Recorder { log: log.clone() };
        let mut sink = Recorder { log: log.clone() };
        pump_once(
            &client,
            &mut cursor,
            &mut deduper,
            "P",
            &mut journal,
            &mut sink,
        )
        .expect("poll fallback");
        let order = log.lock().unwrap().clone();
        assert!(order.contains(&"rpc:events/poll".to_string()), "{order:?}");
        assert!(order.contains(&"emit".to_string()), "{order:?}");
        // The cursor is journaled AFTER the emit, never before.
        let emit = order.iter().position(|s| s == "emit").unwrap();
        let cur = order.iter().position(|s| s == "journal:cursor:7").unwrap();
        assert!(emit < cur, "emit before cursor journal: {order:?}");
    }

    #[test]
    fn flag_off_spawns_no_pump() {
        let seats = vec![
            ("grok".to_string(), Some("GrokPin".to_string()), true),
            ("opencode".to_string(), Some("OcPin".to_string()), true),
        ];
        assert!(
            seats_to_pump(false, &seats).is_empty(),
            "flag off spawns nothing"
        );
        assert_eq!(seats_to_pump(true, &seats), vec!["GrokPin".to_string()]);
    }

    #[test]
    fn flag_off_is_the_default_so_behaviour_is_byte_identical() {
        assert!(
            !crate::config::Config::default().doorbell_toron_events(),
            "the events source must be opt-in"
        );
    }

    #[test]
    fn only_the_named_no_door_harnesses_get_an_events_pump() {
        for kind in ["gemini", "antigravity", "grok", "codex", "gk", "cx"] {
            assert!(events_only_harness(kind), "{kind} has no inbound door");
        }
        for kind in ["opencode", "oc", "claude", "pi"] {
            assert!(!events_only_harness(kind), "{kind} has a native door");
        }
    }
}

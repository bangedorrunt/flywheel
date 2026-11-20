// governed-by: ADR-0039
#![forbid(unsafe_code)]

//! bd-l5vb2 U8: the toron-events wake source for harnesses with no inbound
//! door. Pure fold only: a page becomes typed doorbell msgs; the core's `rung`
//! set is the ONE dedupe key shared with wake.sock. HTTP transport lives in
//! the host; nothing here reads a clock, socket or env.

use doorbell_core::DoorbellMsg;

/// The events source is opt-in; flag-off must be byte-identical (U8 verify).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventsSource {
    pub enabled: bool,
    pub pin: String,
    pub cursor: String,
}

impl EventsSource {
    pub fn new(enabled: bool, pin: impl Into<String>) -> Self {
        Self {
            enabled,
            pin: pin.into(),
            cursor: String::new(),
        }
    }

    /// Canonical doorbell line used for a mail hint. `toron next` classifies
    /// as AtIdle (a real mail wake); `SYSTEM_NOTICE_HINT` would be a silent
    /// notice that never rings (bd-l5vb2).
    pub fn mail_line() -> String {
        doorbell_core::doorbell_line(doorbell_core::EventClass::PlanePMail, "toron next")
    }

    /// Journal first: every accepted page yields msgs BEFORE the cursor moves.
    /// The caller journals `Cursor(new)` only after emitting the msgs.
    pub fn apply(
        &self,
        events: &[SourceEvent],
        gap: bool,
        next_cursor: &str,
    ) -> (Vec<DoorbellMsg>, String) {
        if !self.enabled {
            return (Vec::new(), self.cursor.clone());
        }
        let mut msgs = Vec::new();
        for event in events {
            if event.name == "mail.addressed" || event.name == "gap" {
                msgs.push(DoorbellMsg::EventsHint {
                    event_id: event.id.clone(),
                    pin: self.pin.clone(),
                    line: Self::mail_line(),
                });
            }
        }
        if gap {
            msgs.push(DoorbellMsg::EventsGap {
                pin: self.pin.clone(),
            });
        }
        (msgs, next_cursor.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceEvent {
    pub id: String,
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use doorbell_core::DoorbellAction;

    fn event(id: &str, name: &str) -> SourceEvent {
        SourceEvent {
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn flag_off_is_byte_identical() {
        let source = EventsSource::new(false, "AmberProbe");
        let (msgs, cursor) = source.apply(&[event("e1", "mail.addressed")], true, "c7");
        assert!(msgs.is_empty(), "flag-off emits nothing");
        assert_eq!(cursor, "", "flag-off leaves the cursor untouched");
    }

    #[test]
    fn a_mail_hint_uses_the_bus_event_id_as_the_shared_key() {
        let source = EventsSource::new(true, "AmberProbe");
        let (msgs, cursor) = source.apply(&[event("bus-e1", "mail.addressed")], false, "c7");
        assert_eq!(cursor, "c7");
        assert_eq!(msgs.len(), 1);
        match &msgs[0] {
            DoorbellMsg::EventsHint {
                event_id,
                pin,
                line,
            } => {
                assert_eq!(event_id, "bus-e1");
                assert_eq!(pin, "AmberProbe");
                assert!(!line.is_empty() && !line.contains("body"));
            }
            other => panic!("expected EventsHint, got {other:?}"),
        }
    }

    #[test]
    fn a_gap_is_one_digest_msg_not_a_prompt() {
        let source = EventsSource::new(true, "AmberProbe");
        let (msgs, _) = source.apply(&[], true, "c7");
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], DoorbellMsg::EventsGap { .. }));
        // In the core fold that msg becomes exactly one Digest action.
        let model = doorbell_core::DoorbellModel::default();
        let (_, actions) = doorbell_core::tick(&model, &msgs[0]);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], DoorbellAction::Digest { .. }));
    }
}

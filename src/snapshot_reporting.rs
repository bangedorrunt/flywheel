// governed-by: ADR-0016
//! Pure snapshot reporting projections.
//!
//! This module consumes an assembled snapshot only. It does not read state,
//! clocks, environment, or external services.

use crate::snapshot::Snapshot;

/// Captain bearings: one-line fleet summary for TTY surfaces.
pub(crate) const COALITION_LIVE_SECS: u64 = 48 * 3600;

pub(crate) fn looks_like_pane_id(pin: &str) -> bool {
    let Some((workspace, pane)) = pin.split_once(':') else {
        return false;
    };
    !workspace.is_empty() && pane.starts_with('p')
}

pub(crate) fn fleet_joinable(dispatcher: &str, pins: &[crate::snapshot::PinRow]) -> bool {
    dispatcher == "up"
        || pins
            .iter()
            .any(|pin| !looks_like_pane_id(&pin.pin) && !pin.pin.is_empty())
}

pub(crate) fn coalition_is_live(created_at: &str, now_unix: u64) -> bool {
    let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(created_at) else {
        return false;
    };
    now_unix.saturating_sub(timestamp.timestamp() as u64) < COALITION_LIVE_SECS
}

pub(crate) fn bearings_line(snap: &Snapshot) -> String {
    let join = if snap.joinable { " JOINABLE" } else { "" };
    format!(
        "fleet: {} pins, {} open decisions, {} afk queued, {} unacked wakes dispatcher={}{join}{}",
        snap.pins.len(),
        snap.decisions.len(),
        snap.afk,
        snap.journal.unacked_wakes,
        snap.dispatcher,
        if snap.degraded.is_empty() {
            String::new()
        } else {
            format!(" (degraded: {})", snap.degraded.join(","))
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearings_are_deterministic_and_surface_degradation() {
        let mut snapshot = Snapshot {
            schema: "flywheel-snapshot.v1".into(),
            generated_at: 1,
            workspace: "w".into(),
            freshness_epoch: 1,
            pins: Vec::new(),
            lanes: Vec::new(),
            decisions: Vec::new(),
            mail: crate::snapshot::MailCounts {
                owed: 0,
                acked: 0,
                resulted: 0,
                degraded: false,
            },
            journal: crate::snapshot::JournalState {
                last_seq: 0,
                last_presented: 0,
                last_acked: 0,
                unacked_wakes: 0,
            },
            drain_pending: 0,
            afk: 0,
            degraded: vec!["mail".into()],
            joinable: true,
            dispatcher: "down".into(),
            held_paths: Vec::new(),
            coalitions: Vec::new(),
            launch_pending: 0,
            loop_state: Default::default(),
        };
        assert_eq!(
            bearings_line(&snapshot),
            "fleet: 0 pins, 0 open decisions, 0 afk queued, 0 unacked wakes dispatcher=down JOINABLE (degraded: mail)"
        );
        snapshot.journal.unacked_wakes = 1;
        assert_ne!(bearings_line(&snapshot), "");
    }
}

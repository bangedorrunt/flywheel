//! Hardening slice 2 (bd-c0ha): one captain page for a dead doorbell
//! dispatcher or a stale presence heartbeat, then silence until recovery.
//!
//! The supervisor pass already pages a dead stop. This rides the same probe:
//! a dispatcher whose pid file names no live process pages once; a live pid
//! clears the hold so a later death pages again. A heartbeat older than the
//! toron presence TTL pages only when [`pane_binding_may_clear`] says the
//! binding is a herdr pane herdr actually answered for. A pin herdr cannot
//! see on purpose (freebuff, superlogical, unknown harness) has no herdr
//! heartbeat and is never a stall.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

/// toron's presence TTL (`toron-cli` plane.rs): a heartbeat older than this
/// is stale. The 45 minute dead-stop and 240s write-liveness windows are
/// different signals and stay separate.
pub const PRESENCE_TTL_SECS: u64 = 60;

/// §6: a herdr pane binding may clear only when herdr is the backend, the
/// harness is one herdr injects, a pane is bound, and herdr answered without
/// that pane. Every other binding is kept: absence of a herdr pane is not
/// death for a pin herdr cannot see.
pub fn pane_binding_may_clear(
    harness: &str,
    backend: &str,
    pane_id: &str,
    live_panes: Option<&HashSet<String>>,
) -> bool {
    backend == "herdr"
        && matches!(harness, "opencode" | "pi" | "grok" | "omp")
        && !pane_id.is_empty()
        && live_panes.is_some_and(|live| !live.contains(pane_id))
}

/// What one doorbell workspace's pid file says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatcherSignal {
    /// The pid file names a process that is alive.
    Alive,
    /// The pid file names a process that is not alive.
    Dead { line: String },
    /// The file is missing, unreadable, or does not parse. No evidence of a
    /// dispatcher death, so it never pages.
    Unreadable,
}

/// What one herdr-bound pin's presence row says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeartbeatSignal {
    /// A heartbeat inside the TTL.
    Fresh,
    /// Stale, and the binding is one herdr may clear: page.
    Stale { age_secs: u64, line: String },
    /// Stale, but the binding is not herdr-injectable (or herdr did not
    /// answer): keep it, never page.
    StaleKept,
}

/// One page-or-clear decision. Every variant is a state transition for
/// [`HealthHold`] and, in production, a supervisor message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HealthEvent {
    /// A dispatcher died and was not already paged.
    DeadDoorbell { label: String, line: String },
    /// A dispatcher is live again (or its workspace is gone): clear the hold.
    DoorbellRecovered { label: String },
    /// A herdr-bound pin's heartbeat went stale and was not already paged.
    StaleHeartbeat { pin: String, line: String },
    /// The heartbeat is fresh again (or the pin is gone): clear the hold.
    HeartbeatRecovered { pin: String },
}

/// Page-once state. Lives in the supervisor model so a replayed pass folds
/// the same holds, and a recovery clears them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HealthHold {
    /// Dead doorbell labels already paged.
    pub dead_doorbells: BTreeSet<String>,
    /// Pins already paged for a stale heartbeat.
    pub stale_pins: BTreeSet<String>,
}

impl HealthHold {
    /// Fold one probe snapshot. A dead dispatcher pages once; a live pid
    /// clears. A stale eligible heartbeat pages once; a fresh beat clears.
    /// Unreadable dispatchers and kept heartbeats change nothing.
    pub fn observe(
        &mut self,
        dispatchers: &BTreeMap<String, DispatcherSignal>,
        heartbeats: &BTreeMap<String, HeartbeatSignal>,
    ) -> Vec<HealthEvent> {
        let mut out = Vec::new();
        for (label, signal) in dispatchers {
            match signal {
                DispatcherSignal::Dead { line } => {
                    if self.dead_doorbells.insert(label.clone()) {
                        out.push(HealthEvent::DeadDoorbell {
                            label: label.clone(),
                            line: line.clone(),
                        });
                    }
                }
                DispatcherSignal::Alive => {
                    if self.dead_doorbells.remove(label) {
                        out.push(HealthEvent::DoorbellRecovered {
                            label: label.clone(),
                        });
                    }
                }
                DispatcherSignal::Unreadable => {}
            }
        }
        for label in self.dead_doorbells.clone() {
            if !dispatchers.contains_key(&label) {
                self.dead_doorbells.remove(&label);
                out.push(HealthEvent::DoorbellRecovered { label });
            }
        }
        for (pin, signal) in heartbeats {
            match signal {
                HeartbeatSignal::Stale { line, .. } => {
                    if self.stale_pins.insert(pin.clone()) {
                        out.push(HealthEvent::StaleHeartbeat {
                            pin: pin.clone(),
                            line: line.clone(),
                        });
                    }
                }
                HeartbeatSignal::Fresh => {
                    if self.stale_pins.remove(pin) {
                        out.push(HealthEvent::HeartbeatRecovered { pin: pin.clone() });
                    }
                }
                // Kept is not recovery: herdr did not answer, or the pin is
                // not one herdr injects. Stay silent until a fresh beat.
                HeartbeatSignal::StaleKept => {}
            }
        }
        for pin in self.stale_pins.clone() {
            if !heartbeats.contains_key(&pin) {
                self.stale_pins.remove(&pin);
                out.push(HealthEvent::HeartbeatRecovered { pin });
            }
        }
        out
    }
}

/// The binding inputs for one spawn-registry pin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeartbeatRow {
    pub pin: String,
    pub harness: String,
    pub backend: String,
    pub pane_id: String,
}

/// Registry rows that could hold a herdr pane binding. A row with no pin or
/// no pane is invisible to this probe (and to the wake layer).
pub fn registry_rows(cfg: &crate::config::Config, project: &str) -> Vec<HeartbeatRow> {
    let effective = crate::backend::resolve_backend_name(cfg);
    let catalog = crate::profile::ProfileCatalog::load().ok();
    crate::state::load_spawn_registry(project)
        .into_iter()
        .filter_map(|e| {
            let pin = e.mail_identity.filter(|p| !p.trim().is_empty())?;
            if e.pane_id.trim().is_empty() {
                return None;
            }
            let harness = e.harness.unwrap_or_default();
            // The profile is the spawn-time route; a bare row follows the
            // process backend.
            let backend = e
                .profile
                .as_deref()
                .and_then(|p| catalog.as_ref().map(|c| c.backend(p).to_string()))
                .unwrap_or_else(|| effective.clone());
            Some(HeartbeatRow {
                pin,
                harness,
                backend,
                pane_id: e.pane_id,
            })
        })
        .collect()
}

/// Latest heartbeat (unix seconds) per pin for `project`. Rows for other
/// projects are ignored; malformed lines are skipped. This reads the raw
/// projection on purpose: `toron presence list` drops rows past the TTL and
/// cannot show staleness.
pub fn latest_heartbeats(project: &str, body: &str) -> BTreeMap<String, u64> {
    let mut latest = BTreeMap::new();
    for line in body.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("project").and_then(|p| p.as_str()) != Some(project) {
            continue;
        }
        let (Some(name), Some(ts)) = (
            v.get("name").and_then(|n| n.as_str()),
            v.get("ts").and_then(serde_json::Value::as_u64),
        ) else {
            continue;
        };
        latest
            .entry(name.to_string())
            .and_modify(|t| {
                if ts > *t {
                    *t = ts;
                }
            })
            .or_insert(ts);
    }
    latest
}

/// Per-pin heartbeat signals. A pin with no projection row is absent: no
/// heartbeat is not a stall (freebuff has none by design). A stale row pages
/// only when its binding may clear.
pub fn heartbeat_signals(
    project: &str,
    now: u64,
    rows: &[HeartbeatRow],
    presence_body: &str,
    live_panes: Option<&HashSet<String>>,
) -> BTreeMap<String, HeartbeatSignal> {
    let beats = latest_heartbeats(project, presence_body);
    let mut out = BTreeMap::new();
    for row in rows {
        let Some(ts) = beats.get(&row.pin) else {
            continue;
        };
        let age = now.saturating_sub(*ts);
        let signal = if age <= PRESENCE_TTL_SECS {
            HeartbeatSignal::Fresh
        } else if pane_binding_may_clear(&row.harness, &row.backend, &row.pane_id, live_panes) {
            HeartbeatSignal::Stale {
                age_secs: age,
                line: format!(
                    "loop-mode supervise: STALE HEARTBEAT in {project} — {} (pane {}, harness {}) has not beat for {age}s (>{PRESENCE_TTL_SECS}s) and herdr no longer lists its pane",
                    row.pin, row.pane_id, row.harness
                ),
            }
        } else {
            HeartbeatSignal::StaleKept
        };
        out.insert(row.pin.clone(), signal);
    }
    out
}

/// Doorbell workspaces from `<state>/doorbell/*.gen`, classified by their pid
/// file. A missing pid file is unreadable, not dead: the `.gen` row alone
/// does not prove a dispatcher ever ran here.
pub fn dispatcher_signals(state_dir: &Path, project: &str) -> BTreeMap<String, DispatcherSignal> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(state_dir.join("doorbell")) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|x| x.to_str()) != Some("gen") {
            continue;
        }
        let Some(label) = path.file_stem().and_then(|x| x.to_str()) else {
            continue;
        };
        let pid_path = crate::doorbell::pid_file(state_dir, label);
        let signal = match std::fs::read_to_string(&pid_path) {
            Ok(raw) => match raw.trim().parse::<u32>() {
                Ok(pid) if crate::doorbell::pid_alive(pid) => DispatcherSignal::Alive,
                Ok(_) => DispatcherSignal::Dead {
                    line: format!(
                        "loop-mode supervise: DEAD DOORBELL in {project} — dispatcher {label} has no live pid; restart it (flywheel doorbell start {project})"
                    ),
                },
                Err(_) => DispatcherSignal::Unreadable,
            },
            Err(_) => DispatcherSignal::Unreadable,
        };
        out.insert(label.to_string(), signal);
    }
    out
}

/// One probe: read the pid files and the presence projection, then fold the
/// page-once state.
pub fn probe_events(
    state_dir: &Path,
    project: &str,
    now: u64,
    presence_body: &str,
    rows: &[HeartbeatRow],
    live_panes: Option<&HashSet<String>>,
    hold: &mut HealthHold,
) -> Vec<HealthEvent> {
    let dispatchers = dispatcher_signals(state_dir, project);
    let heartbeats = heartbeat_signals(project, now, rows, presence_body, live_panes);
    hold.observe(&dispatchers, &heartbeats)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pid the OS cannot have: above both macOS `pid_max` (~99998) and
    /// Linux's default (4194304). `kill -0` fails, so the pid file
    /// classifies Dead without racing a recycled pid.
    const NO_SUCH_PID: u32 = 9_999_999;

    fn write_workspace(dir: &Path, label: &str, pid: Option<&str>) {
        let db = dir.join("doorbell");
        std::fs::create_dir_all(&db).unwrap();
        std::fs::write(db.join(format!("{label}.gen")), "ran\n").unwrap();
        if let Some(pid) = pid {
            std::fs::write(db.join(format!("{label}.pid")), format!("{pid}\n")).unwrap();
        }
    }

    fn page_pin<'a>(events: &'a [HealthEvent], pin: &str) -> Option<&'a str> {
        events.iter().find_map(|e| match e {
            HealthEvent::StaleHeartbeat { pin: p, .. } if p == pin => Some("stale"),
            HealthEvent::DeadDoorbell { label, .. } if label == pin => Some("dead"),
            _ => None,
        })
    }

    fn pages_label(events: &[HealthEvent], label: &str) -> bool {
        events.iter().any(|e| match e {
            HealthEvent::DeadDoorbell { label: l, .. } => l == label,
            _ => false,
        })
    }

    #[test]
    fn a_dead_doorbell_pages_once_and_a_mail_only_pin_does_not() {
        // tempfile: HOME/XDG are hermetic in the harness; the tempdir keeps
        // the fake doorbell dir off the operator's state.
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path();
        write_workspace(state, "deadws", Some(&NO_SUCH_PID.to_string()));
        write_workspace(state, "livews", Some(&std::process::id().to_string()));
        write_workspace(state, "unreadablews", Some("not-a-pid"));

        let live: HashSet<String> = ["w1:p1".to_string()].into_iter().collect();
        let now = 1_000_000u64;
        let stale_ts = now - PRESENCE_TTL_SECS - 1;
        let presence = format!(
            "{{\"name\":\"RustyOtter\",\"project\":\"p\",\"ts\":{stale_ts}}}\n\
             {{\"name\":\"SuperPin\",\"project\":\"p\",\"ts\":{stale_ts}}}\n\
             {{\"name\":\"HerdrDown\",\"project\":\"p\",\"ts\":{stale_ts}}}\n"
        );
        // FreeBuff deliberately has no row: no heartbeat is not a stall.
        let rows = vec![
            HeartbeatRow {
                pin: "RustyOtter".into(),
                harness: "opencode".into(),
                backend: "herdr".into(),
                pane_id: "w1:p2".into(),
            },
            HeartbeatRow {
                pin: "FreeBuff".into(),
                harness: "freebuff".into(),
                backend: "herdr".into(),
                pane_id: "w1:p9".into(),
            },
            HeartbeatRow {
                pin: "SuperPin".into(),
                harness: "opencode".into(),
                backend: "superlogical".into(),
                pane_id: "w1:p3".into(),
            },
        ];
        let mut hold = HealthHold::default();

        // Pass 1: the dead dispatcher and the one clearable stale pin page.
        let dispatchers = dispatcher_signals(state, "p");
        assert!(matches!(
            dispatchers.get("deadws"),
            Some(DispatcherSignal::Dead { .. })
        ));
        assert_eq!(dispatchers.get("livews"), Some(&DispatcherSignal::Alive));
        assert_eq!(
            dispatchers.get("unreadablews"),
            Some(&DispatcherSignal::Unreadable)
        );
        let beats = heartbeat_signals("p", now, &rows, &presence, Some(&live));
        let events = hold.observe(&dispatchers, &beats);
        assert!(
            pages_label(&events, "deadws"),
            "a dead dispatcher pages once: {events:?}"
        );
        assert!(
            page_pin(&events, "RustyOtter").is_some(),
            "an eligible stale heartbeat pages once: {events:?}"
        );
        assert!(!pages_label(&events, "livews"), "a live pid never pages");
        assert!(
            !pages_label(&events, "unreadablews"),
            "an unreadable pid file never pages"
        );
        assert!(
            page_pin(&events, "FreeBuff").is_none(),
            "a freebuff pin with no heartbeat is not a stall: {events:?}"
        );
        assert!(
            page_pin(&events, "SuperPin").is_none(),
            "a superlogical backend never pages"
        );
        // The §6 gate itself, not only the absence of a heartbeat row.
        assert!(
            !pane_binding_may_clear("freebuff", "herdr", "w1:p9", Some(&live)),
            "freebuff is never a clearable herdr binding"
        );

        // herdr not answering is not a clearance either: the stale pin is
        // kept and nothing pages.
        let down_rows = vec![HeartbeatRow {
            pin: "HerdrDown".into(),
            harness: "opencode".into(),
            backend: "herdr".into(),
            pane_id: "w1:p4".into(),
        }];
        let down_beats = heartbeat_signals("p", now, &down_rows, &presence, None);
        assert_eq!(
            down_beats.get("HerdrDown"),
            Some(&HeartbeatSignal::StaleKept),
            "live_panes=None must keep the pin"
        );
        let mut down_hold = HealthHold::default();
        assert!(
            down_hold.observe(&BTreeMap::new(), &down_beats).is_empty(),
            "herdr silence never pages"
        );

        // Pass 2: the same snapshot is silent; the hold is the page-once.
        let events = hold.observe(
            &dispatcher_signals(state, "p"),
            &heartbeat_signals("p", now, &rows, &presence, Some(&live)),
        );
        assert!(
            events.is_empty(),
            "the next pass must not page again: {events:?}"
        );

        // Recovery: a live pid and a fresh beat clear both holds.
        write_workspace(state, "deadws", Some(&std::process::id().to_string()));
        let fresh = format!("{{\"name\":\"RustyOtter\",\"project\":\"p\",\"ts\":{now}}}\n");
        let events = hold.observe(
            &dispatcher_signals(state, "p"),
            &heartbeat_signals("p", now, &rows, &fresh, Some(&live)),
        );
        assert!(
            events.iter().any(
                |e| matches!(e, HealthEvent::DoorbellRecovered { label } if label == "deadws")
            ),
            "a live pid clears the doorbell hold: {events:?}"
        );
        assert!(
            events.iter().any(
                |e| matches!(e, HealthEvent::HeartbeatRecovered { pin } if pin == "RustyOtter")
            ),
            "a fresh heartbeat clears the pin hold: {events:?}"
        );
        assert!(hold.dead_doorbells.is_empty());
        assert!(hold.stale_pins.is_empty());

        // A later death pages once more.
        write_workspace(state, "deadws", Some(&NO_SUCH_PID.to_string()));
        let events = hold.observe(
            &dispatcher_signals(state, "p"),
            &heartbeat_signals("p", now, &rows, &presence, Some(&live)),
        );
        assert!(
            pages_label(&events, "deadws"),
            "a second death pages once more: {events:?}"
        );
        assert!(page_pin(&events, "RustyOtter").is_some());
        assert!(
            !pages_label(&events, "unreadablews"),
            "the unreadable pid file stays silent on every pass"
        );
    }
}

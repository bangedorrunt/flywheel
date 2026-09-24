//! Wake pipe quiet-vs-deaf classifier — GS-062 live on `/health`
//! (toron ADR-0036 / GS-061 scout pattern, external; not an in-repo ADR).
//!
//! Distinguishes a **quiet** wake pipe (heartbeats arriving, no events) from
//! a **deaf** pipe (no frames past the idle ceiling), or **never-stamped**
//! (never observed ≠ deaf). No second broker / Redis / Funnel — pure
//! classification over host-observed timestamps. Wired into flywheel
//! `serve` `/health` JSON (GS-062).
//!
//! Callers must stamp **event** and **heartbeat** separately
//! (`doorbell_rt::watchbeat::{touch_event,touch_heartbeat}`) — never pass
//! `max(event, hb)` into `last_event_ms`.

/// Default deaf ceiling (ms): silence past this is deaf, never quiet.
pub const DEFAULT_DEAF_CEILING_MS: u64 = 60_000;

/// Pipe health relative to wake.sock heartbeats / events.
///
/// `/health` serializers: `Live` / `Quiet` carry **absolute** millisecond
/// stamps; `Deaf` carries a measured **age** (`silent_for_ms`);
/// `NeverStamped` carries neither (never observed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeHealth {
    /// Events flowing recently.
    Live { last_event_ms: u64 },
    /// No events, but heartbeats prove the pipe is alive.
    Quiet { last_heartbeat_ms: u64 },
    /// No frame of any kind past the ceiling — watcher is deaf.
    ///
    /// `silent_for_ms` is the measured age of the most recent observed frame
    /// (min of present event/hb ages). Always a real age — never a fabricated
    /// ceiling stand-in.
    Deaf { silent_for_ms: u64 },
    /// Never observed a stamp — UNKNOWN. Distinct from [`Deaf`]: never
    /// observed ≠ silent-too-long (watchbeat honesty).
    NeverStamped,
}

/// Classify wake-pipe liveness from host-observed timestamps.
///
/// - `last_event_ms` — last **event** only (not heartbeat). Do **not** pass
///   `max(event, hb)` here; that collapses Quiet into Live.
/// - `last_heartbeat_ms` — last heartbeat only (`None` if never seen).
///
/// Quiet requires a heartbeat inside the ceiling; deaf is silence past the
/// ceiling after a known observation. Never-stamped (both `None`) is
/// [`PipeHealth::NeverStamped`] — not Deaf.
pub fn classify(
    now_ms: u64,
    last_event_ms: Option<u64>,
    last_heartbeat_ms: Option<u64>,
    deaf_ceiling_ms: u64,
) -> PipeHealth {
    if let Some(ev) = last_event_ms {
        let age = now_ms.saturating_sub(ev);
        if age <= deaf_ceiling_ms {
            return PipeHealth::Live { last_event_ms: ev };
        }
    }
    if let Some(hb) = last_heartbeat_ms {
        let age = now_ms.saturating_sub(hb);
        if age <= deaf_ceiling_ms {
            return PipeHealth::Quiet {
                last_heartbeat_ms: hb,
            };
        }
    }

    // Never stamped: never observed ≠ deaf.
    if last_event_ms.is_none() && last_heartbeat_ms.is_none() {
        return PipeHealth::NeverStamped;
    }

    // Deaf: measured last-frame age from present stamps only. Ceiling is the
    // Live/Quiet threshold above — never a stand-in timestamp when unstamped.
    let silent_for_ms = match (last_event_ms, last_heartbeat_ms) {
        (Some(ev), None) => now_ms.saturating_sub(ev),
        (None, Some(hb)) => now_ms.saturating_sub(hb),
        (Some(ev), Some(hb)) => now_ms.saturating_sub(ev).min(now_ms.saturating_sub(hb)),
        (None, None) => unreachable!("handled above"),
    };
    PipeHealth::Deaf { silent_for_ms }
}

/// Serde-friendly JSON object for `/health` (no serde derive required).
pub fn to_json(h: PipeHealth) -> serde_json::Value {
    match h {
        PipeHealth::Live { last_event_ms } => serde_json::json!({
            "class": "Live",
            "last_event_ms": last_event_ms,
        }),
        PipeHealth::Quiet { last_heartbeat_ms } => serde_json::json!({
            "class": "Quiet",
            "last_heartbeat_ms": last_heartbeat_ms,
        }),
        PipeHealth::Deaf { silent_for_ms } => serde_json::json!({
            "class": "Deaf",
            "silent_for_ms": silent_for_ms,
        }),
        PipeHealth::NeverStamped => serde_json::json!({
            "class": "NeverStamped",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_when_heartbeat_inside_ceiling() {
        let h = classify(100_000, None, Some(90_000), 60_000);
        assert_eq!(
            h,
            PipeHealth::Quiet {
                last_heartbeat_ms: 90_000
            }
        );
    }

    #[test]
    fn deaf_when_no_frames_past_ceiling() {
        let h = classify(200_000, None, Some(100_000), 60_000);
        assert_eq!(
            h,
            PipeHealth::Deaf {
                silent_for_ms: 100_000
            }
        );
    }

    #[test]
    fn live_when_event_recent() {
        let h = classify(50_000, Some(49_000), Some(40_000), 60_000);
        assert_eq!(
            h,
            PipeHealth::Live {
                last_event_ms: 49_000
            }
        );
    }

    #[test]
    fn never_stamped_is_unknown_not_deaf() {
        // both None — never observed ≠ Deaf (watchbeat honesty)
        let h = classify(0, None, None, 60_000);
        assert_eq!(h, PipeHealth::NeverStamped);
        let h = classify(1_000, None, None, 60_000);
        assert_eq!(h, PipeHealth::NeverStamped);
    }

    #[test]
    fn stale_event_without_heartbeat_uses_event_age() {
        // last_heartbeat None + stale event: measured age, not ceiling stand-in
        let h = classify(200_000, Some(100_000), None, 60_000);
        assert_eq!(
            h,
            PipeHealth::Deaf {
                silent_for_ms: 100_000
            }
        );
    }

    #[test]
    fn both_stale_silent_for_is_min_frame_age() {
        // event more recent than hb, both past ceiling → min age = event age
        let h = classify(200_000, Some(120_000), Some(100_000), 60_000);
        assert_eq!(
            h,
            PipeHealth::Deaf {
                silent_for_ms: 80_000
            }
        );
    }

    #[test]
    fn event_only_param_does_not_treat_hb_as_event() {
        // Passing hb-only via last_heartbeat keeps Quiet; last_event stays None.
        // Callers must not collapse max(event,hb) into last_event_ms.
        let h = classify(100_000, None, Some(95_000), 60_000);
        assert_eq!(
            h,
            PipeHealth::Quiet {
                last_heartbeat_ms: 95_000
            }
        );
        // If a caller wrongly stuffed hb into last_event_ms, they'd get Live —
        // document via this contrast that Quiet is the honest shape.
        let collapsed = classify(100_000, Some(95_000), Some(95_000), 60_000);
        assert_eq!(
            collapsed,
            PipeHealth::Live {
                last_event_ms: 95_000
            }
        );
    }

    #[test]
    fn health_json_shape_from_classify() {
        let live = to_json(classify(50_000, Some(49_000), Some(40_000), 60_000));
        assert_eq!(live["class"], "Live");
        let quiet = to_json(classify(100_000, None, Some(90_000), 60_000));
        assert_eq!(quiet["class"], "Quiet");
        let deaf = to_json(classify(200_000, None, Some(100_000), 60_000));
        assert_eq!(deaf["class"], "Deaf");
        let never = to_json(classify(0, None, None, 60_000));
        assert_eq!(never["class"], "NeverStamped");
    }
}

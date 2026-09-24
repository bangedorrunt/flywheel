//! Wake pipe quiet-vs-deaf classifier (GS-061 / ADR-0036 / Buzz #7546 pattern).
//!
//! Distinguishes a **quiet** wake.sock (heartbeats arriving, no events) from a
//! **deaf** pipe (no frames at all past the idle ceiling). No second broker /
//! Redis / Funnel — pure classification over host-observed timestamps.

/// Default deaf ceiling (ms): silence past this is deaf, never quiet.
pub const DEFAULT_DEAF_CEILING_MS: u64 = 60_000;

/// Pipe health relative to wake.sock heartbeats / events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeHealth {
    /// Events flowing recently.
    Live { last_event_ms: u64 },
    /// No events, but heartbeats prove the pipe is alive.
    Quiet { last_heartbeat_ms: u64 },
    /// No frame of any kind past the ceiling — watcher is deaf.
    Deaf { silent_for_ms: u64 },
}

/// Classify wake-pipe liveness from host-observed timestamps.
///
/// `last_frame_ms` = last event OR heartbeat. `last_heartbeat_ms` = last
/// heartbeat only (None if never seen). Quiet requires a heartbeat inside
/// the ceiling; deaf is total silence.
pub fn classify(
    now_ms: u64,
    last_event_ms: Option<u64>,
    last_heartbeat_ms: Option<u64>,
    deaf_ceiling_ms: u64,
) -> PipeHealth {
    if let Some(ev) = last_event_ms {
        let age = now_ms.saturating_sub(ev);
        if age <= deaf_ceiling_ms {
            return PipeHealth::Live {
                last_event_ms: ev,
            };
        }
    }
    if let Some(hb) = last_heartbeat_ms {
        let age = now_ms.saturating_sub(hb);
        if age <= deaf_ceiling_ms {
            return PipeHealth::Quiet {
                last_heartbeat_ms: hb,
            };
        }
        return PipeHealth::Deaf { silent_for_ms: age };
    }
    // Never saw a frame.
    PipeHealth::Deaf {
        silent_for_ms: deaf_ceiling_ms,
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
}

//! GS-062 wake-pipe classifier — shared implementation in
//! `doorbell_rt::wake_liveness` (also used by operator doorbell `/health`).
pub use doorbell_rt::wake_liveness::*;

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
        let h = classify(0, None, None, 60_000);
        assert_eq!(h, PipeHealth::NeverStamped);
        let h = classify(1_000, None, None, 60_000);
        assert_eq!(h, PipeHealth::NeverStamped);
    }

    #[test]
    fn stale_event_without_heartbeat_uses_event_age() {
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
        let h = classify(100_000, None, Some(95_000), 60_000);
        assert_eq!(
            h,
            PipeHealth::Quiet {
                last_heartbeat_ms: 95_000
            }
        );
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

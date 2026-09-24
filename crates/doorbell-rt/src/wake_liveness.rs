//! Wake pipe quiet-vs-deaf classifier — GS-062 live on `/health`
//! (toron ADR-0036 / GS-061 scout pattern, external; not an in-repo ADR).
//!
//! Distinguishes a **quiet** wake pipe (heartbeats arriving, no events) from
//! a **deaf** pipe (no frames past the idle ceiling), or **never-stamped**
//! (never observed ≠ deaf). No second broker / Redis / Funnel — pure
//! classification over host-observed timestamps. Wired into flywheel `serve` `/health` and operator doorbell
//! `/health` (:18980) JSON (GS-062).
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


/// Worst-of severity for multi-hosted wake aggregation (ask-user F).
/// Higher = worse. Order (worst → best):
/// [`PipeHealth::NeverStamped`] > [`PipeHealth::Deaf`] > [`PipeHealth::Quiet`]
/// > [`PipeHealth::Live`].
///
/// Deaf/NeverStamped must win over Live when any hosted workspace is bad —
/// mirrors how `watch.last_event_age_s` aggregates across all hosted (min age),
/// but for class we take the **worst** pipe rather than primary-only.
pub fn pipe_severity(h: PipeHealth) -> u8 {
    match h {
        PipeHealth::NeverStamped => 3,
        PipeHealth::Deaf { .. } => 2,
        PipeHealth::Quiet { .. } => 1,
        PipeHealth::Live { .. } => 0,
    }
}

/// Return the worse of two pipe healths per [`pipe_severity`].
pub fn worse_pipe(a: PipeHealth, b: PipeHealth) -> PipeHealth {
    if pipe_severity(a) >= pipe_severity(b) {
        a
    } else {
        b
    }
}

/// Classify each hosted `(last_event_ms, last_heartbeat_ms)` pair and return
/// the **worst** [`PipeHealth`] (ask-user F).
///
/// Empty iterator → [`PipeHealth::NeverStamped`] (no alive hosted / nothing to
/// observe). Does **not** max-merge stamps then classify once (that would let
/// one Live workspace mask another Deaf).
pub fn classify_worst_of(
    now_ms: u64,
    stamps: impl IntoIterator<Item = (Option<u64>, Option<u64>)>,
    deaf_ceiling_ms: u64,
) -> PipeHealth {
    let mut worst: Option<PipeHealth> = None;
    for (ev, hb) in stamps {
        let h = classify(now_ms, ev, hb, deaf_ceiling_ms);
        worst = Some(match worst {
            None => h,
            Some(w) => worse_pipe(w, h),
        });
    }
    worst.unwrap_or(PipeHealth::NeverStamped)
}

/// Like [`classify_worst_of`], also returning the stamp pair of the chosen
/// worst workspace (first max-severity in iteration order). Empty →
/// `(NeverStamped, None, None)`.
pub fn classify_worst_of_stamps(
    now_ms: u64,
    stamps: impl IntoIterator<Item = (Option<u64>, Option<u64>)>,
    deaf_ceiling_ms: u64,
) -> (PipeHealth, Option<u64>, Option<u64>) {
    let mut best: Option<(PipeHealth, Option<u64>, Option<u64>)> = None;
    for (ev, hb) in stamps {
        let h = classify(now_ms, ev, hb, deaf_ceiling_ms);
        let replace = match &best {
            None => true,
            // Strictly worse replaces; equal severity keeps first (stable).
            Some((w, _, _)) => pipe_severity(h) > pipe_severity(*w),
        };
        if replace {
            best = Some((h, ev, hb));
        }
    }
    best.unwrap_or((PipeHealth::NeverStamped, None, None))
}

/// GS-062: shared `/health` hosted order — alive-first, then stable alphabetical
/// by `sort_key` (typically the workspace slug).
///
/// Flywheel `serve` and doorbell operator `/health` list hosted in this order.
/// Wake **class** aggregation is ask-user F [`classify_worst_of`] over the
/// **alive** partition only — never fall through to `other` when alive is
/// empty (orphan stamps → false Quiet). Empty alive → [`PipeHealth::NeverStamped`].
pub fn order_hosted_alive_first(
    alive: Vec<String>,
    other: Vec<String>,
) -> Vec<String> {
    order_hosted_alive_first_by(alive, other, |s| s)
}

/// Like [`order_hosted_alive_first`], but compare via `sort_key` (e.g. workspace
/// slug extracted from `project/workspace`). Tie-break on the full string.
pub fn order_hosted_alive_first_by<F>(
    mut alive: Vec<String>,
    mut other: Vec<String>,
    sort_key: F,
) -> Vec<String>
where
    F: Fn(&str) -> &str,
{
    let cmp = |a: &String, b: &String| {
        sort_key(a)
            .cmp(sort_key(b))
            .then_with(|| a.as_str().cmp(b.as_str()))
    };
    alive.sort_by(cmp);
    other.sort_by(cmp);
    alive.extend(other);
    alive
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_hosted_alive_first_alive_then_alpha() {
        let got = order_hosted_alive_first(
            vec!["z-alive".into(), "a-alive".into()],
            vec!["m-dead".into(), "b-dead".into()],
        );
        assert_eq!(
            got,
            vec![
                "a-alive".to_string(),
                "z-alive".to_string(),
                "b-dead".to_string(),
                "m-dead".to_string(),
            ]
        );
    }

    #[test]
    fn order_hosted_alive_first_by_slug_not_full_key() {
        // project/workspace keys: alpha slug must win over project alpha.
        let got = order_hosted_alive_first_by(
            vec!["zzz/alpha".into(), "aaa/beta".into()],
            Vec::new(),
            |k| k.rsplit_once('/').map(|(_, ws)| ws).unwrap_or(k),
        );
        assert_eq!(
            got,
            vec!["zzz/alpha".to_string(), "aaa/beta".to_string()],
            "slug alpha before beta even when project zzz > aaa"
        );
    }


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

    #[test]
    fn worst_of_severity_order_never_gt_deaf_gt_quiet_gt_live() {
        assert!(pipe_severity(PipeHealth::NeverStamped) > pipe_severity(PipeHealth::Deaf { silent_for_ms: 1 }));
        assert!(
            pipe_severity(PipeHealth::Deaf { silent_for_ms: 1 })
                > pipe_severity(PipeHealth::Quiet {
                    last_heartbeat_ms: 1
                })
        );
        assert!(
            pipe_severity(PipeHealth::Quiet {
                last_heartbeat_ms: 1
            }) > pipe_severity(PipeHealth::Live {
                last_event_ms: 1
            })
        );
    }

    #[test]
    fn worst_of_deaf_plus_live_is_deaf() {
        // Multi-hosted: one Deaf + one Live → wake=Deaf (not Live/primary).
        let now = 200_000u64;
        let ceiling = 60_000u64;
        let stamps = [
            (None, Some(100_000u64)),       // Deaf (hb age 100s)
            (Some(199_000u64), Some(198_000u64)), // Live
        ];
        let h = classify_worst_of(now, stamps, ceiling);
        assert_eq!(
            h,
            PipeHealth::Deaf {
                silent_for_ms: 100_000
            },
            "Deaf must win over Live: {h:?}"
        );
    }

    #[test]
    fn worst_of_never_stamped_wins_over_live() {
        let now = 50_000u64;
        let ceiling = 60_000u64;
        let stamps = [
            (None, None),                         // NeverStamped
            (Some(49_000u64), Some(48_000u64)), // Live
        ];
        assert_eq!(
            classify_worst_of(now, stamps, ceiling),
            PipeHealth::NeverStamped
        );
    }

    #[test]
    fn worst_of_empty_is_never_stamped() {
        let empty: [(Option<u64>, Option<u64>); 0] = [];
        assert_eq!(
            classify_worst_of(0, empty, 60_000),
            PipeHealth::NeverStamped
        );
    }

    #[test]
    fn worst_of_does_not_max_merge_stamps_into_live() {
        // Max-merging stamps would yield Live; worst-of must stay Deaf.
        let now = 200_000u64;
        let h = classify_worst_of(
            now,
            [
                (Some(100_000u64), None),             // Deaf
                (Some(199_000u64), Some(198_000u64)), // Live
            ],
            60_000,
        );
        assert!(matches!(h, PipeHealth::Deaf { .. }), "got {h:?}");
    }
}

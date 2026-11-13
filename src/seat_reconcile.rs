//! Pure seat reconciler (ADR-0022 D7).
//!
//! Diffs desired seats against live seats and picks the cheapest effect the
//! adapter allows. Precedence on one seat: harness beats model, beats effort,
//! beats role; any respawn wins over relaunch, wins over inject.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::seat_binder::{LiveSeat, SeatPlacement};

/// Desired seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredSeat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    pub role: String,
    pub harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default)]
    pub placement: SeatPlacement,
    #[serde(default)]
    pub context_warm_allowed: bool,
}

/// Seat effect, deterministic order by role then pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "kebab-case")]
pub enum SeatEffect {
    Keep {
        pin: String,
    },
    Relaunch {
        pin: String,
        fields: Vec<String>,
    },
    Respawn {
        pin: String,
        fields: Vec<String>,
        context_fresh: bool,
    },
    InjectRole {
        pin: String,
        role: String,
    },
    Spawn {
        role: String,
        count: usize,
    },
    Retire {
        pins: Vec<String>,
    },
    RetireAfterTurn {
        pin: String,
    },
}

fn diff_fields(desired: &DesiredSeat, live: &LiveSeat) -> Vec<String> {
    let mut fields = Vec::new();
    if desired.harness != live.harness {
        fields.push("harness".to_string());
    }
    if desired.model != live.model {
        fields.push("model".to_string());
    }
    if desired.effort != live.effort {
        fields.push("effort".to_string());
    }
    if desired.mode != live.mode {
        fields.push("mode".to_string());
    }
    fields
}

fn same_except_role(desired: &DesiredSeat, live: &LiveSeat) -> bool {
    desired.harness == live.harness
        && desired.model == live.model
        && desired.effort == live.effort
        && desired.mode == live.mode
        && desired.placement == live.placement
}

fn decide_one(
    desired: &DesiredSeat,
    live: &LiveSeat,
    model_by_env: &dyn Fn(&str) -> bool,
) -> SeatEffect {
    let pin = live.pin.clone();
    if desired.role != live.role {
        if same_except_role(desired, live) {
            if desired.context_warm_allowed {
                return SeatEffect::InjectRole {
                    pin,
                    role: desired.role.clone(),
                };
            }
            return SeatEffect::Respawn {
                pin,
                fields: vec!["role".to_string()],
                context_fresh: true,
            };
        }
        return SeatEffect::Respawn {
            pin,
            fields: {
                let mut f = vec!["role".to_string()];
                for extra in diff_fields(desired, live) {
                    if extra != "role" && !f.contains(&extra) {
                        f.push(extra);
                    }
                }
                f
            },
            context_fresh: true,
        };
    }
    if desired.harness != live.harness {
        let mut fields = vec!["harness".to_string()];
        for extra in diff_fields(desired, live) {
            if extra != "harness" {
                fields.push(extra);
            }
        }
        return SeatEffect::Respawn {
            pin,
            fields,
            context_fresh: true,
        };
    }
    let model_diff = desired.model != live.model;
    let effort_diff = desired.effort != live.effort;
    let mode_diff = desired.mode != live.mode;
    if model_diff {
        let mut fields = vec!["model".to_string()];
        if effort_diff {
            fields.push("effort".to_string());
        }
        if mode_diff {
            fields.push("mode".to_string());
        }
        if model_by_env(&live.harness) {
            return SeatEffect::Respawn {
                pin,
                fields,
                context_fresh: true,
            };
        }
        return SeatEffect::Relaunch { pin, fields };
    }
    if effort_diff || mode_diff {
        let mut fields = Vec::new();
        if effort_diff {
            fields.push("effort".to_string());
        }
        if mode_diff {
            fields.push("mode".to_string());
        }
        return SeatEffect::Relaunch { pin, fields };
    }
    if desired.placement == SeatPlacement::Headless && live.placement == SeatPlacement::Pane {
        return SeatEffect::RetireAfterTurn { pin };
    }
    SeatEffect::Keep { pin }
}

/// Pure fold: diff desired against live, cheapest effect the adapter allows.
/// Deterministic by role then pin; never borrows another role.
pub fn reconcile(
    desired: &[DesiredSeat],
    live: &[LiveSeat],
    model_by_env: &dyn Fn(&str) -> bool,
) -> Vec<SeatEffect> {
    let mut live_by_pin: BTreeMap<&str, &LiveSeat> = BTreeMap::new();
    for seat in live {
        live_by_pin.insert(seat.pin.as_str(), seat);
    }
    let mut used_live: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut effects: Vec<(String, String, SeatEffect)> = Vec::new();

    let mut desired_no_pin_by_role: BTreeMap<String, Vec<&DesiredSeat>> = BTreeMap::new();
    for want in desired {
        if let Some(pin) = &want.pin {
            if let Some(got) = live_by_pin.get(pin.as_str()) {
                used_live.insert(pin.clone());
                let effect = decide_one(want, got, model_by_env);
                effects.push((want.role.clone(), pin.clone(), effect));
            } else {
                effects.push((
                    want.role.clone(),
                    pin.clone(),
                    SeatEffect::Spawn {
                        role: want.role.clone(),
                        count: 1,
                    },
                ));
            }
        } else {
            desired_no_pin_by_role
                .entry(want.role.clone())
                .or_default()
                .push(want);
        }
    }

    let mut live_unmatched_by_role: BTreeMap<String, Vec<&LiveSeat>> = BTreeMap::new();
    for seat in live {
        if !used_live.contains(&seat.pin) {
            live_unmatched_by_role
                .entry(seat.role.clone())
                .or_default()
                .push(seat);
        }
    }
    for seats in live_unmatched_by_role.values_mut() {
        seats.sort_by(|a, b| a.pin.cmp(&b.pin));
    }

    let mut roles: Vec<String> = desired_no_pin_by_role
        .keys()
        .chain(live_unmatched_by_role.keys())
        .cloned()
        .collect();
    roles.sort();
    roles.dedup();

    for role in roles {
        let wants = desired_no_pin_by_role
            .get(&role)
            .cloned()
            .unwrap_or_default();
        let mut haves = live_unmatched_by_role
            .get(&role)
            .cloned()
            .unwrap_or_default();
        haves.sort_by(|a, b| a.pin.cmp(&b.pin));
        if haves.len() > wants.len() {
            let mut ordered = haves.clone();
            ordered.sort_by(|a, b| {
                (if a.idle { 0 } else { 1 }, &a.pin).cmp(&(if b.idle { 0 } else { 1 }, &b.pin))
            });
            let n = haves.len() - wants.len();
            let retire_pins: std::collections::HashSet<String> =
                ordered.iter().take(n).map(|s| s.pin.clone()).collect();
            let mut remaining: Vec<&LiveSeat> = haves
                .iter()
                .filter(|s| !retire_pins.contains(&s.pin))
                .copied()
                .collect();
            remaining.sort_by(|a, b| a.pin.cmp(&b.pin));
            for i in 0..wants.len() {
                let effect = decide_one(wants[i], remaining[i], model_by_env);
                effects.push((role.clone(), remaining[i].pin.clone(), effect));
                used_live.insert(remaining[i].pin.clone());
            }
            let mut pins: Vec<String> = retire_pins.into_iter().collect();
            pins.sort();
            effects.push((
                role.clone(),
                pins.first().cloned().unwrap_or_default(),
                SeatEffect::Retire { pins },
            ));
            continue;
        }
        let paired = wants.len().min(haves.len());
        for i in 0..paired {
            let effect = decide_one(wants[i], haves[i], model_by_env);
            effects.push((role.clone(), haves[i].pin.clone(), effect));
            used_live.insert(haves[i].pin.clone());
        }
        if wants.len() > haves.len() {
            effects.push((
                role.clone(),
                String::new(),
                SeatEffect::Spawn {
                    role: role.clone(),
                    count: wants.len() - haves.len(),
                },
            ));
        }
    }

    effects.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    effects.into_iter().map(|(_, _, e)| e).collect()
}

/// Thin JSON edge for the `__reconcile-seats` probe.
#[derive(Debug, serde::Deserialize)]
struct ReconcileRequest {
    desired: Vec<DesiredSeat>,
    live: Vec<LiveSeat>,
}

pub fn reconcile_json(input: &str) -> anyhow::Result<String> {
    let req: ReconcileRequest = serde_json::from_str(input)?;
    let by_env = |harness: &str| {
        matches!(
            crate::headless::interactive_model_plan(harness, "", None),
            crate::headless::InteractiveModelPlan::PinnedByEnv { .. }
        )
    };
    let effects = reconcile(&req.desired, &req.live, &by_env);
    Ok(serde_json::to_string(&effects)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seat_binder::{LiveSeat, SeatPlacement};

    fn live(pin: &str, role: &str, harness: &str, idle: bool) -> LiveSeat {
        LiveSeat {
            pin: pin.to_string(),
            role: role.to_string(),
            harness: harness.to_string(),
            model: None,
            mode: None,
            effort: None,
            placement: SeatPlacement::Pane,
            idle,
            fresh_session: false,
        }
    }

    fn want(role: &str) -> DesiredSeat {
        DesiredSeat {
            pin: None,
            role: role.to_string(),
            harness: "h".to_string(),
            model: None,
            effort: None,
            mode: None,
            placement: SeatPlacement::Pane,
            context_warm_allowed: true,
        }
    }

    fn argv(_: &str) -> bool {
        false
    }

    fn env(_: &str) -> bool {
        true
    }

    #[test]
    fn seat_reconcile_no_change_gives_keep() {
        let live = vec![live("a", "coder", "h", true)];
        let mut want = want("coder");
        want.harness = "h".to_string();
        assert_eq!(
            reconcile(&[want], &live, &argv),
            vec![SeatEffect::Keep {
                pin: "a".to_string()
            }]
        );
    }

    #[test]
    fn seat_reconcile_argv_model_gives_relaunch() {
        let mut live = live("a", "coder", "pi", true);
        live.model = Some("m1".to_string());
        let mut want = want("coder");
        want.harness = "pi".to_string();
        want.model = Some("m2".to_string());
        assert_eq!(
            reconcile(&[want], &[live], &argv),
            vec![SeatEffect::Relaunch {
                pin: "a".to_string(),
                fields: vec!["model".to_string()]
            }]
        );
    }

    #[test]
    fn seat_reconcile_env_model_gives_respawn_fresh() {
        let mut live = live("a", "coder", "opencode", true);
        live.model = Some("m1".to_string());
        let mut want = want("coder");
        want.harness = "opencode".to_string();
        want.model = Some("m2".to_string());
        assert_eq!(
            reconcile(&[want], &[live], &env),
            vec![SeatEffect::Respawn {
                pin: "a".to_string(),
                fields: vec!["model".to_string()],
                context_fresh: true
            }]
        );
    }

    #[test]
    fn seat_reconcile_harness_change_gives_respawn() {
        let live = live("a", "coder", "pi", true);
        let mut want = want("coder");
        want.harness = "opencode".to_string();
        assert_eq!(
            reconcile(&[want], &[live], &argv),
            vec![SeatEffect::Respawn {
                pin: "a".to_string(),
                fields: vec!["harness".to_string()],
                context_fresh: true
            }]
        );
    }

    #[test]
    fn seat_reconcile_role_inject_when_warm_allowed() {
        let live = live("a", "coder", "h", true);
        let mut want = want("reviewer");
        want.harness = "h".to_string();
        want.pin = Some("a".to_string());
        want.context_warm_allowed = true;
        assert_eq!(
            reconcile(&[want], &[live], &argv),
            vec![SeatEffect::InjectRole {
                pin: "a".to_string(),
                role: "reviewer".to_string()
            }]
        );
    }

    #[test]
    fn seat_reconcile_role_respawns_when_warm_not_allowed() {
        let live = live("a", "coder", "h", true);
        let mut want = want("reviewer");
        want.harness = "h".to_string();
        want.pin = Some("a".to_string());
        want.context_warm_allowed = false;
        assert_eq!(
            reconcile(&[want], &[live], &argv),
            vec![SeatEffect::Respawn {
                pin: "a".to_string(),
                fields: vec!["role".to_string()],
                context_fresh: true
            }]
        );
    }

    #[test]
    fn seat_reconcile_count_plus_two_spawns_two() {
        let live = vec![live("a", "coder", "h", true)];
        let wants = vec![want("coder"), want("coder"), want("coder")];
        let got = reconcile(&wants, &live, &argv);
        assert!(got.iter().any(|e| matches!(
            e,
            SeatEffect::Spawn { role, count } if role == "coder" && *count == 2
        )));
    }

    #[test]
    fn seat_reconcile_count_minus_one_retires_idle_not_busy() {
        let live = vec![
            live("a", "coder", "h", false),
            live("b", "coder", "h", true),
        ];
        let wants = vec![want("coder")];
        assert_eq!(
            reconcile(&wants, &live, &argv),
            vec![
                SeatEffect::Keep {
                    pin: "a".to_string()
                },
                SeatEffect::Retire {
                    pins: vec!["b".to_string()]
                }
            ]
        );
    }

    #[test]
    fn seat_reconcile_count_minus_one_retires_idle_even_when_idle_pin_sorts_first() {
        let live = vec![
            live("a", "coder", "h", true),
            live("b", "coder", "h", false),
        ];
        let wants = vec![want("coder")];
        assert_eq!(
            reconcile(&wants, &live, &argv),
            vec![
                SeatEffect::Retire {
                    pins: vec!["a".to_string()]
                },
                SeatEffect::Keep {
                    pin: "b".to_string()
                }
            ]
        );
    }

    #[test]
    fn seat_reconcile_pane_to_headless_gives_retire_after_turn() {
        let mut got = live("a", "coder", "h", true);
        got.placement = SeatPlacement::Pane;
        let mut want = want("coder");
        want.harness = "h".to_string();
        want.placement = SeatPlacement::Headless;
        assert_eq!(
            reconcile(&[want], &[got], &argv),
            vec![SeatEffect::RetireAfterTurn {
                pin: "a".to_string()
            }]
        );
    }

    #[test]
    fn seat_reconcile_precedence_harness_model_one_respawn() {
        let mut live = live("a", "coder", "pi", true);
        live.model = Some("m1".to_string());
        let mut want = want("coder");
        want.harness = "opencode".to_string();
        want.model = Some("m2".to_string());
        let got = reconcile(&[want], &[live], &argv);
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], SeatEffect::Respawn { .. }));
    }

    #[test]
    fn seat_reconcile_order_deterministic_shuffled() {
        let live = vec![live("b", "coder", "h", true), live("a", "coder", "h", true)];
        let wants = vec![want("coder"), want("coder")];
        let ab = reconcile(&wants, &live, &argv);
        let ba = reconcile(&wants, &vec![live[1].clone(), live[0].clone()], &argv);
        assert_eq!(ab, ba);
    }

    #[test]
    fn seat_reconcile_json_round_trip() {
        let live = vec![live("a", "coder", "h", true)];
        let mut want = want("coder");
        want.harness = "h".to_string();
        let input = serde_json::json!({"desired": [want], "live": live}).to_string();
        let out = reconcile_json(&input).expect("binds");
        let effects: Vec<SeatEffect> = serde_json::from_str(&out).expect("parses");
        assert_eq!(
            effects,
            vec![SeatEffect::Keep {
                pin: "a".to_string()
            }]
        );
    }

    #[test]
    fn seat_reconcile_json_malformed_errors() {
        assert!(reconcile_json("not json").is_err());
        assert!(reconcile_json("{\"desired\":[]}").is_err());
    }
}

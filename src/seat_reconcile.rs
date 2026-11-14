//! Pure seat reconciler (ADR-0022 D7).
//!
//! Diffs desired seats against live seats and picks the cheapest effect the
//! adapter allows. Precedence on one seat: harness beats model, beats effort,
//! beats role; any respawn wins over relaunch, wins over inject.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::parse_idle_retire_after;
use crate::kernel::{SeatHarness, SeatRole};
use crate::seat_binder::{LiveSeat, SeatPlacement};

/// One field a seat effect rewrites (law 11: a closed typed set; the wire
/// spelling is the old lowercase string, so JSON is byte-identical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeatField {
    Harness,
    Model,
    Effort,
    Mode,
    Role,
}

impl SeatField {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::Model => "model",
            Self::Effort => "effort",
            Self::Mode => "mode",
            Self::Role => "role",
        }
    }
}

/// Desired seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredSeat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    pub role: SeatRole,
    pub harness: SeatHarness,
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
        fields: Vec<SeatField>,
        /// The desired binding the relaunch lands (ADR-0022 D7): the
        /// interpreter needs the target, not just the field names.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<DesiredSeat>,
    },
    Respawn {
        pin: String,
        fields: Vec<SeatField>,
        context_fresh: bool,
        /// The desired binding the respawn lands.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<DesiredSeat>,
    },
    InjectRole {
        pin: String,
        role: SeatRole,
    },
    Spawn {
        role: SeatRole,
        count: usize,
    },
    Retire {
        pins: Vec<String>,
    },
    RetireAfterTurn {
        pin: String,
    },
    /// Idle past `idle_retire_after`: pin and reservations kept (SeatMachine
    /// `IdleTimeout` edge). Distinct from count-driven `Retire` so the
    /// journal says why.
    RetireIdle {
        pin: String,
    },
}

/// Review/verify roles are `context:fresh` by rule (ADR-0021 D3): a seat
/// used for them must not inherit the context of the work it checks, so a
/// role-only change respawns instead of injecting. Role names are the proxy
/// the plan tier has (the registry carries no stage contract).
#[must_use]
pub fn role_is_fresh(role: &SeatRole) -> bool {
    matches!(
        role.as_str().trim().to_ascii_lowercase().as_str(),
        "reviewer" | "verifier" | "review" | "verify"
    )
}

fn diff_fields(desired: &DesiredSeat, live: &LiveSeat) -> Vec<SeatField> {
    let mut fields = Vec::new();
    if desired.harness != live.harness {
        fields.push(SeatField::Harness);
    }
    if desired.model != live.model {
        fields.push(SeatField::Model);
    }
    if desired.effort != live.effort {
        fields.push(SeatField::Effort);
    }
    if desired.mode != live.mode {
        fields.push(SeatField::Mode);
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

/// Policy for idle retirement (ADR-0022 D7). The clock comes in through
/// `now_ms`; the reconciler never reads it itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcilePolicy {
    /// Retire an idle seat this long idle, in ms. `None` (absent) = never.
    pub idle_retire_after_ms: Option<u64>,
    /// Wall clock, ms since epoch, supplied by the edge.
    pub now_ms: u64,
}

fn idle_retire_due(seat: &LiveSeat, policy: &ReconcilePolicy) -> bool {
    match (policy.idle_retire_after_ms, seat.idle_since_ms) {
        (Some(after), Some(since)) => seat.idle && policy.now_ms.saturating_sub(since) >= after,
        _ => false,
    }
}

/// Idle retirement keeps pin and reservations (SeatMachine `IdleTimeout`
/// edge), unlike count-driven `Retire`. The plan wins: a seat that would
/// otherwise retire-idle is kept when retiring it would leave its role with
/// zero live seats while the plan still wants that role.
fn apply_idle_retire(
    effect: SeatEffect,
    seat: &LiveSeat,
    role: &str,
    policy: &ReconcilePolicy,
    desired_count: &BTreeMap<&str, usize>,
    live_count: &BTreeMap<&str, usize>,
    retiring_count: &mut BTreeMap<String, usize>,
) -> SeatEffect {
    if !matches!(effect, SeatEffect::Keep { .. }) || !idle_retire_due(seat, policy) {
        return effect;
    }
    let wanted = desired_count.get(role).copied().unwrap_or(0);
    let remaining = live_count
        .get(role)
        .copied()
        .unwrap_or(0)
        .saturating_sub(retiring_count.get(role).copied().unwrap_or(0));
    if wanted > 0 && remaining <= 1 {
        return effect;
    }
    *retiring_count.entry(role.to_string()).or_default() += 1;
    match effect {
        SeatEffect::Keep { pin } => SeatEffect::RetireIdle { pin },
        other => other,
    }
}

/// Env-pin reroute decision (ADR-0022 D7 acceptance #5): the current pane
/// (with its env pin) against the next profile's harness/model/effort.
/// Decided by the reconciler, never a parallel rule: a different env-pinned
/// model respawns (the caller keeps the pin), the same model is Keep
/// (today's AlreadyPinned path), and an argv-delivered model relaunches,
/// never respawns. `model_by_env` is the harness's own delivery rule (env
/// pin vs argv). A single pinned seat always reconciles to exactly one
/// effect; anything else falls back to Keep (no hop) rather than inventing
/// an answer.
pub fn envpin_reroute_effect(
    current: &LiveSeat,
    next: &DesiredSeat,
    model_by_env: &dyn Fn(&str) -> bool,
) -> SeatEffect {
    match reconcile(
        std::slice::from_ref(next),
        std::slice::from_ref(current),
        model_by_env,
        &ReconcilePolicy::default(),
    )
    .as_slice()
    {
        [effect] => effect.clone(),
        _ => SeatEffect::Keep {
            pin: current.pin.clone(),
        },
    }
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
                fields: vec![SeatField::Role],
                context_fresh: true,
                to: Some(desired.clone()),
            };
        }
        return SeatEffect::Respawn {
            pin,
            fields: {
                let mut f = vec![SeatField::Role];
                for extra in diff_fields(desired, live) {
                    if extra != SeatField::Role && !f.contains(&extra) {
                        f.push(extra);
                    }
                }
                f
            },
            context_fresh: true,
            to: Some(desired.clone()),
        };
    }
    if desired.harness != live.harness {
        let mut fields = vec![SeatField::Harness];
        for extra in diff_fields(desired, live) {
            if extra != SeatField::Harness {
                fields.push(extra);
            }
        }
        return SeatEffect::Respawn {
            pin,
            fields,
            context_fresh: true,
            to: Some(desired.clone()),
        };
    }
    let model_diff = desired.model != live.model;
    let effort_diff = desired.effort != live.effort;
    let mode_diff = desired.mode != live.mode;
    if model_diff {
        let mut fields = vec![SeatField::Model];
        if effort_diff {
            fields.push(SeatField::Effort);
        }
        if mode_diff {
            fields.push(SeatField::Mode);
        }
        if model_by_env(live.harness.as_str()) {
            return SeatEffect::Respawn {
                pin,
                fields,
                context_fresh: true,
                to: Some(desired.clone()),
            };
        }
        return SeatEffect::Relaunch {
            pin,
            fields,
            to: Some(desired.clone()),
        };
    }
    if effort_diff || mode_diff {
        let mut fields = Vec::new();
        if effort_diff {
            fields.push(SeatField::Effort);
        }
        if mode_diff {
            fields.push(SeatField::Mode);
        }
        return SeatEffect::Relaunch {
            pin,
            fields,
            to: Some(desired.clone()),
        };
    }
    if desired.placement == SeatPlacement::Headless && live.placement == SeatPlacement::Pane {
        return SeatEffect::RetireAfterTurn { pin };
    }
    SeatEffect::Keep { pin }
}

/// Pure fold: diff desired against live, cheapest effect the adapter allows.
/// Deterministic by role then pin; never borrows another role.
///
/// Idle retirement (ADR-0022 D7): a seat that would otherwise be `Keep`
/// becomes `RetireIdle` when it is idle and `now_ms - idle_since_ms >=
/// `idle_retire_after_ms`. The plan wins: the conversion is skipped when it
/// would leave the seat's role with zero live seats while desired still
/// wants that role (pin and reservations are kept either way).
pub fn reconcile(
    desired: &[DesiredSeat],
    live: &[LiveSeat],
    model_by_env: &dyn Fn(&str) -> bool,
    policy: &ReconcilePolicy,
) -> Vec<SeatEffect> {
    let mut live_by_pin: BTreeMap<&str, &LiveSeat> = BTreeMap::new();
    for seat in live {
        live_by_pin.insert(seat.pin.as_str(), seat);
    }
    let mut used_live: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut desired_count: BTreeMap<&str, usize> = BTreeMap::new();
    for want in desired {
        *desired_count.entry(want.role.as_str()).or_default() += 1;
    }
    let mut live_count: BTreeMap<&str, usize> = BTreeMap::new();
    for seat in live {
        *live_count.entry(seat.role.as_str()).or_default() += 1;
    }
    let mut retiring_count: BTreeMap<String, usize> = BTreeMap::new();
    let mut effects: Vec<(String, String, SeatEffect)> = Vec::new();

    let mut desired_no_pin_by_role: BTreeMap<String, Vec<&DesiredSeat>> = BTreeMap::new();
    for want in desired {
        if let Some(pin) = &want.pin {
            if let Some(got) = live_by_pin.get(pin.as_str()) {
                used_live.insert(pin.clone());
                let effect = apply_idle_retire(
                    decide_one(want, got, model_by_env),
                    got,
                    want.role.as_str(),
                    policy,
                    &desired_count,
                    &live_count,
                    &mut retiring_count,
                );
                effects.push((want.role.as_str().to_string(), pin.clone(), effect));
            } else {
                effects.push((
                    want.role.as_str().to_string(),
                    pin.clone(),
                    SeatEffect::Spawn {
                        role: want.role.clone(),
                        count: 1,
                    },
                ));
            }
        } else {
            desired_no_pin_by_role
                .entry(want.role.as_str().to_string())
                .or_default()
                .push(want);
        }
    }

    let mut live_unmatched_by_role: BTreeMap<String, Vec<&LiveSeat>> = BTreeMap::new();
    for seat in live {
        if !used_live.contains(&seat.pin) {
            live_unmatched_by_role
                .entry(seat.role.as_str().to_string())
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
                let effect = apply_idle_retire(
                    decide_one(wants[i], remaining[i], model_by_env),
                    remaining[i],
                    role.as_str(),
                    policy,
                    &desired_count,
                    &live_count,
                    &mut retiring_count,
                );
                effects.push((role.clone(), remaining[i].pin.clone(), effect));
                used_live.insert(remaining[i].pin.clone());
            }
            let mut pins: Vec<String> = retire_pins.into_iter().collect();
            pins.sort();
            *retiring_count.entry(role.clone()).or_default() += pins.len();
            effects.push((
                role.clone(),
                pins.first().cloned().unwrap_or_default(),
                SeatEffect::Retire { pins },
            ));
            continue;
        }
        let paired = wants.len().min(haves.len());
        for i in 0..paired {
            let effect = apply_idle_retire(
                decide_one(wants[i], haves[i], model_by_env),
                haves[i],
                role.as_str(),
                policy,
                &desired_count,
                &live_count,
                &mut retiring_count,
            );
            effects.push((role.clone(), haves[i].pin.clone(), effect));
            used_live.insert(haves[i].pin.clone());
        }
        if wants.len() > haves.len() {
            effects.push((
                role.clone(),
                String::new(),
                SeatEffect::Spawn {
                    role: role.as_str().into(),
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
    #[serde(default)]
    now_ms: Option<u64>,
    #[serde(default)]
    idle_retire_after: Option<String>,
}

pub fn reconcile_json(input: &str, now_ms: u64) -> anyhow::Result<String> {
    let req: ReconcileRequest = serde_json::from_str(input)?;
    let by_env = |harness: &str| {
        matches!(
            crate::headless::interactive_model_plan(harness, "", None),
            crate::headless::InteractiveModelPlan::PinnedByEnv { .. }
        )
    };
    let after_ms = req
        .idle_retire_after
        .as_deref()
        .map(parse_idle_retire_after)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let policy = ReconcilePolicy {
        idle_retire_after_ms: after_ms.map(|d| d.as_millis() as u64),
        now_ms: req.now_ms.unwrap_or(now_ms),
    };
    let effects = reconcile(&req.desired, &req.live, &by_env, &policy);
    Ok(serde_json::to_string(&effects)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seat_binder::{LiveSeat, SeatPlacement};

    fn live(pin: &str, role: &str, harness: &str, idle: bool) -> LiveSeat {
        LiveSeat {
            pin: pin.to_string(),
            role: role.to_string().into(),
            harness: harness.to_string().into(),
            model: None,
            mode: None,
            effort: None,
            placement: SeatPlacement::Pane,
            idle,
            fresh_session: false,
            idle_since_ms: None,
        }
    }

    fn want(role: &str) -> DesiredSeat {
        DesiredSeat {
            pin: None,
            role: role.to_string().into(),
            harness: "h".to_string().into(),
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
        want.harness = "h".into();
        assert_eq!(
            reconcile(&[want.clone()], &live, &argv, &ReconcilePolicy::default()),
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
        want.harness = "pi".into();
        want.model = Some("m2".to_string());
        assert_eq!(
            reconcile(&[want.clone()], &[live], &argv, &ReconcilePolicy::default()),
            vec![SeatEffect::Relaunch {
                pin: "a".to_string(),
                fields: vec![SeatField::Model],
                to: Some(want),
            }]
        );
    }

    #[test]
    fn seat_reconcile_env_model_gives_respawn_fresh() {
        let mut live = live("a", "coder", "opencode", true);
        live.model = Some("m1".to_string());
        let mut want = want("coder");
        want.harness = "opencode".into();
        want.model = Some("m2".to_string());
        assert_eq!(
            reconcile(&[want.clone()], &[live], &env, &ReconcilePolicy::default()),
            vec![SeatEffect::Respawn {
                pin: "a".to_string(),
                fields: vec![SeatField::Model],
                context_fresh: true,
                to: Some(want),
            }]
        );
    }

    #[test]
    fn seat_reconcile_harness_change_gives_respawn() {
        let live = live("a", "coder", "pi", true);
        let mut want = want("coder");
        want.harness = "opencode".into();
        assert_eq!(
            reconcile(&[want.clone()], &[live], &argv, &ReconcilePolicy::default()),
            vec![SeatEffect::Respawn {
                pin: "a".to_string(),
                fields: vec![SeatField::Harness],
                context_fresh: true,
                to: Some(want),
            }]
        );
    }

    #[test]
    fn seat_reconcile_role_inject_when_warm_allowed() {
        let live = live("a", "coder", "h", true);
        let mut want = want("reviewer");
        want.harness = "h".into();
        want.pin = Some("a".to_string());
        want.context_warm_allowed = true;
        assert_eq!(
            reconcile(&[want.clone()], &[live], &argv, &ReconcilePolicy::default()),
            vec![SeatEffect::InjectRole {
                pin: "a".to_string(),
                role: "reviewer".into()
            }]
        );
    }

    #[test]
    fn seat_reconcile_role_respawns_when_warm_not_allowed() {
        let live = live("a", "coder", "h", true);
        let mut want = want("reviewer");
        want.harness = "h".into();
        want.pin = Some("a".to_string());
        want.context_warm_allowed = false;
        assert_eq!(
            reconcile(&[want.clone()], &[live], &argv, &ReconcilePolicy::default()),
            vec![SeatEffect::Respawn {
                pin: "a".to_string(),
                fields: vec![SeatField::Role],
                context_fresh: true,
                to: Some(want),
            }]
        );
    }

    #[test]
    fn seat_reconcile_count_plus_two_spawns_two() {
        let live = vec![live("a", "coder", "h", true)];
        let wants = vec![want("coder"), want("coder"), want("coder")];
        let got = reconcile(&wants, &live, &argv, &ReconcilePolicy::default());
        assert!(got.iter().any(|e| matches!(
            e,
            SeatEffect::Spawn { role, count } if role.as_str() == "coder" && *count == 2
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
            reconcile(&wants, &live, &argv, &ReconcilePolicy::default()),
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
            reconcile(&wants, &live, &argv, &ReconcilePolicy::default()),
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
        want.harness = "h".into();
        want.placement = SeatPlacement::Headless;
        assert_eq!(
            reconcile(&[want], &[got], &argv, &ReconcilePolicy::default()),
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
        want.harness = "opencode".into();
        want.model = Some("m2".to_string());
        let got = reconcile(&[want.clone()], &[live], &argv, &ReconcilePolicy::default());
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0], SeatEffect::Respawn { .. }));
    }

    #[test]
    fn seat_reconcile_order_deterministic_shuffled() {
        let live = vec![live("b", "coder", "h", true), live("a", "coder", "h", true)];
        let wants = vec![want("coder"), want("coder")];
        let ab = reconcile(&wants, &live, &argv, &ReconcilePolicy::default());
        let ba = reconcile(
            &wants,
            &[live[1].clone(), live[0].clone()],
            &argv,
            &ReconcilePolicy::default(),
        );
        assert_eq!(ab, ba);
    }

    /// Law 11 wire proof: the typed seat vocabulary serializes to the SAME
    /// bytes the string fields did. A fixture parses and re-serializes
    /// byte-identically.
    #[test]
    fn seat_vocabulary_serde_is_byte_identical() {
        let desired = DesiredSeat {
            pin: Some("a".into()),
            role: "reviewer".into(),
            harness: "opencode".into(),
            model: Some("m".into()),
            effort: Some("high".into()),
            mode: Some("read-only".into()),
            placement: SeatPlacement::Pane,
            context_warm_allowed: true,
        };
        let want = r#"{"pin":"a","role":"reviewer","harness":"opencode","model":"m","effort":"high","mode":"read-only","placement":"pane","context_warm_allowed":true}"#;
        assert_eq!(serde_json::to_string(&desired).expect("serializes"), want);
        let back: DesiredSeat = serde_json::from_str(want).expect("parses");
        assert_eq!(back, desired);

        let live = crate::seat_binder::LiveSeat {
            pin: "a".into(),
            role: "coder".into(),
            harness: "pi".into(),
            model: Some("m1".into()),
            mode: None,
            effort: None,
            placement: SeatPlacement::Pane,
            idle: false,
            fresh_session: false,
            idle_since_ms: None,
        };
        let live_want = r#"{"pin":"a","role":"coder","harness":"pi","model":"m1","mode":null,"placement":"pane","idle":false,"fresh_session":false}"#;
        assert_eq!(serde_json::to_string(&live).expect("serializes"), live_want);

        let effect = SeatEffect::Relaunch {
            pin: "a".into(),
            fields: vec![SeatField::Model, SeatField::Effort],
            to: None,
        };
        assert_eq!(
            serde_json::to_string(&effect).expect("serializes"),
            r#"{"effect":"relaunch","pin":"a","fields":["model","effort"]}"#
        );
    }

    #[test]
    fn seat_reconcile_json_round_trip() {
        let live = vec![live("a", "coder", "h", true)];
        let mut want = want("coder");
        want.harness = "h".into();
        let input = serde_json::json!({"desired": [want], "live": live}).to_string();
        let out = reconcile_json(&input, 0).expect("binds");
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
        assert!(reconcile_json("not json", 0).is_err());
        assert!(reconcile_json("{\"desired\":[]}", 0).is_err());
    }

    fn envpin_live(pin: &str, model: Option<&str>) -> LiveSeat {
        LiveSeat {
            pin: pin.into(),
            role: "coder".into(),
            harness: "opencode".into(),
            model: model.map(str::to_string),
            mode: None,
            effort: None,
            placement: SeatPlacement::Pane,
            idle: false,
            fresh_session: false,
            idle_since_ms: None,
        }
    }

    fn envpin_want(model: Option<&str>) -> DesiredSeat {
        DesiredSeat {
            pin: Some("a".into()),
            role: "coder".into(),
            harness: "opencode".into(),
            model: model.map(str::to_string),
            effort: None,
            mode: None,
            placement: SeatPlacement::Pane,
            context_warm_allowed: true,
        }
    }

    #[test]
    fn envpin_respawn_env_model_gives_respawn() {
        let live = envpin_live("a", Some("opencode/gold"));
        let want = envpin_want(Some("opencode/new"));
        let got = envpin_reroute_effect(&live, &want, &|_| true);
        assert!(
            matches!(got, SeatEffect::Respawn { ref pin, .. } if pin == "a"),
            "different env-pinned model respawns, keeps pin: {got:?}"
        );
    }

    #[test]
    fn envpin_respawn_same_model_gives_keep() {
        let live = envpin_live("a", Some("opencode/gold"));
        let want = envpin_want(Some("opencode/gold"));
        let got = envpin_reroute_effect(&live, &want, &|_| true);
        assert_eq!(
            got,
            SeatEffect::Keep {
                pin: "a".to_string()
            }
        );
    }

    #[test]
    fn envpin_respawn_argv_model_gives_relaunch() {
        let live = envpin_live("a", Some("opencode/gold"));
        let want = envpin_want(Some("opencode/new"));
        let got = envpin_reroute_effect(&live, &want, &|_| false);
        assert!(
            matches!(got, SeatEffect::Relaunch { ref pin, .. } if pin == "a"),
            "argv-delivered model relaunches, never respawns: {got:?}"
        );
    }

    fn v9_gen_live(n: usize, reverse: bool) -> Vec<LiveSeat> {
        const PINS: [&str; 8] = ["a", "b", "c", "d", "e", "f", "g", "h"];
        const ROLES: [&str; 3] = ["coder", "reviewer", "planner"];
        const HARNESSES: [&str; 3] = ["pi", "opencode", "grok"];
        const MODELS: [Option<&str>; 3] = [None, Some("m1"), Some("m2")];
        const EFFORTS: [Option<&str>; 2] = [None, Some("max")];
        const MODES: [Option<&str>; 2] = [None, Some("read-only")];
        let mut live: Vec<LiveSeat> = (0..n)
            .map(|i| LiveSeat {
                pin: PINS[i].into(),
                role: ROLES[i % 3].into(),
                harness: HARNESSES[i % 3].into(),
                model: MODELS[i % 3].map(str::to_string),
                mode: MODES[i % 2].map(str::to_string),
                effort: EFFORTS[i % 2].map(str::to_string),
                placement: if i % 5 == 4 {
                    SeatPlacement::Headless
                } else {
                    SeatPlacement::Pane
                },
                idle: i % 2 == 0,
                fresh_session: false,
                idle_since_ms: None,
            })
            .collect();
        if reverse {
            live.reverse();
        }
        live
    }

    fn v9_sorted(live: &[LiveSeat]) -> Vec<LiveSeat> {
        let mut sorted = live.to_vec();
        sorted.sort_by(|a, b| a.pin.cmp(&b.pin));
        sorted
    }

    fn v9_want_from(live: &LiveSeat) -> DesiredSeat {
        DesiredSeat {
            pin: Some(live.pin.clone()),
            role: live.role.clone(),
            harness: live.harness.clone(),
            model: live.model.clone(),
            effort: live.effort.clone(),
            mode: live.mode.clone(),
            placement: live.placement,
            context_warm_allowed: true,
        }
    }

    #[test]
    fn seat_reconcile_v9_generated_idempotent_keep_all() {
        let mut cases = 0;
        for n in 0..=8usize {
            for reverse in [false, true] {
                let live = v9_gen_live(n, reverse);
                let canon = v9_sorted(&live);
                for env in [false, true] {
                    let by_env = move |_: &str| env;
                    let want_pinned: Vec<DesiredSeat> = live.iter().map(v9_want_from).collect();
                    let got = reconcile(&want_pinned, &live, &by_env, &ReconcilePolicy::default());
                    assert_eq!(
                        got.len(),
                        n,
                        "one effect per seat (pinned n={n} rev={reverse} env={env})"
                    );
                    assert!(
                        got.iter().all(|e| matches!(e, SeatEffect::Keep { .. })),
                        "pinned idempotent Keeps only (n={n} rev={reverse} env={env}): {got:?}"
                    );
                    cases += 1;
                    let want_unpinned: Vec<DesiredSeat> = canon
                        .iter()
                        .map(|s| {
                            let mut w = v9_want_from(s);
                            w.pin = None;
                            w
                        })
                        .collect();
                    let got =
                        reconcile(&want_unpinned, &live, &by_env, &ReconcilePolicy::default());
                    assert_eq!(
                        got.len(),
                        n,
                        "one effect per seat (unpinned n={n} rev={reverse} env={env})"
                    );
                    assert!(
                        got.iter().all(|e| matches!(e, SeatEffect::Keep { .. })),
                        "unpinned idempotent Keeps only (n={n} rev={reverse} env={env}): {got:?}"
                    );
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 72, "9 sizes x 2 orders x 2 env x pinned/unpinned");
    }

    #[test]
    fn seat_reconcile_v9_generated_converse_one_non_keep() {
        let mut cases = 0;
        for n in 1..=8usize {
            let live = v9_gen_live(n, false);
            let mut want: Vec<DesiredSeat> = live.iter().map(v9_want_from).collect();
            if want[0].model.is_none() {
                want[0].model = Some("mX".into());
            } else {
                want[0].model = None;
            }
            assert_ne!(want[0].model, live[0].model);
            let got = reconcile(&want, &live, &|_: &str| false, &ReconcilePolicy::default());
            let non_keep: Vec<&SeatEffect> = got
                .iter()
                .filter(|e| !matches!(e, SeatEffect::Keep { .. }))
                .collect();
            assert_eq!(non_keep.len(), 1, "exactly one non-Keep (n={n}): {got:?}");
            cases += 1;
        }
        assert_eq!(cases, 8);
    }

    fn idle_live(pin: &str, idle: bool, idle_since_ms: Option<u64>) -> LiveSeat {
        LiveSeat {
            pin: pin.into(),
            role: "coder".into(),
            harness: "pi".into(),
            model: None,
            mode: None,
            effort: None,
            placement: SeatPlacement::Pane,
            idle,
            fresh_session: false,
            idle_since_ms,
        }
    }

    fn idle_policy(after_ms: Option<u64>, now_ms: u64) -> ReconcilePolicy {
        ReconcilePolicy {
            idle_retire_after_ms: after_ms,
            now_ms,
        }
    }

    fn idle_want(pin: &str) -> DesiredSeat {
        DesiredSeat {
            pin: Some(pin.into()),
            role: "coder".into(),
            harness: "pi".into(),
            model: None,
            effort: None,
            mode: None,
            placement: SeatPlacement::Pane,
            context_warm_allowed: true,
        }
    }

    #[test]
    fn idle_retire_past_threshold_gives_retire_idle() {
        let live = vec![
            idle_live("a", true, Some(0)),
            idle_live("b", false, Some(0)),
        ];
        let want = vec![idle_want("a"), idle_want("b")];
        let got = reconcile(
            &want,
            &live,
            &|_: &str| false,
            &idle_policy(Some(600_000), 600_001),
        );
        assert_eq!(
            got,
            vec![
                SeatEffect::RetireIdle {
                    pin: "a".to_string()
                },
                SeatEffect::Keep {
                    pin: "b".to_string()
                },
            ]
        );
    }

    #[test]
    fn idle_retire_at_threshold_retires() {
        let live = vec![
            idle_live("a", true, Some(1)),
            idle_live("b", false, Some(0)),
        ];
        let want = vec![idle_want("a"), idle_want("b")];
        let got = reconcile(&want, &live, &|_: &str| false, &idle_policy(Some(600), 601));
        assert_eq!(
            got,
            vec![
                SeatEffect::RetireIdle {
                    pin: "a".to_string()
                },
                SeatEffect::Keep {
                    pin: "b".to_string()
                },
            ]
        );
    }

    #[test]
    fn idle_retire_below_threshold_keeps() {
        let live = vec![idle_live("a", true, Some(600_000))];
        let want = vec![idle_want("a")];
        let got = reconcile(
            &want,
            &live,
            &|_: &str| false,
            &idle_policy(Some(600_000), 600_001),
        );
        assert_eq!(
            got,
            vec![SeatEffect::Keep {
                pin: "a".to_string()
            }]
        );
    }

    #[test]
    fn idle_retire_busy_keeps_despite_idle_time() {
        let live = vec![
            idle_live("a", false, Some(0)),
            idle_live("b", false, Some(0)),
        ];
        let want = vec![idle_want("a"), idle_want("b")];
        let got = reconcile(
            &want,
            &live,
            &|_: &str| false,
            &idle_policy(Some(600_000), 600_001),
        );
        assert_eq!(
            got,
            vec![
                SeatEffect::Keep {
                    pin: "a".to_string()
                },
                SeatEffect::Keep {
                    pin: "b".to_string()
                },
            ]
        );
    }

    #[test]
    fn idle_retire_absent_config_never_retires() {
        let live = vec![idle_live("a", true, Some(0))];
        let want = vec![idle_want("a")];
        let got = reconcile(&want, &live, &|_: &str| false, &idle_policy(None, 999_999));
        assert_eq!(
            got,
            vec![SeatEffect::Keep {
                pin: "a".to_string()
            }]
        );
    }

    #[test]
    fn idle_retire_plan_wanted_role_kept_despite_idle() {
        let live = vec![idle_live("a", true, Some(0))];
        let want = vec![idle_want("a")];
        let got = reconcile(&want, &live, &|_: &str| false, &idle_policy(Some(0), 1));
        assert_eq!(
            got,
            vec![SeatEffect::Keep {
                pin: "a".to_string()
            }],
            "plan still wants the role: keep, never strand it"
        );
    }

    #[test]
    fn idle_retire_invalid_config_fails_parse() {
        assert!(parse_idle_retire_after("ten minutes").is_err());
        assert_eq!(
            parse_idle_retire_after("10m").expect("valid duration"),
            std::time::Duration::from_secs(600)
        );
        let rfc3339 = parse_idle_retire_after("2026-08-18T00:00:00Z");
        assert!(rfc3339.is_err(), "RFC3339 is not a duration");
        assert!(
            rfc3339.unwrap_err().contains("idle_retire_after"),
            "rejection names the field"
        );
        let last = parse_idle_retire_after("last");
        assert!(last.is_err(), "`last` is not a duration");
        assert!(
            last.unwrap_err().contains("idle_retire_after"),
            "rejection names the field"
        );
    }

    #[test]
    fn idle_retire_probe_json_round_trip() {
        let live = vec![
            idle_live("a", true, Some(0)),
            idle_live("b", false, Some(0)),
        ];
        let want = vec![idle_want("a"), idle_want("b")];
        let input = serde_json::json!({
            "desired": want,
            "live": live,
            "now_ms": 600_001u64,
            "idle_retire_after": "10m",
        })
        .to_string();
        let out = reconcile_json(&input, 0).expect("probe binds");
        let effects: Vec<SeatEffect> = serde_json::from_str(&out).expect("parses");
        assert_eq!(
            effects,
            vec![
                SeatEffect::RetireIdle {
                    pin: "a".to_string()
                },
                SeatEffect::Keep {
                    pin: "b".to_string()
                },
            ]
        );
    }

    #[test]
    fn idle_retire_json_without_now_uses_caller_clock() {
        let live = vec![
            idle_live("a", true, Some(0)),
            idle_live("b", false, Some(0)),
        ];
        let want = vec![idle_want("a"), idle_want("b")];
        let input = serde_json::json!({
            "desired": want,
            "live": live,
            "idle_retire_after": "10m",
        })
        .to_string();
        assert!(!input.contains("now_ms"), "request omits now_ms");
        let out = reconcile_json(&input, 600_001).expect("probe binds");
        let effects: Vec<SeatEffect> = serde_json::from_str(&out).expect("parses");
        assert_eq!(
            effects,
            vec![
                SeatEffect::RetireIdle {
                    pin: "a".to_string()
                },
                SeatEffect::Keep {
                    pin: "b".to_string()
                },
            ]
        );
    }
}

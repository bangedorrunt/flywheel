//! `flywheel-plan.v1` — see the truth before anything launches.
//!
//! Flywheel ADR-0022 phase 1 ("see the truth"): before any pane exists, the
//! captain sees per seat and per field what was asked for, who asked
//! (provenance), and whether the harness will actually receive it. No launch
//! behaviour changes here; phase 2 (adapters) turns blocked rows real.
//!
//! PURE: no I/O, no env, no clock. Delivery truth comes from
//! [`crate::headless::interactive_model_plan`], the same function the real
//! launch path uses, so a status cannot disagree with the argv/env.

use serde::{Deserialize, Serialize};

/// Who asked for a field value (flywheel ADR-0022 D2).
///
/// Only `User`, `Profile`, `Slot`, and `HarnessDefault` are produced in this
/// phase. `Preference`, `Stage`, and `Jev` exist so the resolver's later
/// phases (consent, binder) share one type; [`field_status`] already handles
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provenance {
    User,
    Preference,
    Stage,
    Profile,
    Slot,
    Jev,
    HarnessDefault,
    /// The step's own ask (ADR-0021 D3.7): beats stage, profile, everything.
    Step,
    /// The value came from a `replaced_by` retirement hop; `Field.was`
    /// carries the asked id.
    Replacement,
}

/// Whether the harness will actually receive a requested field (D3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum FieldStatus {
    Honored,
    Degraded { reason: String },
    Blocked { remedy: String },
}

/// One requested value with its provenance and delivery status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub value: String,
    pub from: Provenance,
    pub status: FieldStatus,
    /// Registry membership marker: `Some("unregistered")` when the ask
    /// matched nothing; absent when registered. Serialized only when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// The asked id when `from` is `Replacement` (retirement hop).
    /// Serialized only when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub was: Option<String>,
}

/// One seat of the plan: a role × count on one harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seat {
    pub role: String,
    pub count: u32,
    pub harness: String,
    pub model: Option<Field>,
    pub effort: Option<Field>,
    pub mode: Option<Field>,
    pub phase: Option<Field>,
    /// Where the seat runs. Phase 1 only knows `"pane"`.
    pub placement: String,
}

/// The whole plan: `flywheel-plan.v1`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub schema: String,
    pub mode: String,
    pub seats: Vec<Seat>,
    /// ADR-0022 D5: computed consent set. Additive to `flywheel-plan.v1`:
    /// absent on the wire reads as empty.
    #[serde(default)]
    pub needs_consent: Vec<ConsentItem>,
    /// ADR-0022 D5: effective consent policy for this plan. Additive and
    /// optional: absent on the wire reads as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ConsentPolicy>,
}

impl Plan {
    /// An empty plan in the given mode (`"open" | "swarm"`).
    #[must_use]
    pub fn new(mode: &str) -> Self {
        Self {
            schema: "flywheel-plan.v1".to_string(),
            mode: mode.to_string(),
            seats: Vec::new(),
            needs_consent: Vec::new(),
            policy: None,
        }
    }
}

/// ADR-0022 D5: when a spawn must ask instead of launching silently.
/// `on-change` (default) asks only about Jev-touched, blocked, and
/// over-budget/over-cap rows; `always` lists every seat's model and
/// harness; `never` lists only blocked rows (blocked never launches anyway).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ConsentAsk {
    #[default]
    OnChange,
    Always,
    Never,
}

/// ADR-0022 D5: why one consent item needs the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsentReason {
    Jev,
    Blocked,
    Unregistered,
    Budget,
    CrewMax,
    Policy,
}

impl ConsentReason {
    /// Wire spelling for refusal lines (`crew-max`, not `CrewMax`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jev => "jev",
            Self::Blocked => "blocked",
            Self::Unregistered => "unregistered",
            Self::Budget => "budget",
            Self::CrewMax => "crew-max",
            Self::Policy => "policy",
        }
    }
}

/// One question for the operator: this seat's field needs sign-off before
/// launch (or the aggregate row does, for `budget` / `crew-max`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentItem {
    pub seat: String,
    pub field: String,
    pub reason: ConsentReason,
}

/// Default budget threshold (USD per mtok) when `[consent]` is absent.
pub const DEFAULT_BUDGET_USD_PER_MTOK: f64 = 5.0;

/// ADR-0022 D5 consent policy: the `[consent]` ask mode and the budget
/// threshold the summed estimate is weighed against. Serialized with the plan
/// so the effective policy is on the wire next to the consent it produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsentPolicy {
    #[serde(default)]
    pub ask: ConsentAsk,
    #[serde(default = "default_budget_usd_per_mtok")]
    pub budget_usd_per_mtok: f64,
}

fn default_budget_usd_per_mtok() -> f64 {
    DEFAULT_BUDGET_USD_PER_MTOK
}

impl Default for ConsentPolicy {
    fn default() -> Self {
        Self {
            ask: ConsentAsk::OnChange,
            budget_usd_per_mtok: DEFAULT_BUDGET_USD_PER_MTOK,
        }
    }
}

/// ADR-0022 D5: compute the consent set. Pure: no I/O, no env, no clock.
/// Rule 2 (blocked) fires under every policy; unregistered asks surface as
/// `unregistered` (they need sign-off but are not delivery-blocked, so
/// `--yes` launches them); rule 1 (any field with `from: jev`) fires
/// on-change; `always` lists every seat's model and harness as `policy`;
/// `never` lists only rule 2. Rule 3 (estimate over budget, seat total over
/// `crew_max`) fires under every policy except `never`.
#[must_use]
pub fn needs_consent(
    plan: &Plan,
    policy: &ConsentPolicy,
    est_usd_per_mtok: Option<f64>,
    crew_max: u32,
) -> Vec<ConsentItem> {
    use std::collections::BTreeSet;
    let mut out = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    fn push(
        out: &mut Vec<ConsentItem>,
        seen: &mut BTreeSet<(String, String)>,
        seat: &str,
        field: &str,
        reason: ConsentReason,
    ) {
        if seen.insert((seat.to_string(), field.to_string())) {
            out.push(ConsentItem {
                seat: seat.to_string(),
                field: field.to_string(),
                reason,
            });
        }
    }
    fn fields(seat: &Seat) -> [(&'static str, &Option<Field>); 4] {
        [
            ("model", &seat.model),
            ("effort", &seat.effort),
            ("mode", &seat.mode),
            ("phase", &seat.phase),
        ]
    }
    // Rule 2 first so a blocked field keeps its reason under `always`.
    for seat in &plan.seats {
        for (name, field) in fields(seat) {
            let Some(f) = field else { continue };
            if matches!(f.status, FieldStatus::Blocked { .. }) {
                push(
                    &mut out,
                    &mut seen,
                    &seat.role,
                    name,
                    ConsentReason::Blocked,
                );
            } else if f.registry.as_deref() == Some("unregistered") {
                push(
                    &mut out,
                    &mut seen,
                    &seat.role,
                    name,
                    ConsentReason::Unregistered,
                );
            }
        }
    }
    match policy.ask {
        ConsentAsk::Never => {}
        ConsentAsk::OnChange => {
            for seat in &plan.seats {
                for (name, field) in fields(seat) {
                    if field.as_ref().is_some_and(|f| f.from == Provenance::Jev) {
                        push(&mut out, &mut seen, &seat.role, name, ConsentReason::Jev);
                    }
                }
            }
        }
        ConsentAsk::Always => {
            for seat in &plan.seats {
                if seat.model.is_some() {
                    push(
                        &mut out,
                        &mut seen,
                        &seat.role,
                        "model",
                        ConsentReason::Policy,
                    );
                }
                push(
                    &mut out,
                    &mut seen,
                    &seat.role,
                    "harness",
                    ConsentReason::Policy,
                );
            }
        }
    }
    if !matches!(policy.ask, ConsentAsk::Never) {
        if est_usd_per_mtok.is_some_and(|est| est > policy.budget_usd_per_mtok) {
            push(&mut out, &mut seen, "crew", "budget", ConsentReason::Budget);
        }
        let total: u32 = plan.seats.iter().map(|s| s.count).sum();
        if total > crew_max {
            push(&mut out, &mut seen, "crew", "count", ConsentReason::CrewMax);
        }
    }
    out
}

/// ADR-0022 D5: summed per-mtok estimate over seats — `count *`
/// the registered `cost_in_per_mtok` of each seat's resolved model. `None`
/// when any seat's model is unregistered or costs 0, so rule 3 budget never
/// fires on unknowns. Pure; reads the registry passed in.
#[must_use]
pub fn estimate_usd_per_mtok(plan: &Plan, registry: &crate::models::ModelRegistry) -> Option<f64> {
    let mut total = 0.0;
    for seat in &plan.seats {
        let Some(field) = &seat.model else { continue };
        if field.registry.as_deref() == Some("unregistered") {
            return None;
        }
        let entry = registry.get(&field.value)?;
        if entry.cost_in_per_mtok <= 0.0 {
            return None;
        }
        total += seat.count as f64 * entry.cost_in_per_mtok;
    }
    Some(total)
}

/// D3 exactly: delivered => honored; not delivered and from user/preference =>
/// blocked with a remedy naming the harness and field; otherwise degraded.
#[must_use]
pub fn field_status(delivered: bool, from: Provenance, harness: &str, field: &str) -> FieldStatus {
    if delivered {
        return FieldStatus::Honored;
    }
    match from {
        Provenance::User | Provenance::Preference => FieldStatus::Blocked {
            remedy: format!(
                "harness {harness:?} takes no {field} flag — ask for a model it honours or pick another harness"
            ),
        },
        _ => FieldStatus::Degraded {
            reason: format!(
                "harness {harness:?} has no native {field} flag; the seat runs without it"
            ),
        },
    }
}

/// Delivery truth for one seat's model and effort against the launch plan the
/// real spawn path computes: a model is delivered iff its string is in the
/// launch argv or equals the env pin; an effort is delivered iff its string
/// is in the launch argv.
#[must_use]
pub fn seat_status(
    harness: &str,
    model: Option<(String, Provenance)>,
    effort: Option<(String, Provenance)>,
    launch: &crate::headless::InteractiveModelPlan,
) -> (Option<Field>, Option<Field>) {
    let model_field = model.map(|(value, from)| {
        let delivered =
            launch.args().iter().any(|a| a == &value) || launch.env_pin() == Some(value.as_str());
        Field {
            status: field_status(delivered, from, harness, "model"),
            value,
            from,
            registry: None,
            was: None,
        }
    });
    let effort_field = effort.map(|(value, from)| {
        let delivered = launch.args().iter().any(|a| a == &value);
        Field {
            status: field_status(delivered, from, harness, "effort"),
            value,
            from,
            registry: None,
            was: None,
        }
    });
    (model_field, effort_field)
}

/// Delivery truth for one seat's mode against the adapter's mode argv:
/// honored iff the adapter emits argv for it. Downgraded to blocked by
/// provenance (user/preference asks) in [`field_status`].
#[must_use]
pub fn mode_field(harness: &str, mode: Option<(String, Provenance)>) -> Option<Field> {
    mode.map(|(value, from)| {
        let (args, delivery) = crate::headless::mode_launch_args(harness, &value);
        let delivered =
            !args.is_empty() && matches!(delivery, crate::headless::ModeDelivery::Honored);
        Field {
            status: field_status(delivered, from, harness, "mode"),
            value,
            from,
            registry: None,
            was: None,
        }
    })
}

/// Delivery truth for one seat's phase against the adapter's phase argv:
/// honored iff the adapter emits argv for it (phase_launch_args reports
/// degraded=false with args).
#[must_use]
pub fn phase_field(harness: &str, phase: Option<(String, Provenance)>) -> Option<Field> {
    phase.map(|(value, from)| {
        let (args, degraded) = crate::headless::phase_launch_args(harness, &value);
        let delivered = !args.is_empty() && !degraded;
        Field {
            status: field_status(delivered, from, harness, "phase"),
            value,
            from,
            registry: None,
            was: None,
        }
    })
}

/// A resolved model ask as a plan field: honored iff the launch will
/// carry it, with `from` flipped to `Replacement` when a retirement hop
/// produced the value, and `was` carrying the asked id.
#[must_use]
pub fn resolved_field(
    harness: &str,
    value: String,
    from: Provenance,
    delivered: bool,
    registry: Option<String>,
    was: Option<String>,
) -> Field {
    let from = if was.is_some() {
        Provenance::Replacement
    } else {
        from
    };
    Field {
        status: field_status(delivered, from, harness, "model"),
        value,
        from,
        registry,
        was,
    }
}

/// A harness-default ask as a plan field: the harness honours it by
/// construction (nothing is injected), so the row is always `honored`.
#[must_use]
pub fn harness_default_field(from: Provenance) -> Field {
    Field {
        value: crate::models::HARNESS_DEFAULT.to_string(),
        from,
        status: FieldStatus::Honored,
        registry: None,
        was: None,
    }
}

/// An unresolvable model ask as a plan field (ambiguous or retired):
/// carries the ask with the provenance split applied — `blocked` for
/// user/preference asks, `degraded` otherwise.
#[must_use]
pub fn unresolved_field(_harness: &str, ask: &str, from: Provenance, message: String) -> Field {
    let status = match from {
        Provenance::User | Provenance::Preference => FieldStatus::Blocked {
            remedy: message.clone(),
        },
        _ => FieldStatus::Degraded {
            reason: message.clone(),
        },
    };
    Field {
        value: ask.to_string(),
        from,
        status,
        registry: None,
        was: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch_for(
        harness: &str,
        model: &str,
        effort: Option<&str>,
    ) -> crate::headless::InteractiveModelPlan {
        crate::headless::interactive_model_plan(harness, model, effort)
    }

    #[test]
    fn field_status_table() {
        // Delivered always honours, whatever asked.
        for from in [
            Provenance::User,
            Provenance::Preference,
            Provenance::Stage,
            Provenance::Profile,
            Provenance::Slot,
            Provenance::Jev,
            Provenance::HarnessDefault,
        ] {
            assert_eq!(
                field_status(true, from, "gemini", "model"),
                FieldStatus::Honored,
                "delivered must honour (from={from:?})"
            );
        }
        // Undelivered user/preference asks block, and the remedy names both.
        for from in [Provenance::User, Provenance::Preference] {
            match field_status(false, from, "gemini", "model") {
                FieldStatus::Blocked { remedy } => {
                    assert!(remedy.contains("gemini"), "remedy names harness: {remedy}");
                    assert!(remedy.contains("model"), "remedy names field: {remedy}");
                }
                other => panic!("user ask must block, got {other:?}"),
            }
        }
        // Undelivered stage/profile/slot/jev/default degrades.
        for from in [
            Provenance::Stage,
            Provenance::Profile,
            Provenance::Slot,
            Provenance::Jev,
            Provenance::HarnessDefault,
        ] {
            assert!(
                matches!(
                    field_status(false, from, "grok", "effort"),
                    FieldStatus::Degraded { .. }
                ),
                "non-user ask must degrade (from={from:?})"
            );
        }
    }

    #[test]
    fn gemini_model_from_user_is_honored() {
        let launch = launch_for("gemini", "gemini-2.5-pro", None);
        assert_eq!(
            launch.args(),
            &["-m".to_string(), "gemini-2.5-pro".to_string()],
            "phase 2a: gemini delivers the model as -m"
        );
        let (model, _) = seat_status(
            "gemini",
            Some(("gemini-2.5-pro".to_string(), Provenance::User)),
            None,
            &launch,
        );
        let field = model.expect("model field present");
        assert_eq!(field.from, Provenance::User);
        assert_eq!(
            field.status,
            FieldStatus::Honored,
            "phase 2a: gemini takes a model flag: {field:?}"
        );
    }

    #[test]
    fn fx_model_from_user_is_blocked() {
        let launch = launch_for("fx", "fx/whatever", None);
        assert!(launch.args().is_empty(), "fx has no model flag by policy");
        let (model, _) = seat_status(
            "fx",
            Some(("fx/whatever".to_string(), Provenance::User)),
            None,
            &launch,
        );
        let field = model.expect("model field present");
        assert!(
            matches!(field.status, FieldStatus::Blocked { .. }),
            "fx takes no model flag: {field:?}"
        );
    }

    #[test]
    fn pi_model_from_user_is_honored() {
        let launch = launch_for("pi", "zai/glm-5.3", None);
        let (model, _) = seat_status(
            "pi",
            Some(("zai/glm-5.3".to_string(), Provenance::User)),
            None,
            &launch,
        );
        let field = model.expect("model field present");
        assert_eq!(field.status, FieldStatus::Honored);
        assert_eq!(field.from, Provenance::User);
    }

    #[test]
    fn opencode_model_rides_env_and_is_honored() {
        let launch = launch_for("opencode", "zai/glm-5.3", None);
        assert!(launch.args().is_empty(), "opencode emits no model argv");
        assert_eq!(launch.env_pin(), Some("zai/glm-5.3"));
        let (model, _) = seat_status(
            "opencode",
            Some(("zai/glm-5.3".to_string(), Provenance::Slot)),
            None,
            &launch,
        );
        assert_eq!(
            model.expect("model field present").status,
            FieldStatus::Honored
        );
    }

    #[test]
    fn grok_effort_is_honored_from_profile_and_user() {
        // Phase 2a: grok delivers effort via --reasoning-effort, so a
        // requested effort is honored whatever the provenance.
        let launch = launch_for("grok", "grok/grok-4.5", Some("high"));
        assert!(
            launch.args().iter().any(|a| a == "high"),
            "grok emits the effort flag now: {:?}",
            launch.args()
        );
        for (from, label) in [(Provenance::Profile, "profile"), (Provenance::User, "user")] {
            let (_, effort) = seat_status(
                "grok",
                Some(("grok/grok-4.5".to_string(), Provenance::HarnessDefault)),
                Some(("high".to_string(), from)),
                &launch,
            );
            assert_eq!(
                effort.expect("effort present").status,
                FieldStatus::Honored,
                "{label} effort on grok is honored now"
            );
        }
    }

    #[test]
    fn no_model_means_null_field() {
        let launch = launch_for("pi", "", None);
        let (model, effort) = seat_status("pi", None, None, &launch);
        assert!(model.is_none());
        assert!(effort.is_none());
    }

    /// V4 (ADR-0022 §3.4): no false green — for every harness in the registry
    /// and a fixed model and effort, status is honored iff the value appears
    /// in the argv/env the launch plan returns.
    #[test]
    fn v4_status_matches_adapter_truth_for_every_harness() {
        let model = "test/model-1";
        let effort = "high";
        for spec in crate::harness_registry::REGISTRY {
            let harness = spec.canonical;
            let launch = launch_for(harness, model, Some(effort));
            let model_in =
                launch.args().iter().any(|a| a == model) || launch.env_pin() == Some(model);
            let effort_in = launch.args().iter().any(|a| a == effort);
            let (model_field, effort_field) = seat_status(
                harness,
                Some((model.to_string(), Provenance::User)),
                Some((effort.to_string(), Provenance::User)),
                &launch,
            );
            let model_honored =
                model_field.expect("model field present").status == FieldStatus::Honored;
            let effort_honored =
                effort_field.expect("effort present").status == FieldStatus::Honored;
            assert_eq!(
                model_honored, model_in,
                "false green on {harness}: model honored={model_honored} but in argv/env={model_in}"
            );
            assert_eq!(
                effort_honored, effort_in,
                "false green on {harness}: effort honored={effort_honored} but in argv={effort_in}"
            );
        }
    }

    /// The V4 property bites: an always-honored stub fails it on the first
    /// harness that emits nothing (fx), so a broken resolver cannot pass.
    #[test]
    #[should_panic(expected = "false green")]
    fn v4_catches_an_always_honored_stub() {
        let model = "test/model-1";
        for spec in crate::harness_registry::REGISTRY {
            let harness = spec.canonical;
            let launch = launch_for(harness, model, Some("high"));
            let model_in =
                launch.args().iter().any(|a| a == model) || launch.env_pin() == Some(model);
            // Broken variant: status honours whatever was asked.
            let stub_honored = true;
            assert_eq!(
                stub_honored, model_in,
                "false green on {harness}: stub honours what the adapter drops"
            );
        }
    }

    #[test]
    fn status_json_shape() {
        assert_eq!(
            serde_json::to_value(FieldStatus::Honored).unwrap(),
            serde_json::json!({"status": "honored"})
        );
        assert_eq!(
            serde_json::to_value(FieldStatus::Degraded { reason: "r".into() }).unwrap(),
            serde_json::json!({"status": "degraded", "reason": "r"})
        );
        assert_eq!(
            serde_json::to_value(FieldStatus::Blocked { remedy: "m".into() }).unwrap(),
            serde_json::json!({"status": "blocked", "remedy": "m"})
        );
        assert_eq!(
            serde_json::to_value(Provenance::HarnessDefault).unwrap(),
            serde_json::json!("harness-default")
        );
        let plan = Plan::new("swarm");
        assert_eq!(plan.schema, "flywheel-plan.v1");
    }

    #[test]
    fn mode_field_user_readonly_on_fx_is_blocked() {
        let f = mode_field("fx", Some(("read-only".to_string(), Provenance::User)))
            .expect("field present");
        match f.status {
            FieldStatus::Blocked { remedy } => {
                assert!(remedy.contains("fx"), "{remedy}");
                assert!(remedy.contains("mode"), "{remedy}");
            }
            other => panic!("user read-only on fx must block, got {other:?}"),
        }
    }

    #[test]
    fn mode_field_stage_readonly_on_gemini_is_degraded() {
        let f = mode_field("gemini", Some(("read-only".to_string(), Provenance::Stage)))
            .expect("field present");
        assert!(
            matches!(f.status, FieldStatus::Degraded { .. }),
            "stage read-only on gemini degrades: {:?}",
            f.status
        );
    }

    #[test]
    fn phase_field_user_plan_on_grok_is_honored() {
        let f = phase_field("grok", Some(("plan".to_string(), Provenance::User)))
            .expect("field present");
        assert_eq!(f.status, FieldStatus::Honored);
    }

    #[test]
    fn phase_field_plan_on_fx_is_degraded() {
        let f =
            phase_field("fx", Some(("plan".to_string(), Provenance::User))).expect("field present");
        assert!(
            matches!(f.status, FieldStatus::Blocked { .. }),
            "user plan on flag-less fx blocks: {:?}",
            f.status
        );
    }

    /// V4 extended (ADR-0022 §3.4): mode and phase statuses flip only
    /// because the argv changed — honored iff the adapter emits for it.
    #[test]
    fn v4_mode_and_phase_flip_only_with_argv() {
        for spec in crate::harness_registry::REGISTRY {
            let harness = spec.canonical;
            let (args, delivery) = crate::headless::mode_launch_args(harness, "read-only");
            let honored =
                !args.is_empty() && matches!(delivery, crate::headless::ModeDelivery::Honored);
            let f = mode_field(harness, Some(("read-only".to_string(), Provenance::User)))
                .expect("field present");
            assert_eq!(
                f.status == FieldStatus::Honored,
                honored,
                "false green on {harness} mode: {:?} vs argv {args:?}",
                f.status
            );
            let (pargs, pdegraded) = crate::headless::phase_launch_args(harness, "plan");
            let phonored = !pargs.is_empty() && !pdegraded;
            let pf = phase_field(harness, Some(("plan".to_string(), Provenance::User)))
                .expect("field present");
            assert_eq!(
                pf.status == FieldStatus::Honored,
                phonored,
                "false green on {harness} phase: {:?} vs argv {pargs:?}",
                pf.status
            );
        }
    }

    /// The extended V4 bites: an always-honored stub fails on the first
    /// harness with no mode mechanic (fx).
    #[test]
    #[should_panic(expected = "false green")]
    fn v4_mode_catches_an_always_honored_stub() {
        for spec in crate::harness_registry::REGISTRY {
            let harness = spec.canonical;
            let (args, _) = crate::headless::mode_launch_args(harness, "read-only");
            let stub_honored = true;
            assert_eq!(
                stub_honored,
                !args.is_empty(),
                "false green on {harness}: stub honours what the adapter drops"
            );
        }
    }

    #[test]
    fn replacement_shows_from_replacement_and_was() {
        let f = resolved_field(
            "pi",
            "zai/glm-5.3".to_string(),
            Provenance::User,
            true,
            None,
            Some("old/glm".to_string()),
        );
        assert_eq!(f.from, Provenance::Replacement);
        assert_eq!(f.was.as_deref(), Some("old/glm"));
        assert_eq!(f.status, FieldStatus::Honored);
    }

    #[test]
    fn unresolved_ambiguous_user_is_blocked_listing_candidates() {
        let f = unresolved_field(
            "pi",
            "m",
            Provenance::User,
            "ambiguous model `m` — did you mean one of: a/m, b/m".to_string(),
        );
        assert_eq!(f.value, "m");
        match f.status {
            FieldStatus::Blocked { remedy } => {
                assert!(remedy.contains("a/m") && remedy.contains("b/m"), "{remedy}");
            }
            other => panic!("user ambiguity must block, got {other:?}"),
        }
    }

    #[test]
    fn unresolved_from_profile_is_degraded() {
        let f = unresolved_field(
            "pi",
            "m",
            Provenance::Profile,
            "ambiguous model `m`".to_string(),
        );
        assert!(
            matches!(f.status, FieldStatus::Degraded { .. }),
            "{:?}",
            f.status
        );
    }

    #[test]
    fn field_registry_and_was_serialize_only_when_present() {
        let bare = Field {
            value: "x".into(),
            from: Provenance::User,
            status: FieldStatus::Honored,
            registry: None,
            was: None,
        };
        let v = serde_json::to_value(&bare).unwrap();
        assert!(v.get("registry").is_none(), "{v}");
        assert!(v.get("was").is_none(), "{v}");
        let marked = Field {
            value: "x".into(),
            from: Provenance::HarnessDefault,
            status: FieldStatus::Honored,
            registry: Some("unregistered".to_string()),
            was: Some("old".to_string()),
        };
        let v = serde_json::to_value(&marked).unwrap();
        assert_eq!(v["registry"], "unregistered");
        assert_eq!(v["was"], "old");
        assert_eq!(
            serde_json::to_value(Provenance::Replacement).unwrap(),
            serde_json::json!("replacement")
        );
        assert_eq!(
            serde_json::to_value(Provenance::Step).unwrap(),
            serde_json::json!("step")
        );
    }

    fn consent_field(value: &str, from: Provenance, status: FieldStatus) -> Field {
        Field {
            value: value.into(),
            from,
            status,
            registry: None,
            was: None,
        }
    }

    fn consent_seat(role: &str, count: u32, model: Option<Field>) -> Seat {
        Seat {
            role: role.into(),
            count,
            harness: "pi".into(),
            model,
            effort: None,
            mode: None,
            phase: None,
            placement: "pane".into(),
        }
    }

    fn on_change() -> ConsentPolicy {
        ConsentPolicy {
            ask: ConsentAsk::OnChange,
            budget_usd_per_mtok: 5.0,
        }
    }

    #[test]
    fn needs_consent_lists_jev_field() {
        let mut plan = Plan::new("swarm");
        plan.seats.push(consent_seat(
            "coder",
            1,
            Some(consent_field("m", Provenance::Jev, FieldStatus::Honored)),
        ));
        let got = needs_consent(&plan, &on_change(), None, 5);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].seat, "coder");
        assert_eq!(got[0].field, "model");
        assert_eq!(got[0].reason, ConsentReason::Jev);
    }

    #[test]
    fn needs_consent_lists_blocked_field() {
        let mut plan = Plan::new("swarm");
        plan.seats.push(consent_seat(
            "coder",
            1,
            Some(consent_field(
                "m",
                Provenance::User,
                FieldStatus::Blocked {
                    remedy: "pick another harness".into(),
                },
            )),
        ));
        let got = needs_consent(&plan, &on_change(), None, 5);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].reason, ConsentReason::Blocked);
    }

    #[test]
    fn needs_consent_unregistered_marker_lists_unregistered() {
        let mut plan = Plan::new("swarm");
        let mut field = consent_field("m", Provenance::User, FieldStatus::Honored);
        field.registry = Some("unregistered".to_string());
        plan.seats.push(consent_seat("coder", 1, Some(field)));
        let got = needs_consent(&plan, &on_change(), None, 5);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].seat, "coder");
        assert_eq!(got[0].field, "model");
        assert_eq!(got[0].reason, ConsentReason::Unregistered);
    }

    #[test]
    fn needs_consent_over_budget_lists_estimate() {
        let plan = Plan::new("swarm");
        let got = needs_consent(&plan, &on_change(), Some(7.5), 5);
        assert!(got.iter().any(|i| i.reason == ConsentReason::Budget));
        let got = needs_consent(&plan, &on_change(), Some(4.9), 5);
        assert!(got.iter().all(|i| i.reason != ConsentReason::Budget));
        let got = needs_consent(&plan, &on_change(), None, 5);
        assert!(got.iter().all(|i| i.reason != ConsentReason::Budget));
    }

    #[test]
    fn needs_consent_over_crew_max_listed() {
        let mut plan = Plan::new("swarm");
        plan.seats.push(consent_seat("a", 3, None));
        plan.seats.push(consent_seat("b", 3, None));
        let got = needs_consent(&plan, &on_change(), None, 5);
        assert!(got.iter().any(|i| i.reason == ConsentReason::CrewMax));
        let got = needs_consent(&plan, &on_change(), None, 6);
        assert!(got.iter().all(|i| i.reason != ConsentReason::CrewMax));
    }

    #[test]
    fn needs_consent_never_lists_only_blocked() {
        let mut plan = Plan::new("swarm");
        plan.seats.push(consent_seat(
            "coder",
            1,
            Some(consent_field(
                "m",
                Provenance::User,
                FieldStatus::Blocked { remedy: "x".into() },
            )),
        ));
        plan.seats.push(consent_seat(
            "reviewer",
            1,
            Some(consent_field("m", Provenance::Jev, FieldStatus::Honored)),
        ));
        let policy = ConsentPolicy {
            ask: ConsentAsk::Never,
            budget_usd_per_mtok: 5.0,
        };
        let got = needs_consent(&plan, &policy, Some(99.0), 0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].reason, ConsentReason::Blocked);
    }

    #[test]
    fn needs_consent_always_lists_model_and_harness() {
        let mut plan = Plan::new("swarm");
        plan.seats.push(consent_seat(
            "coder",
            1,
            Some(consent_field(
                "m",
                Provenance::Profile,
                FieldStatus::Honored,
            )),
        ));
        let policy = ConsentPolicy {
            ask: ConsentAsk::Always,
            budget_usd_per_mtok: 5.0,
        };
        let got = needs_consent(&plan, &policy, None, 5);
        assert!(
            got.iter().any(|i| i.seat == "coder"
                && i.field == "model"
                && i.reason == ConsentReason::Policy),
            "always lists model as policy: {got:?}"
        );
        assert!(
            got.iter().any(|i| i.seat == "coder"
                && i.field == "harness"
                && i.reason == ConsentReason::Policy),
            "always lists harness as policy: {got:?}"
        );
    }

    #[test]
    fn consent_item_serializes_with_kebab_reason() {
        let item = ConsentItem {
            seat: "coder".into(),
            field: "model".into(),
            reason: ConsentReason::CrewMax,
        };
        assert_eq!(
            serde_json::to_value(&item).unwrap(),
            serde_json::json!({"seat": "coder", "field": "model", "reason": "crew-max"})
        );
        let plan = Plan::new("swarm");
        assert_eq!(
            serde_json::to_value(&plan).unwrap()["needs_consent"],
            serde_json::json!([])
        );
    }

    #[test]
    fn consent_policy_serializes_in_plan() {
        let policy = ConsentPolicy {
            ask: ConsentAsk::Always,
            budget_usd_per_mtok: 5.0,
        };
        let mut plan = Plan::new("swarm");
        plan.policy = Some(policy);
        let v = serde_json::to_value(&plan).unwrap();
        assert_eq!(v["policy"]["ask"], serde_json::json!("always"));
        assert_eq!(v["policy"]["budget_usd_per_mtok"], serde_json::json!(5.0));
        let back: Plan = serde_json::from_value(v).unwrap();
        assert_eq!(
            back.policy.map(|p| p.ask),
            Some(ConsentAsk::Always),
            "the policy round-trips with the plan"
        );
        let legacy: Plan = serde_json::from_value(serde_json::json!({
            "schema": "flywheel-plan.v1",
            "mode": "swarm",
            "seats": []
        }))
        .unwrap();
        assert_eq!(legacy.policy, None);
        assert!(legacy.needs_consent.is_empty());
    }
}

/// ADR-0022 D7/acceptance #3: a parsed `spawn --patch <seat>.<field>=<value>` ask.
/// The seat matches a plan seat's role (seats carry no pins at plan time).
/// The value is always a user ask (`from: user`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPatch {
    /// Seat role the patch targets.
    pub seat: String,
    /// Which field to rewrite.
    pub field: PatchField,
    /// The new value (a user ask).
    pub value: String,
}

/// Fields a patch may rewrite (`mode` included; `phase` arrives with workflows, not asks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchField {
    Harness,
    Model,
    Effort,
    Mode,
    /// Live only (`swarm apply`): change the seat's role. `spawn --patch`
    /// rejects it — a plan seat's role is its identity.
    Role,
}

impl PatchField {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "harness" => Some(Self::Harness),
            "model" => Some(Self::Model),
            "effort" => Some(Self::Effort),
            "mode" => Some(Self::Mode),
            "role" => Some(Self::Role),
            _ => None,
        }
    }

    const VALID: &'static str = "harness|model|effort|mode|role";
}

/// Parse `<seat>.<field>=<value>`; unknown fields name the valid ones.
pub fn parse_patch(s: &str) -> Result<PlanPatch, String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("bad patch {s:?}: want <seat>.<field>=<value>"))?;
    if value.is_empty() {
        return Err(format!("bad patch {s:?}: value must not be empty"));
    }
    let (seat, field) = key
        .rsplit_once('.')
        .ok_or_else(|| format!("bad patch {s:?}: want <seat>.<field>=<value>"))?;
    if seat.is_empty() {
        return Err(format!("bad patch {s:?}: seat must not be empty"));
    }
    let field = PatchField::parse(field)
        .ok_or_else(|| format!("unknown field {field:?} (valid: {})", PatchField::VALID))?;
    Ok(PlanPatch {
        seat: seat.to_string(),
        field,
        value: value.to_string(),
    })
}

/// Reject an unknown seat naming the valid roles. The spawn coverage gate
/// calls this on the plan built from the same overrides it checks.
pub fn validate_patch(plan: &Plan, patch: &PlanPatch) -> Result<(), String> {
    if !plan.seats.iter().any(|s| s.role == patch.seat) {
        let mut names: Vec<&str> = plan.seats.iter().map(|s| s.role.as_str()).collect();
        names.sort();
        names.dedup();
        return Err(format!(
            "unknown seat {:?} (valid: {})",
            patch.seat,
            names.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod plan_patch_tests {
    /// `<seat>.<field>=<value>` parses for every patchable field.
    #[test]
    fn plan_patch_parses_seat_field_value() {
        let p = super::parse_patch("reviewer.harness=pi").expect("parses");
        assert_eq!(p.seat, "reviewer");
        assert!(matches!(p.field, super::PatchField::Harness));
        assert_eq!(p.value, "pi");
        let p = super::parse_patch("coder.model=x").expect("parses");
        assert!(matches!(p.field, super::PatchField::Model));
        assert_eq!(p.value, "x");
        let p = super::parse_patch("a.effort=high").expect("parses");
        assert!(matches!(p.field, super::PatchField::Effort));
        let p = super::parse_patch("b.mode=read-only").expect("parses");
        assert!(matches!(p.field, super::PatchField::Mode));
        let p = super::parse_patch("a.model=x=y").expect("value keeps =");
        assert_eq!(p.value, "x=y");
    }

    /// An unknown field names the valid ones.
    #[test]
    fn plan_patch_rejects_unknown_field() {
        let err = super::parse_patch("coder.bogus=x").expect_err("unknown field fails");
        for f in ["harness", "model", "effort", "mode"] {
            assert!(err.contains(f), "names valid field {f}: {err}");
        }
    }
}

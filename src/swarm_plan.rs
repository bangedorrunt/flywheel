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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub schema: String,
    pub mode: String,
    pub seats: Vec<Seat>,
}

impl Plan {
    /// An empty plan in the given mode (`"open" | "swarm"`).
    #[must_use]
    pub fn new(mode: &str) -> Self {
        Self {
            schema: "flywheel-plan.v1".to_string(),
            mode: mode.to_string(),
            seats: Vec::new(),
        }
    }
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
        }
    });
    let effort_field = effort.map(|(value, from)| {
        let delivered = launch.args().iter().any(|a| a == &value);
        Field {
            status: field_status(delivered, from, harness, "effort"),
            value,
            from,
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
        }
    })
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
}

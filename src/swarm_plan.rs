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
    fn gemini_model_from_user_is_blocked() {
        let launch = launch_for("gemini", "gemini-2.5-pro", None);
        let (model, _) = seat_status(
            "gemini",
            Some(("gemini-2.5-pro".to_string(), Provenance::User)),
            None,
            &launch,
        );
        let field = model.expect("model field present");
        assert_eq!(field.from, Provenance::User);
        assert!(
            matches!(field.status, FieldStatus::Blocked { .. }),
            "gemini takes no model flag: {field:?}"
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
    fn grok_effort_from_profile_degrades_from_user_blocks() {
        let launch = launch_for("grok", "grok/grok-4.5", Some("high"));
        assert!(
            !launch.args().iter().any(|a| a == "high"),
            "grok emits no effort flag today"
        );
        let (_, profile_effort) = seat_status(
            "grok",
            Some(("grok/grok-4.5".to_string(), Provenance::HarnessDefault)),
            Some(("high".to_string(), Provenance::Profile)),
            &launch,
        );
        assert!(
            matches!(
                profile_effort.expect("effort present").status,
                FieldStatus::Degraded { .. }
            ),
            "profile effort on grok degrades"
        );
        let (_, user_effort) = seat_status(
            "grok",
            Some(("grok/grok-4.5".to_string(), Provenance::HarnessDefault)),
            Some(("high".to_string(), Provenance::User)),
            &launch,
        );
        assert!(
            matches!(
                user_effort.expect("effort present").status,
                FieldStatus::Blocked { .. }
            ),
            "user effort on grok blocks"
        );
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
}

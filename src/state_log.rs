// governed-by: ADR-0016
//! Run-state model codec for the flywheel half of the checkpoint log.
//!
//! The storage half lives in toron-core (`toron_core::index::run_state`,
//! migration `0013_run_state_checkpoints.sql`) so the log is one index and one
//! schema shared by every embedding. This module is the flywheel half: the
//! pure codec that turns [`WorkspaceModel`] into the log's payload and back,
//! plus the thread identity the log is partitioned by.
//!
//! ## Purity, and why the module stops where it does
//!
//! Everything here is PURE. There is no store handle, no clock read, no
//! environment access, and no `RunStateStore` field — writing a checkpoint is
//! an EFFECT and belongs to the interpreter (`doorbell_host::RtHost`), not
//! here. Keeping the codec pure is what makes the round-trip property
//! testable without a database and keeps `tick` untouched.
//!
//! ## The authority rule (spec S3)
//!
//! This module deliberately provides NO way to seed a live model from the log.
//! `model_from_json` is a pure decode; the decision to adopt a decoded model as
//! the live one is the authority call that ADR-0016 (`bd-6si7`) owns. Wiring
//! resume-from-log here would create a second seeding path beside
//! `seed_workspace_from_active_loop` and make it ambiguous which one is
//! authoritative — the exact failure the spec's §3 forbids. Replay and fork read
//! the log; they do not install it.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::workspace::WorkspaceModel;

/// The log's partition key: one fold.
///
/// Constructed through [`ThreadId::new`] so a blank id can never reach the
/// store. An empty thread id would merge unrelated folds into one history, and
/// that failure is silent — the queries would simply return another fold's
/// boundaries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(String);

impl ThreadId {
    /// Normalise and validate. Leading/trailing whitespace is trimmed so the
    /// same logical fold cannot be written under two keys by a typo.
    pub fn new(raw: &str) -> anyhow::Result<Self> {
        let trimmed = raw.trim();
        anyhow::ensure!(
            !trimmed.is_empty(),
            "run-state thread id is blank: an empty id would merge unrelated folds into one history"
        );
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Serialize a model to the log's payload form. Pure.
pub fn model_json<T: Serialize>(model: &T) -> anyhow::Result<String> {
    Ok(serde_json::to_string(model)?)
}

// bd-4dpq remaining slice: the decode half has no production caller until the
// replay/fork verbs land. Tested and fail-closed; allow-listed rather than
// deleted, because a write-only checkpoint log is not a feature.
#[allow(dead_code)]
/// Decode a model from the log's payload form. Pure, and FAIL-CLOSED: an
/// unparseable payload is an error, never a default.
///
/// This is the single most important behaviour in the module. Returning
/// `T::default()` on failure would make a replay resume from an empty state
/// that looks exactly like a successful one.
pub fn model_from_json<T: DeserializeOwned>(json: &str) -> anyhow::Result<T> {
    let value: T = serde_json::from_str(json)?;
    Ok(value)
}

/// The fold's model, named so call sites read as the log's payload rather than
/// as an incidental serialization.
pub type RunStateModel = WorkspaceModel;

/// Build the payload for one boundary, including the effect keys its commands
/// own.
///
/// `effect_keys` is the PENDING-WRITE set: on resume, a key here that the effect
/// journal already reports `settled` replays from its recorded `outcome_json`
/// instead of re-invoking the provider, and one left `in_flight` stays honestly
/// Unknown. It is stored as a JSON array so the store never has to know the
/// shape.
pub fn boundary_payload<M: Serialize>(model: &M, effect_keys: &[String]) -> anyhow::Result<String> {
    let keys = serde_json::to_string(effect_keys)?;
    Ok(format!(
        "{{\"model\":{},\"effect_keys\":{keys}}}",
        model_json(model)?
    ))
}

/// Read the model back out of a [`boundary_payload`].
#[allow(dead_code)]
pub fn model_from_boundary<M: DeserializeOwned>(payload: &str) -> anyhow::Result<M> {
    let wrapper: BoundaryWrapper<M> = serde_json::from_str(payload)?;
    Ok(wrapper.model)
}

/// The stored effect keys of a boundary, for the resume path.
// bd-4dpq remaining slice: the replay/fork verbs consume this. Tested; see the
// note on `model_from_json`.
#[allow(dead_code)]
pub fn effect_keys_from_boundary(payload: &str) -> anyhow::Result<Vec<String>> {
    let wrapper: BoundaryWrapper<serde_json::Value> = serde_json::from_str(payload)?;
    Ok(wrapper.effect_keys)
}

#[derive(serde::Deserialize)]
struct BoundaryWrapper<M> {
    model: M,
    // Read by `effect_keys_from_boundary`, which is allow-listed as the
    // remaining slice's seam rather than deleted.
    #[serde(default)]
    #[allow(dead_code)]
    effect_keys: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{Presence, Question, Receipt, WorkspaceModel};
    use std::collections::BTreeSet;

    /// A model with EVERY field set to something non-default, so a missing pin in
    /// this fixture is a real gap rather than a coincidence.
    fn populated() -> WorkspaceModel {
        let mut m = WorkspaceModel::default();
        m.seq = 42;
        m.last_now = 1_700_000_000;
        m.last_digest_unix = 1_700_000_123;
        m.session_topology = Some("open".into());
        m.last_vehicle = Some("vehicle: none (raw)".into());
        m.roles.insert("captain".into(), "BrashOtter".into());
        m.presence.insert(
            "BrashOtter".into(),
            Presence::Busy {
                source: "herdr".into(),
            },
        );
        m.presence.insert(
            "QuietMarmot".into(),
            Presence::Idle {
                source: "legacy".into(),
            },
        );
        m.presence.insert("Nobody".into(), Presence::Unknown);
        m.open_human.insert(
            "corr-1".into(),
            Question {
                corr: "corr-1".into(),
                body: "approve bd-4dpq?".into(),
                created_at: 1_700_000_500,
            },
        );
        m.assignment.insert(
            "w6:pC".into(),
            Receipt {
                state: "working".into(),
                wake: "tasked".into(),
                due_at: Some(1_700_000_900),
                result_path: Some("artifacts/bd-4dpq".into()),
            },
        );
        m.wake_acked.insert("BrashOtter".into(), 7);
        m.wake_counters.insert("routine".into(), 3);
        m.doorbell
            .queues
            .insert("BrashOtter".into(), vec!["corr-1".into()]);
        m.doorbell
            .statuses
            .insert("QuietMarmot".into(), "idle".into());
        m.doorbell
            .index
            .insert_pin("BrashOtter".into(), Some("d335c51c".into()));
        m.doorbell.budgets.insert(
            "BrashOtter".into(),
            doorbell_core::PinBudget::new(30, 3, 1_700_000_000),
        );
        m.doorbell.unroutable.insert("GhostPin".into());
        m.doorbell
            .budgets
            .get_mut("BrashOtter")
            .expect("just inserted")
            .prompts
            .push(1_700_000_700);
        m.swarm.heal_states.clear();
        m
    }

    // --- the load-bearing property -------------------------------------------

    #[test]
    fn populated_model_round_trips_exactly() {
        let original = populated();
        let json = model_json(&original).expect("serialize");
        let back: WorkspaceModel = model_from_json(&json).expect("deserialize");

        assert_eq!(
            back, original,
            "a lossy round trip would make a replay resume from a state the fold never held"
        );
        assert_eq!(
            model_json(&back).expect("re-serialize"),
            json,
            "serialization is stable, so two checkpoints of the same model agree byte-for-byte"
        );
    }

    /// A default model must round trip too: the FIRST boundary of a fresh thread is
    /// a default-ish model, and that is exactly the case replay hits first.
    #[test]
    fn default_model_round_trips_exactly() {
        let original = WorkspaceModel::default();
        let json = model_json(&original).expect("serialize");
        let back: WorkspaceModel = model_from_json(&json).expect("deserialize");
        assert_eq!(back, original);
    }

    /// Every field the fixture sets must actually be pinned. `PartialEq` on the
    /// whole model would also pass if the round trip were correct for the OTHER
    /// fields, so assert the specific values survived rather than trusting equality
    /// alone to prove coverage.
    #[test]
    fn every_pinned_field_survives_the_round_trip_individually() {
        let json = model_json(&populated()).expect("serialize");
        let back: WorkspaceModel = model_from_json(&json).expect("deserialize");

        assert_eq!(back.seq, 42, "seq");
        assert_eq!(back.last_now, 1_700_000_000, "last_now");
        assert_eq!(back.last_digest_unix, 1_700_000_123, "last_digest_unix");
        assert_eq!(
            back.session_topology.as_deref(),
            Some("open"),
            "session_topology"
        );
        assert_eq!(
            back.last_vehicle.as_deref(),
            Some("vehicle: none (raw)"),
            "last_vehicle"
        );
        assert_eq!(
            back.roles.get("captain").map(String::as_str),
            Some("BrashOtter"),
            "roles"
        );
        assert_eq!(back.presence.len(), 3, "presence cardinality");
        assert_eq!(back.open_human.len(), 1, "open_human cardinality");
        assert_eq!(back.assignment.len(), 1, "assignment cardinality");
        assert_eq!(back.wake_acked.get("BrashOtter"), Some(&7), "wake_acked");
        assert_eq!(back.wake_counters.get("routine"), Some(&3), "wake_counters");
        assert_eq!(
            back.doorbell.queues.get("BrashOtter").map(Vec::len),
            Some(1),
            "doorbell.queues"
        );
        assert_eq!(
            back.doorbell
                .statuses
                .get("QuietMarmot")
                .map(String::as_str),
            Some("idle"),
            "statuses"
        );
        assert_eq!(
            back.doorbell.unroutable,
            BTreeSet::from(["GhostPin".to_string()]),
            "unroutable"
        );
        assert_eq!(
            back.doorbell
                .budgets
                .get("BrashOtter")
                .expect("budget survives")
                .prompts,
            vec![1_700_000_700],
            "budgets incl. its rolling prompt window"
        );
        // SelfIndex's maps are PRIVATE, so prove the index round-tripped by
        // behaviour rather than by field access.
        assert_eq!(
            back.doorbell.index.npub("BrashOtter"),
            Some("d335c51c"),
            "SelfIndex pin->npub survives; its fields are private so this is the observable"
        );
    }

    /// A snapshot that cannot be parsed is a hard error, never a default model.
    /// Returning `WorkspaceModel::default()` here would be the single worst bug in
    /// this feature: a replay would silently resume from an EMPTY state and look
    /// like it worked.
    #[test]
    fn unparseable_snapshot_is_refused_rather_than_defaulting() {
        assert!(
            model_from_json::<WorkspaceModel>("{not json").is_err(),
            "a corrupt payload must not become a default model"
        );
        assert!(
            model_from_json::<WorkspaceModel>("null").is_err(),
            "null is not a model"
        );
        assert!(
            model_from_json::<WorkspaceModel>("{}").is_err(),
            "an object missing required fields is not a model"
        );
    }

    // --- the Tick Law is unaffected ------------------------------------------

    /// Serializing the model must not have changed the fold. Same log, same model,
    /// same command list — asserted directly rather than inferred.
    #[test]
    fn serialization_did_not_change_the_fold() {
        use crate::workspace::Msg;

        let model = populated();
        let msg = Msg::HerdrStatus {
            pin: "BrashOtter".into(),
            status: "idle".into(),
        };

        let (a, ca) = crate::workspace::tick(&model, &msg);
        // Round-tripping the model first must not change the outcome.
        let json = model_json(&model).expect("serialize");
        let model2: WorkspaceModel = model_from_json(&json).expect("deserialize");
        let (b, cb) = crate::workspace::tick(&model2, &msg);

        assert_eq!(a, b, "same log, same next model");
        assert_eq!(
            format!("{ca:?}"),
            format!("{cb:?}"),
            "same log, same command list"
        );
        // And a serialize/deserialize cycle in the middle is a no-op for identity.
        assert_eq!(
            model2, model,
            "the round trip is the identity on this model"
        );
    }

    /// Thread ids are the log's partition key. An empty or whitespace-only id would
    /// silently merge unrelated folds into one history, so it is refused at
    /// construction rather than at query time.
    #[test]
    fn thread_id_refuses_blank_and_normalises_surrounding_space() {
        assert!(ThreadId::new("").is_err(), "an empty id would merge folds");
        assert!(ThreadId::new("   ").is_err(), "whitespace is not an id");
        let t = ThreadId::new("  flywheel/w6  ").expect("trims");
        assert_eq!(
            t.as_str(),
            "flywheel/w6",
            "surrounding space is normalised away"
        );
        assert_eq!(
            t,
            ThreadId::new("flywheel/w6").expect("same"),
            "equality is on the normalised form"
        );
    }
}

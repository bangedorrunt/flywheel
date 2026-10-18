//! bd-command-receipts-zile: request-key receipts for mutating doors.
//!
//! A retried mutation has to converge, not repeat. The spawn door already
//! journals its intent before dispatch ([`crate::agent_ref::AgentEffectGuard`],
//! I4) and keys that row by `(project, subject, phase)` through
//! `derive_effect_key` — one row per logical slot, not an append-only pile of
//! duplicates. That journal IS the receipt: what a retry needs to know is
//! whether the slot it is about to dispatch was ever settled.
//!
//! The request key makes the slot stable: the workspace label derives from it,
//! so a retry lands on the same subject and therefore the same row, and
//! [`spawn_gate`] answers with that row's last state. Nothing here dispatches
//! and nothing here writes — the door commits its own row at the same boundary
//! it always did.

use crate::agent_ref::{AgentEffect, EffectState};

/// What a door does with a request key, given the last journaled row for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// No row, or a row whose dispatch never settled: dispatch. The row is
    /// written at the same boundary as before.
    Proceed,
    /// The row settled: the effect already happened. Replay the recorded
    /// slot; never dispatch a second time.
    Replay,
    /// The row is unresolved (`Prepared` / `InFlight`): a dispatch may be in
    /// flight right now, and a second one could fork a crew into one
    /// workspace. Refuse loudly rather than race it.
    InFlight,
    /// The key is already used for a DIFFERENT plan. A replay would hand back
    /// another crew's slot, so this is a refusal, never a silent ok.
    Conflict,
}

/// The pure gate core. One rule, two row shapes ([`spawn_gate`] over an agent
/// effect, [`receipt_gate`] over a door receipt): `offered_digest` is the
/// digest of what the caller is asking for; a row recorded under the same slot
/// with another digest is [`Gate::Conflict`].
///
/// `Unknown`/`Failed` rows proceed: both paths are at-least-once by documented
/// residual, the key's job is to make the retry land on the same slot, and the
/// door's own guards stay the last line.
fn gate_core(state: EffectState, recorded: Option<&str>, offered_digest: &str) -> Gate {
    match state {
        EffectState::Prepared | EffectState::InFlight => Gate::InFlight,
        EffectState::Settled => match recorded {
            Some(recorded) if recorded != offered_digest => Gate::Conflict,
            // A row from before digests were recorded cannot be compared; the
            // slot settled, so the replay is still the answer.
            _ => Gate::Replay,
        },
        EffectState::Failed | EffectState::Unknown => Gate::Proceed,
    }
}

/// The gate for one spawn/add request key.
pub fn spawn_gate(row: Option<&AgentEffect>, offered_digest: &str) -> Gate {
    match row {
        None => Gate::Proceed,
        Some(row) => gate_core(row.state, row.plan_digest.as_deref(), offered_digest),
    }
}

/// The label a request key pins. Deterministic and slug-shaped, so a retry
/// computes the same workspace label — and therefore the same effect row — as
/// the first attempt.
pub fn label_for_request_key(base: &str, key: &str) -> String {
    let joined = format!("{base}-rk-{key}");
    crate::slugify_label(&joined).0
}

/// A stable digest of the spawn plan a request key is used for. Sorted so the
/// same plan in another order is the same plan; the version tag keeps a later
/// plan shape from colliding with an older digest.
pub fn plan_digest(items: &[(String, u32, Option<String>)]) -> String {
    use sha2::{Digest, Sha256};
    let mut rows: Vec<String> = items
        .iter()
        .map(|(kind, count, model)| {
            format!(
                "{}\u{1f}{}\u{1f}{}",
                kind.trim().to_ascii_lowercase(),
                count,
                model.as_deref().unwrap_or("-").trim().to_ascii_lowercase()
            )
        })
        .collect();
    rows.sort();
    let mut h = Sha256::new();
    h.update(b"gs043-spawn-plan-v1\0");
    h.update(rows.join("\n").as_bytes());
    format!("plan:{}", hex::encode(h.finalize()))
}

/// One request-key slot for a door that is NOT an agent lifecycle: a steer
/// dispatch or a halt. Same four answers as the agent shape (one rule, through
/// [`gate_core`]), but the row is the door's own, tagged `door_receipt` in the
/// log so a reader can tell it from an agent effect row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DoorReceipt {
    pub door: String,
    pub slot: String,
    pub state: EffectState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

impl DoorReceipt {
    pub fn prepare(door: &str, slot: &str, digest: &str) -> Self {
        Self {
            door: door.to_string(),
            slot: slot.to_string(),
            state: EffectState::Prepared,
            digest: Some(digest.to_string()),
            evidence: None,
        }
    }
}

/// Intent-before-dispatch for a door: `Prepared` then `InFlight`, journaled
/// before the door performs anything. A dispatch whose intent cannot be
/// committed does not run — the caller gets the error and must not proceed.
pub struct DoorReceiptGuard {
    project: String,
    receipt: DoorReceipt,
}

impl DoorReceiptGuard {
    pub fn begin(project: &str, door: &str, slot: &str, digest: &str) -> anyhow::Result<Self> {
        let mut receipt = DoorReceipt::prepare(door, slot, digest);
        journal_receipt(project, &receipt)?;
        receipt.state = EffectState::InFlight;
        journal_receipt(project, &receipt)?;
        Ok(Self {
            project: project.to_string(),
            receipt,
        })
    }

    /// Apply evidence observed (the dispatch really ran): `Settled`.
    pub fn settle(mut self, evidence: &str) {
        self.receipt.state = EffectState::Settled;
        self.receipt.evidence = Some(evidence.to_string());
        let _ = journal_receipt(&self.project, &self.receipt);
    }

    /// The dispatch ran and its outcome is unobservable: `Unknown` — never a
    /// silent absence, never an invented `Settled`.
    pub fn mark_unknown(mut self, evidence: &str) {
        self.receipt.state = EffectState::Unknown;
        self.receipt.evidence = Some(evidence.to_string());
        let _ = journal_receipt(&self.project, &self.receipt);
    }

    /// The dispatch was refused before it could run (an aborted gate, a failed
    /// precondition): `Failed`. A later attempt under the same key proceeds.
    pub fn mark_failed(mut self, evidence: &str) {
        self.receipt.state = EffectState::Failed;
        self.receipt.evidence = Some(evidence.to_string());
        let _ = journal_receipt(&self.project, &self.receipt);
    }
}

fn journal_receipt(project: &str, receipt: &DoorReceipt) -> anyhow::Result<()> {
    let mut entry = serde_json::to_value(receipt)?;
    // Tagged so a reader can tell a door row from an agent effect row.
    if let serde_json::Value::Object(map) = &mut entry {
        map.insert(
            "journal".to_string(),
            serde_json::Value::String("door_receipt".to_string()),
        );
    }
    crate::state::log_event(project, crate::state::EventKind::State, entry)?;
    eprintln!(
        "flywheel: door receipt {} slot={} state={:?}",
        receipt.door, receipt.slot, receipt.state
    );
    Ok(())
}

/// The slot a door's request key pins: `"<project>-<door>-rk-<key>"`, slugged.
/// Deterministic, so a retry reads the same row the first attempt wrote, and
/// namespaced per door so a steer key can never collide with a halt key.
pub fn door_slot(project: &str, door: &str, key: &str) -> String {
    label_for_request_key(&format!("{project}-{door}"), key)
}

/// A stable digest of a door's payload, version-tagged like [`plan_digest`] so
/// a later payload shape cannot collide with an older digest. Parts are
/// trimmed and joined with a separator that cannot appear in a trimmed field.
pub fn payload_digest(tag: &str, parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(tag.as_bytes());
    h.update(b"\0");
    for p in parts {
        h.update(p.trim().as_bytes());
        h.update(b"\x1f");
    }
    format!("{tag}:{}", hex::encode(h.finalize()))
}

/// The gate for one door request key.
pub fn receipt_gate(row: Option<&DoorReceipt>, offered_digest: &str) -> Gate {
    match row {
        None => Gate::Proceed,
        Some(row) => gate_core(row.state, row.digest.as_deref(), offered_digest),
    }
}

/// The last journaled door row for `(door, slot)`. Missing log, unreadable
/// rows, and rows of another door all read `None` — an absent receipt is not a
/// settled one.
pub fn load_door_receipt(project: &str, door: &str, slot: &str) -> Option<DoorReceipt> {
    let log = crate::state::read_log(project).ok()?;
    log.into_iter().rev().find_map(|e| {
        let data = e.data;
        if data.get("journal").and_then(|v| v.as_str()) != Some("door_receipt") {
            return None;
        }
        let row: DoorReceipt = serde_json::from_value(data).ok()?;
        (row.door == door && row.slot == slot).then_some(row)
    })
}

/// The last journaled spawn row for `subject`, read from the project event
/// log. Missing log, unreadable rows, and a row for another phase all read
/// `None` — an absent receipt is not a settled one.
pub fn load_spawn_row(project: &str, subject: &str) -> Option<AgentEffect> {
    let log = crate::state::read_log(project).ok()?;
    log.into_iter().rev().find_map(|e| {
        let data = e.data;
        let tagged = data.get("journal").and_then(|v| v.as_str()) == Some("agent_effect");
        if !tagged {
            return None;
        }
        let row: AgentEffect = serde_json::from_value(data).ok()?;
        (row.subject == subject && row.phase == crate::agent_ref::AgentPhase::Spawn).then_some(row)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_ref::{AgentEffect, AgentPhase};

    fn row(state: EffectState, digest: Option<&str>) -> AgentEffect {
        let mut r = AgentEffect::prepare("flywheel", "flywheel-flywheel-rk-abc", AgentPhase::Spawn);
        r.state = state;
        r.plan_digest = digest.map(str::to_string);
        r
    }

    /// bd-command-receipts-zile: one key, one dispatch. The second attempt
    /// with the same key and plan replays the recorded slot and performs no
    /// second effect; an unresolved slot refuses; a different plan under the
    /// same key is a typed conflict.
    #[test]
    fn request_key_replay_is_idempotent() {
        let plan = plan_digest(&[("opencode".into(), 3, None)]);
        assert_eq!(
            plan,
            plan_digest(&[("opencode".into(), 3, None)]),
            "the same plan digests the same"
        );
        assert_ne!(
            plan,
            plan_digest(&[("opencode".into(), 4, None)]),
            "another crew size is another plan"
        );
        assert_ne!(
            plan,
            plan_digest(&[("opencode".into(), 3, Some("pi/muse".into()))]),
            "another model is another plan"
        );

        // The label a key pins is stable and slug-shaped, so a retry reads the
        // same slot as the first attempt.
        let label = label_for_request_key("flywheel-flywheel", "Deploy 42");
        assert_eq!(
            label,
            label_for_request_key("flywheel-flywheel", "Deploy 42")
        );
        assert_eq!(label, "flywheel-flywheel-rk-deploy-42");
        assert_ne!(
            label,
            label_for_request_key("flywheel-flywheel", "Deploy 43")
        );

        // The door's contract, driven through the same gate it uses: the
        // second attempt must not touch the dispatch.
        let mut journal: std::collections::BTreeMap<String, AgentEffect> =
            std::collections::BTreeMap::new();
        let mut dispatched = 0u32;
        let attempt = |journal: &mut std::collections::BTreeMap<String, AgentEffect>,
                       dispatched: &mut u32|
         -> Gate {
            let key = "Deploy 42".to_string();
            let gate = spawn_gate(journal.get(&key), &plan);
            if gate == Gate::Proceed {
                *dispatched += 1;
                let mut settled = row(EffectState::Settled, Some(&plan));
                settled.state = EffectState::Settled;
                journal.insert(key, settled);
            }
            gate
        };
        assert_eq!(attempt(&mut journal, &mut dispatched), Gate::Proceed);
        assert_eq!(dispatched, 1, "the first attempt dispatches");
        assert_eq!(
            attempt(&mut journal, &mut dispatched),
            Gate::Replay,
            "a retry replays the settled slot"
        );
        assert_eq!(dispatched, 1, "and dispatches nothing");

        // A second attempt while the first is unresolved refuses: the door
        // may be mid-dispatch, and a fork is worse than a refusal.
        let unresolved = std::collections::BTreeMap::from([(
            "Deploy 43".to_string(),
            row(EffectState::InFlight, Some(&plan)),
        )]);
        assert_eq!(
            spawn_gate(unresolved.get("Deploy 43"), &plan),
            Gate::InFlight
        );
        for state in [EffectState::Prepared, EffectState::InFlight] {
            assert_eq!(
                spawn_gate(Some(&row(state, Some(&plan))), &plan),
                Gate::InFlight
            );
        }

        // The same key with another plan is a typed conflict, never a replay
        // of somebody else's crew.
        let other = plan_digest(&[("opencode".into(), 5, None)]);
        assert_eq!(
            spawn_gate(Some(&row(EffectState::Settled, Some(&plan))), &other),
            Gate::Conflict
        );
        assert_eq!(
            spawn_gate(Some(&row(EffectState::Settled, Some(&plan))), &plan),
            Gate::Replay
        );

        // An unsettled row (the documented at-least-once window) proceeds, and
        // an absent receipt is not a settled one.
        for state in [EffectState::Failed, EffectState::Unknown] {
            assert_eq!(
                spawn_gate(Some(&row(state, Some(&plan))), &plan),
                Gate::Proceed
            );
        }
        assert_eq!(spawn_gate(None, &plan), Gate::Proceed);
    }

    /// bd-4fgc: the same one-key-one-dispatch contract for the doors that are
    /// not agent lifecycles (steer, halt) — the row, the slot namespace, the
    /// payload digest, and the four answers driven through the gate the doors
    /// actually call.
    #[test]
    fn door_receipt_replay_is_idempotent() {
        let digest = payload_digest("steer-v1", &["ship the auth refactor", "auto", "goal"]);
        assert_eq!(
            digest,
            payload_digest("steer-v1", &["ship the auth refactor", "auto", "goal"]),
            "the same payload digests the same"
        );
        assert_eq!(
            digest,
            payload_digest("steer-v1", &["ship the auth refactor ", "auto", "goal"]),
            "a trimmed field is the same field"
        );
        assert_ne!(
            digest,
            payload_digest("steer-v1", &["ship the auth refactor", "auto", "direct"]),
            "another mode is another payload"
        );

        // Doors namespace their slots: one key on two doors is two slots, and
        // the same key on the same door is the same slot.
        let steer = door_slot("flywheel", "steer", "Deploy 42");
        let halt = door_slot("flywheel", "halt", "Deploy 42");
        assert_eq!(steer, "flywheel-steer-rk-deploy-42");
        assert_eq!(steer, door_slot("flywheel", "steer", "Deploy 42"));
        assert_ne!(steer, halt, "a steer key never names a halt slot");

        // The door's contract, driven through the same gate it uses: the
        // second attempt must not send anything.
        let mut rows: std::collections::BTreeMap<String, DoorReceipt> =
            std::collections::BTreeMap::new();
        let mut sent = 0u32;
        let attempt =
            |rows: &mut std::collections::BTreeMap<String, DoorReceipt>, sent: &mut u32| -> Gate {
                let slot = door_slot("flywheel", "steer", "Deploy 42");
                let gate = receipt_gate(rows.get(&slot), &digest);
                if gate == Gate::Proceed {
                    *sent += 1;
                    let mut settled = DoorReceipt::prepare("steer", &slot, &digest);
                    settled.state = EffectState::Settled;
                    rows.insert(slot, settled);
                }
                gate
            };
        assert_eq!(attempt(&mut rows, &mut sent), Gate::Proceed);
        assert_eq!(sent, 1, "the first attempt dispatches");
        assert_eq!(
            attempt(&mut rows, &mut sent),
            Gate::Replay,
            "a retry replays the settled slot"
        );
        assert_eq!(sent, 1, "and sends nothing");

        // An unresolved slot refuses (a dispatch may be running right now),
        // another payload under the same key is a typed conflict, and an
        // unsettled row proceeds.
        let slot = door_slot("flywheel", "steer", "Deploy 42");
        for state in [EffectState::Prepared, EffectState::InFlight] {
            let mut row = DoorReceipt::prepare("steer", &slot, &digest);
            row.state = state;
            assert_eq!(receipt_gate(Some(&row), &digest), Gate::InFlight);
        }
        let mut settled = DoorReceipt::prepare("steer", &slot, &digest);
        settled.state = EffectState::Settled;
        let other = payload_digest("steer-v1", &["a different objective", "auto", "goal"]);
        assert_eq!(receipt_gate(Some(&settled), &other), Gate::Conflict);
        for state in [EffectState::Failed, EffectState::Unknown] {
            let mut row = DoorReceipt::prepare("steer", &slot, &digest);
            row.state = state;
            assert_eq!(receipt_gate(Some(&row), &digest), Gate::Proceed);
        }
        assert_eq!(receipt_gate(None, &digest), Gate::Proceed);
    }
}

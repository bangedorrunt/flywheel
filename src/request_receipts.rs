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

/// The gate for one request key. `offered_digest` is the digest of the plan
/// the caller is asking for ([`plan_digest`]); a row recorded under the same
/// key with another digest is [`Gate::Conflict`].
///
/// `Unknown`/`Failed` rows proceed: the spawn path is at-least-once by
/// documented residual, the key's job is to make the retry land on the same
/// slot, and the workspace/name guard stays the last line.
pub fn spawn_gate(row: Option<&AgentEffect>, offered_digest: &str) -> Gate {
    let Some(row) = row else {
        return Gate::Proceed;
    };
    match row.state {
        EffectState::Prepared | EffectState::InFlight => Gate::InFlight,
        EffectState::Settled => match row.plan_digest.as_deref() {
            Some(recorded) if recorded != offered_digest => Gate::Conflict,
            // A row from before plan digests were recorded cannot be
            // compared; the slot settled, so the replay is still the answer.
            _ => Gate::Replay,
        },
        EffectState::Failed | EffectState::Unknown => Gate::Proceed,
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
}

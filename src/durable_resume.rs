// governed-by: ADR-0016
//! Pure durable-loop recovery projections.
//!
//! Recovery data is an input projection. Loading it is an interpreter concern;
//! applying it to a `LoopModel` remains pure and replayable.

use std::collections::{BTreeMap, HashSet};

use crate::durable::{DRAIN_GRACE_POLLS, DrainState, HealState, LoopModel};

/// Seed crash-resume state from persisted projections without performing I/O.
///
/// Heal counters carry over with `awaiting_clear` reset. Completion-mail
/// deduplication carries over verbatim. Open drains restart with fresh grace,
/// so a crash never force-kills immediately on resume.
pub(crate) fn seed_swarm_resume(
    model: &mut LoopModel,
    heal_states: &BTreeMap<String, crate::state::HealState>,
    done_mailed: &HashSet<String>,
    drain_pending: &BTreeMap<String, String>,
) {
    for (name, state) in heal_states {
        model.heal_states.insert(
            name.clone(),
            HealState {
                unknown_count: state.unknown_count,
                diagnose_count: state.diagnose_count,
                respawn_count: state.respawn_count,
                awaiting_clear: false,
            },
        );
    }
    model.done_mailed = done_mailed.clone();
    model.drain_pending = drain_pending
        .iter()
        .map(|(name, bead_id)| {
            (
                name.clone(),
                DrainState {
                    bead_id: bead_id.clone(),
                    grace_polls: DRAIN_GRACE_POLLS,
                    defers: 0,
                    paused: None,
                },
            )
        })
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_seed_resets_transient_heal_and_drain_state() {
        let mut model = LoopModel::default();
        let mut heal = BTreeMap::new();
        heal.insert(
            "coder".into(),
            crate::state::HealState {
                unknown_count: 3,
                diagnose_count: 2,
                respawn_count: 1,
                awaiting_clear: true,
            },
        );
        let done = HashSet::from(["bd-done".to_string()]);
        let drains = BTreeMap::from([("coder".to_string(), "bd-drain".to_string())]);

        seed_swarm_resume(&mut model, &heal, &done, &drains);

        assert_eq!(model.done_mailed, done);
        assert!(!model.heal_states["coder"].awaiting_clear);
        assert_eq!(model.drain_pending["coder"].grace_polls, DRAIN_GRACE_POLLS);
        assert_eq!(model.drain_pending["coder"].defers, 0);
        assert!(model.drain_pending["coder"].paused.is_none());
    }
}

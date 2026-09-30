// governed-by: ADR-0016
//! Configuration values used by the pure workspace reducer.
//!
//! `WorkspacePolicy` is data inside the model. The reducer reads it without
//! touching process state; the environment adapter is called only at an
//! effect boundary when a live host is constructed.

use serde::{Deserialize, Serialize};

use crate::workspace::{
    DIGEST_INTERVAL_DEFAULT_SECS, DIGEST_THRESHOLD_DEFAULT, DOORBELL_GRACE_DEFAULT_SECS,
    DOORBELL_RING_MAX_DEFAULT,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePolicy {
    pub digest_interval_secs: u64,
    pub digest_threshold: usize,
    pub doorbell_grace_secs: u64,
    pub doorbell_ring_max: u32,
}

impl Default for WorkspacePolicy {
    fn default() -> Self {
        Self {
            digest_interval_secs: DIGEST_INTERVAL_DEFAULT_SECS,
            digest_threshold: DIGEST_THRESHOLD_DEFAULT,
            doorbell_grace_secs: DOORBELL_GRACE_DEFAULT_SECS,
            doorbell_ring_max: DOORBELL_RING_MAX_DEFAULT,
        }
    }
}

impl WorkspacePolicy {
    /// Resolve host configuration once, before the policy enters the model.
    /// Invalid, zero, or absent values keep the deterministic defaults.
    pub fn from_environment() -> Self {
        let default = Self::default();
        Self {
            digest_interval_secs: positive_u64(
                "FLYWHEEL_DIGEST_SECS",
                default.digest_interval_secs,
            ),
            digest_threshold: positive_usize("FLYWHEEL_DIGEST_THRESHOLD", default.digest_threshold),
            doorbell_grace_secs: positive_u64(
                "FLYWHEEL_DOORBELL_GRACE_SECS",
                default.doorbell_grace_secs,
            ),
            doorbell_ring_max: positive_u32(
                "FLYWHEEL_DOORBELL_RING_MAX",
                default.doorbell_ring_max,
            ),
        }
    }
}

fn positive_u64(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

fn positive_usize(name: &str, fallback: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

fn positive_u32(name: &str, fallback: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_deterministic_and_nonzero() {
        let policy = WorkspacePolicy::default();
        assert_eq!(policy.digest_interval_secs, DIGEST_INTERVAL_DEFAULT_SECS);
        assert!(policy.digest_threshold > 0);
        assert!(policy.doorbell_grace_secs > 0);
        assert!(policy.doorbell_ring_max > 0);
    }

    #[test]
    fn policy_is_consumed_by_the_reducer_instead_of_process_state() {
        let model = crate::workspace::WorkspaceModel {
            policy: WorkspacePolicy {
                digest_interval_secs: 10_000,
                digest_threshold: 1,
                ..WorkspacePolicy::default()
            },
            ..Default::default()
        };
        let (model, _) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::Fact {
                summary: "policy-witness".into(),
            },
        );
        let (_, cmds) =
            crate::workspace::tick(&model, &crate::workspace::Msg::AckSlaTick { now: 1 });
        assert!(
            cmds.iter()
                .any(|cmd| matches!(cmd, crate::workspace::Cmd::Digest { .. })),
            "model policy must control digest threshold"
        );
    }
}

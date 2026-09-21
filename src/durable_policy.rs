// governed-by: ADR-0016
//! Reclaim policy for the durable loop (ADR-0025 thresholds).
//!
//! Thresholds are loop params, resolved from the environment once at loop
//! start and serialized with the invocation so a replay uses the same
//! policy. The pure reclaim predicates read the struct; they never read
//! process state.

use serde::{Deserialize, Serialize};

use crate::durable::{FLYWHEEL_RECLAIM_HUMAN_SECS, FLYWHEEL_RECLAIM_SWARM_SECS};

/// Deterministic crate defaults; `Default` must never be zero (a zero
/// threshold would reclaim live work immediately).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimPolicy {
    pub swarm_secs: u64,
    pub human_secs: u64,
}

impl Default for ReclaimPolicy {
    fn default() -> Self {
        Self {
            swarm_secs: FLYWHEEL_RECLAIM_SWARM_SECS,
            human_secs: FLYWHEEL_RECLAIM_HUMAN_SECS,
        }
    }
}

impl ReclaimPolicy {
    // Test-only constructor; production resolves via `default()` (crate
    // constants) or `from_environment()` (loop start).
    #[cfg(test)]
    pub const fn new(swarm_secs: u64, human_secs: u64) -> Self {
        Self {
            swarm_secs,
            human_secs,
        }
    }

    /// Resolve host configuration once, at loop start. Zero or unparseable
    /// values keep the crate defaults (a zero threshold would reclaim live
    /// work immediately).
    pub fn from_environment() -> Self {
        Self {
            swarm_secs: positive("FLYWHEEL_RECLAIM_SWARM_SECS", FLYWHEEL_RECLAIM_SWARM_SECS),
            human_secs: positive("FLYWHEEL_RECLAIM_HUMAN_SECS", FLYWHEEL_RECLAIM_HUMAN_SECS),
        }
    }
}

fn positive(name: &str, fallback: u64) -> u64 {
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
    fn defaults_match_crate_constants() {
        let policy = ReclaimPolicy::default();
        assert_eq!(policy.swarm_secs, FLYWHEEL_RECLAIM_SWARM_SECS);
        assert_eq!(policy.human_secs, FLYWHEEL_RECLAIM_HUMAN_SECS);
    }
}

// governed-by: ADR-0011
//! SIGNOFF auto-pass predicate (ADR-0011 D7).
//!
//! Captain-only for irreversible preference. Auto-pass iff REVIEW residual
//! is empty AND OPEN DECISIONS is empty.

pub fn auto_pass(residual_empty: bool, open_decisions: usize) -> bool {
    residual_empty && open_decisions == 0
}

/// REVIEW residual is empty when no lane is still in review and helm
/// did not degrade the review surface. OPEN DECISIONS is `decisions.len()`.
pub fn observe(open_decisions: usize, review_residual: usize, review_degraded: bool) -> bool {
    auto_pass(review_residual == 0 && !review_degraded, open_decisions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn residual_and_no_decisions_auto_passes() {
        assert!(auto_pass(true, 0));
    }

    #[test]
    fn open_decision_blocks_auto_pass() {
        assert!(!auto_pass(true, 1));
    }

    #[test]
    fn residual_findings_block_auto_pass() {
        assert!(!auto_pass(false, 0));
    }

    #[test]
    fn observe_empty_world_auto_passes() {
        assert!(observe(0, 0, false));
    }

    #[test]
    fn observe_open_decision_blocks() {
        assert!(!observe(1, 0, false));
    }

    #[test]
    fn open_lanes_are_review_residual() {
        assert!(!observe(0, 3, false));
    }
}

// governed-by: ADR-0015
// governed-by: ADR-0016
//! Host-held consecutive-failure breaker (P2).
//!
//! New callers should use the `*_at` methods with a timestamp from injected
//! authority. The compatibility methods remain for untouched legacy edges and
//! use the system clock at the effect boundary, never inside a reducer.

use std::time::Duration;

use crate::authority::{Clock, SystemClock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
}

#[derive(Debug)]
pub struct ConsecutiveBreaker {
    pub threshold: u32,
    pub cooldown: Duration,
    failures: u32,
    opened_at: Option<u64>,
}

impl ConsecutiveBreaker {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold,
            cooldown,
            failures: 0,
            opened_at: None,
        }
    }

    /// Compatibility boundary for callers not yet migrated to authority.
    pub fn allows(&mut self) -> bool {
        self.allows_at(SystemClock.now_unix())
    }

    pub fn allows_at(&mut self, now_unix: u64) -> bool {
        let Some(opened_at) = self.opened_at else {
            return true;
        };
        if now_unix.saturating_sub(opened_at) >= self.cooldown.as_secs() {
            self.opened_at = None;
            self.failures = 0;
            return true;
        }
        false
    }

    /// Compatibility boundary for callers not yet migrated to authority.
    pub fn record_success(&mut self) {
        self.record_success_at();
    }

    pub fn record_success_at(&mut self) {
        self.failures = 0;
    }

    /// Compatibility boundary for callers not yet migrated to authority.
    pub fn record_failure(&mut self) {
        self.record_failure_at(SystemClock.now_unix());
    }

    pub fn record_failure_at(&mut self, now_unix: u64) {
        self.failures = self.failures.saturating_add(1);
        if self.failures >= self.threshold {
            self.opened_at = Some(now_unix);
        }
    }

    pub fn state(&self) -> BreakerState {
        if self.opened_at.is_some() {
            BreakerState::Open
        } else {
            BreakerState::Closed
        }
    }
}

impl Default for ConsecutiveBreaker {
    fn default() -> Self {
        Self::new(5, Duration::from_secs(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_at_threshold_then_half_open_probe_without_sleep() {
        let mut b = ConsecutiveBreaker::new(3, Duration::from_secs(50));
        b.record_failure_at(100);
        b.record_failure_at(101);
        assert_eq!(b.state(), BreakerState::Closed);
        b.record_failure_at(102);
        assert_eq!(b.state(), BreakerState::Open);
        assert!(!b.allows_at(151));
        assert!(b.allows_at(152));
        assert_eq!(b.state(), BreakerState::Closed);
    }

    #[test]
    fn success_resets_count() {
        let mut b = ConsecutiveBreaker::new(2, Duration::from_secs(60));
        b.record_failure_at(100);
        b.record_success_at();
        b.record_failure_at(101);
        assert_eq!(b.state(), BreakerState::Closed);
    }
}

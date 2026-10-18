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
    HalfOpen,
}

#[derive(Debug)]
pub struct ConsecutiveBreaker {
    pub threshold: u32,
    pub cooldown: Duration,
    failures: u32,
    opened_at: Option<u64>,
    half_open: bool,
}

impl ConsecutiveBreaker {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold,
            cooldown,
            failures: 0,
            opened_at: None,
            half_open: false,
        }
    }

    /// Compatibility boundary for callers not yet migrated to authority.
    #[allow(dead_code)]
    pub fn allows(&mut self) -> bool {
        self.allows_at(SystemClock.now_unix())
    }

    pub fn allows_at(&mut self, now_unix: u64) -> bool {
        if self.half_open {
            return false;
        }
        let Some(opened_at) = self.opened_at else {
            return true;
        };
        if now_unix.saturating_sub(opened_at) >= self.cooldown.as_secs() {
            self.half_open = true;
            return true;
        }
        false
    }

    /// Compatibility boundary for callers not yet migrated to authority.
    #[allow(dead_code)]
    pub fn record_success(&mut self) {
        self.record_success_at();
    }

    pub fn record_success_at(&mut self) {
        self.failures = 0;
        self.opened_at = None;
        self.half_open = false;
    }

    /// Compatibility boundary for callers not yet migrated to authority.
    #[allow(dead_code)]
    pub fn record_failure(&mut self) {
        self.record_failure_at(SystemClock.now_unix());
    }

    pub fn record_failure_at(&mut self, now_unix: u64) {
        if self.half_open {
            self.opened_at = Some(now_unix);
            self.half_open = false;
            return;
        }
        self.failures = self.failures.saturating_add(1);
        if self.failures >= self.threshold {
            self.opened_at = Some(now_unix);
        }
    }

    pub fn state(&self) -> BreakerState {
        if self.half_open {
            BreakerState::HalfOpen
        } else if self.opened_at.is_some() {
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

/// Breaker bounds as elm-effect's `policy::remote::BreakerConfig`
/// (`max_failures` + `reset`), via `toron_core::kernel` (ADR-0040: only the
/// kernel port names the framework). One source of truth: this converts the
/// host-held threshold/cooldown into the type the policy stack consumes.
impl ConsecutiveBreaker {
    pub fn to_elm_breaker_config(&self) -> toron_core::kernel::BreakerConfig {
        toron_core::kernel::BreakerConfig {
            max_failures: self.threshold,
            reset: toron_core::kernel::Duration::from_millis(
                self.cooldown.as_millis().min(u64::MAX as u128) as u64,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_at_threshold_then_allows_one_half_open_probe() {
        let mut b = ConsecutiveBreaker::new(3, Duration::from_secs(50));
        b.record_failure_at(100);
        b.record_failure_at(101);
        assert_eq!(b.state(), BreakerState::Closed);
        b.record_failure_at(102);
        assert_eq!(b.state(), BreakerState::Open);
        assert!(!b.allows_at(151));
        assert!(b.allows_at(152));
        assert_eq!(b.state(), BreakerState::HalfOpen);
        assert!(!b.allows_at(153));
        b.record_failure_at(153);
        assert_eq!(b.state(), BreakerState::Open);
        assert!(!b.allows_at(154));
        assert!(b.allows_at(203));
        b.record_success_at();
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

    #[test]
    fn mapping_preserves_threshold_and_cooldown() {
        let b = ConsecutiveBreaker::new(3, Duration::from_secs(50));
        let cfg = b.to_elm_breaker_config();
        assert_eq!(cfg.max_failures, 3);
        assert_eq!(cfg.reset.as_millis(), 50_000);
        let d = ConsecutiveBreaker::default().to_elm_breaker_config();
        assert_eq!(d.max_failures, 5);
        assert_eq!(d.reset.as_millis(), 60_000);
    }

    #[test]
    fn decide_stays_pure_for_elm_fold() {
        let src = include_str!("decide.rs");
        for banned in ["SystemTime", "std::env", "thread_rng", "StdRng", "rand::"] {
            assert!(
                !src.contains(banned),
                "decide.rs must stay pure for elm-effect fold, found {banned}"
            );
        }
    }
}

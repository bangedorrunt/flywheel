// governed-by: ADR-0015
//! Host-held consecutive-failure breaker (P2). Open = fast escalate + audit.
//! Remote edges only; reviewers reject breaker-on-every-call.

use std::time::{Duration, Instant};

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
    opened_at: Option<Instant>,
}

impl ConsecutiveBreaker {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self { threshold, cooldown, failures: 0, opened_at: None }
    }

    pub fn allows(&mut self) -> bool {
        match self.opened_at {
            None => true,
            Some(t) if t.elapsed() >= self.cooldown => {
                self.opened_at = None;
                self.failures = 0;
                true
            }
            Some(_) => false,
        }
    }

    pub fn record_success(&mut self) {
        self.failures = 0;
    }

    pub fn record_failure(&mut self) {
        self.failures += 1;
        if self.failures >= self.threshold {
            self.opened_at = Some(Instant::now());
        }
    }

    pub fn state(&self) -> BreakerState {
        match self.opened_at {
            None => BreakerState::Closed,
            Some(_) => BreakerState::Open,
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
    fn opens_at_threshold_then_half_open_probe() {
        let mut b = ConsecutiveBreaker::new(3, Duration::from_millis(50));
        b.record_failure();
        b.record_failure();
        assert_eq!(b.state(), BreakerState::Closed);
        b.record_failure();
        assert_eq!(b.state(), BreakerState::Open);
        assert!(!b.allows());
        std::thread::sleep(Duration::from_millis(60));
        assert!(b.allows());
        assert_eq!(b.state(), BreakerState::Closed);
    }

    #[test]
    fn success_resets_count() {
        let mut b = ConsecutiveBreaker::new(2, Duration::from_secs(60));
        b.record_failure();
        b.record_success();
        b.record_failure();
        assert_eq!(b.state(), BreakerState::Closed);
    }
}

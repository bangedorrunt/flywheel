// governed-by: ADR-0012
//! Supervision policy (Transient) + watch-retry gate — pure state machines
//! shared by sensor and sink supervisors (D3, kameo concepts, no dependency).

/// Capped restarts: failures only, `cap` restarts per `window_secs`,
/// then park + `Degraded` + audit. The daemon itself keeps serving.
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    cap: usize,
    window_secs: u64,
    restarts: Vec<u64>,
}

impl RestartPolicy {
    /// D3 default: 5 restarts / 5s.
    pub fn transient() -> Self {
        Self::transient_cap(5, 5)
    }

    pub fn transient_cap(cap: usize, window_secs: u64) -> Self {
        Self {
            cap,
            window_secs,
            restarts: Vec::new(),
        }
    }

    /// Record a failure at `now_unix`. True = restart, false = park.
    pub fn should_restart(&mut self, now_unix: u64) -> bool {
        self.restarts
            .retain(|t| now_unix.saturating_sub(*t) < self.window_secs);
        if self.restarts.len() >= self.cap {
            return false;
        }
        self.restarts.push(now_unix);
        true
    }

    pub fn parked(&self, now_unix: u64) -> bool {
        let recent = self
            .restarts
            .iter()
            .filter(|t| now_unix.saturating_sub(**t) < self.window_secs)
            .count();
        recent >= self.cap
    }
}

/// Watch-child retry gate (GS-007 never-deaf rule): 5-burst 1→16s backoff,
/// then a 16s retry gate forever. The child never exits on repeated failure.
#[derive(Debug, Clone, Default)]
pub struct WatchRetry {
    fails: u32,
}

impl WatchRetry {
    /// Backoff after a failure: 1,2,4,8,16s, then the 16s gate forever.
    pub fn record_fail(&mut self) -> u64 {
        let backoff = (1u64 << self.fails.min(4)).min(16);
        self.fails = self.fails.saturating_add(1);
        backoff
    }

    pub fn reset(&mut self) {
        self.fails = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_parks_after_cap() {
        let mut p = RestartPolicy::transient_cap(5, 5);
        // Five rapid failures consume the cap; the sixth parks.
        for t in 0..5 {
            assert!(p.should_restart(t), "restart {t}");
        }
        assert!(!p.should_restart(4), "parks at cap");
        assert!(p.parked(4));
        // Window slides: old failures age out, restarts resume.
        assert!(p.should_restart(100));
    }

    #[test]
    fn watch_retry_burst_then_gate() {
        let mut r = WatchRetry::default();
        let seq: Vec<u64> = (0..8).map(|_| r.record_fail()).collect();
        assert_eq!(seq, vec![1, 2, 4, 8, 16, 16, 16, 16]);
        r.reset();
        assert_eq!(r.record_fail(), 1);
    }
}

// governed-by: ADR-0015
//! Shared retry policy (P1). Timeout inside, retry outside, fallback outside that.
//!
//! bd-retry-jitter-xlh6: the delay is drawn from the upper half of the attempt's
//! ceiling (`ceiling/2 + ceiling/2 * draw`). The old fixed +10% multiplier was
//! not jitter — every caller slept the same and retried in lockstep after a
//! shared outage. The draw is an input, never read here: [`host_draw`] is the
//! one host entropy seam and tests inject their own.
//!
//! Boundary (bd-fa58): this driver is for LOCAL ipc only — herdr socket
//! re-reads, pane text primes, registry settles. Remote dispatch rides the
//! framework's `policy::remote` (see `dispatch_policy` in pane.rs) and must
//! never be "unified" onto this driver: a shared breaker or bulkhead across
//! unrelated local operations would couple failures that have nothing in
//! common, and this driver has neither. One carrier per direction.

use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base: Duration::from_millis(200),
            cap: Duration::from_secs(5),
        }
    }
}

/// One host entropy draw in `[0, 1)` for [`RetryPolicy::delay_for`]. Same
/// convention as `comms_send::random_draw` (std's per-process random hasher
/// state, mixed with the clock and a counter; no new dependency). Tests inject
/// their own draw instead of calling this, so no test sleeps or races.
pub fn host_draw() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(N.fetch_add(1, Ordering::Relaxed));
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    // 53 bits keep the [0, 1) mapping exact in f64.
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

impl RetryPolicy {
    /// Pure: the un-jittered ceiling for `attempt` (0-based), never above `cap`.
    pub fn ceiling_for(&self, attempt: u32) -> Duration {
        let exp = self
            .base
            .as_millis()
            .saturating_mul(1u128 << attempt.min(10));
        Duration::from_millis(exp.min(self.cap.as_millis()) as u64)
    }

    /// Pure: upper-half jitter. `draw` is a value in `[0, 1)`: 0 maps to
    /// `ceiling/2`, a draw near 1 approaches the ceiling. The same attempt and
    /// draw always give the same delay, so the schedule is asserted without
    /// sleeping.
    pub fn delay_for(&self, attempt: u32, draw: f64) -> Duration {
        let ceiling = self.ceiling_for(attempt).as_millis();
        let floor = ceiling / 2;
        let draw = if draw.is_finite() {
            draw.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // f64 math: `(ceiling - floor) * draw as u128` would truncate the draw
        // to 0 or 1 and collapse the whole window to the floor.
        Duration::from_millis((floor as f64 + (ceiling - floor) as f64 * draw) as u64)
    }

    /// Run `op` until success, non-retryable error, or cap. `is_retryable`
    /// classifies the error; `on_retry` observes each wait (warn lines);
    /// `draw` supplies one jitter draw per retry and `sleep` performs the wait,
    /// both injected so tests substitute randomness and never sleep.
    pub fn retry_with_backoff<T, E>(
        &self,
        mut op: impl FnMut() -> Result<T, E>,
        is_retryable: impl Fn(&E) -> bool,
        mut on_retry: impl FnMut(u32, &E),
        mut draw: impl FnMut() -> f64,
        mut sleep: impl FnMut(Duration),
    ) -> Result<T, E> {
        let mut attempt = 0u32;
        loop {
            match op() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if !is_retryable(&e) || attempt + 1 >= self.max_attempts {
                        return Err(e);
                    }
                    on_retry(attempt + 1, &e);
                    sleep(self.delay_for(attempt, draw()));
                    attempt += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            ..Default::default()
        }
    }

    #[test]
    fn fail_twice_then_succeed_stops_at_cap() {
        let p = policy();
        let mut calls = 0;
        let mut sleeps = 0;
        let mut retries_seen = 0;
        let r = p.retry_with_backoff(
            || {
                calls += 1;
                if calls < 3 {
                    Err("boom")
                } else {
                    Ok("ok".to_string())
                }
            },
            |_| true,
            |_, _| retries_seen += 1,
            || 0.5,
            |_| sleeps += 1,
        );
        assert_eq!(r.unwrap(), "ok");
        assert_eq!(calls, 3);
        assert_eq!(sleeps, 2);
        assert_eq!(retries_seen, 2);
    }

    #[test]
    fn non_retryable_does_not_sleep() {
        let p = RetryPolicy::default();
        let mut sleeps = 0;
        let r: Result<String, &str> = p.retry_with_backoff(
            || Err("nope"),
            |_| false,
            |_, _| {},
            || 0.5,
            |_| sleeps += 1,
        );
        assert!(r.is_err());
        assert_eq!(sleeps, 0);
    }

    #[test]
    fn ceiling_caps_the_schedule() {
        let p = policy();
        assert_eq!(p.ceiling_for(0), p.base);
        assert_eq!(p.ceiling_for(1), p.base * 2);
        assert!(p.ceiling_for(100) <= p.cap, "far attempts bounded by cap");
    }

    /// bd-retry-jitter-xlh6: the delay is drawn from the UPPER HALF of the
    /// attempt's ceiling. Draw 0 is the floor (ceiling/2), a draw near 1
    /// approaches the ceiling, and a far attempt never exceeds the cap.
    #[test]
    fn delay_is_drawn_from_the_upper_half_of_the_ceiling() {
        let p = policy();
        let ceiling = p.ceiling_for(3).as_millis();
        assert_eq!(p.delay_for(3, 0.0).as_millis(), ceiling / 2);
        let high = p.delay_for(3, 0.999).as_millis();
        assert!(
            high >= ceiling / 2 && high < ceiling,
            "high draw {high} inside [half, ceiling)"
        );
        assert_eq!(p.delay_for(3, 1.0).as_millis(), ceiling);
        assert!(p.delay_for(100, 1.0) <= p.cap);
    }

    /// Two callers that draw independently at one attempt must not sleep the
    /// same amount; a fixed multiplier kept them in lockstep after an outage.
    #[test]
    fn two_draws_at_one_attempt_decorrelate() {
        let p = policy();
        let low = p.delay_for(4, 0.05).as_millis();
        let high = p.delay_for(4, 0.95).as_millis();
        assert_ne!(low, high, "independent draws must not share a delay");
        assert!(high > low, "the larger draw sleeps longer: {low} vs {high}");
    }

    /// One draw per retry, and the drawn delay is what gets slept.
    #[test]
    fn retry_consults_the_draw_once_per_retry() {
        let p = policy();
        let mut draws = vec![0.0, 1.0];
        let mut slept: Vec<Duration> = Vec::new();
        let mut calls = 0;
        let out = p.retry_with_backoff(
            || {
                calls += 1;
                if calls < 3 { Err(()) } else { Ok(()) }
            },
            |_| true,
            |_, _| {},
            || draws.remove(0),
            |d| slept.push(d),
        );
        assert!(out.is_ok());
        assert!(draws.is_empty(), "one draw per retry");
        assert_eq!(slept.len(), 2);
        assert_ne!(slept[0], slept[1], "each retry uses its own draw");
    }

    /// The host seam is the only place randomness enters. Callers substitute
    /// it; this test pins the range and that consecutive draws differ.
    #[test]
    fn host_draw_stays_in_range_and_varies() {
        let draws: Vec<f64> = (0..32).map(|_| host_draw()).collect();
        assert!(
            draws.iter().all(|d| (0.0..1.0).contains(d)),
            "draws stay in [0,1): {draws:?}"
        );
        let distinct: std::collections::BTreeSet<u64> = draws.iter().map(|d| d.to_bits()).collect();
        assert!(distinct.len() > 1, "consecutive draws vary");
    }
}

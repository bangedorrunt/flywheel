// governed-by: ADR-0015
//! Shared retry policy (P1). Timeout inside, retry outside, fallback outside that.
//! ponytail: fixed 10% jitter, richer decorrelation when a storm test demands it.

use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base: Duration,
    pub cap: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 3, base: Duration::from_millis(200), cap: Duration::from_secs(5) }
    }
}

impl RetryPolicy {
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let exp = self.base.as_millis().saturating_mul(1u128 << attempt.min(10));
        let capped = exp.min(self.cap.as_millis());
        let jitter = capped / 10;
        Duration::from_millis((capped + jitter) as u64)
    }

    /// Run `op` until success, non-retryable error, or cap. `is_retryable`
    /// classifies the error; `sleep` is injected so tests substitute.
    pub fn retry_with_backoff<E>(
        &self,
        mut op: impl FnMut() -> Result<String, E>,
        is_retryable: impl Fn(&E) -> bool,
        mut sleep: impl FnMut(Duration),
    ) -> Result<String, E> {
        let mut attempt = 0u32;
        loop {
            match op() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if !is_retryable(&e) || attempt + 1 >= self.max_attempts {
                        return Err(e);
                    }
                    sleep(self.delay_for(attempt));
                    attempt += 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_twice_then_succeed_stops_at_cap() {
        let p = RetryPolicy { max_attempts: 3, ..Default::default() };
        let mut calls = 0;
        let mut sleeps = 0;
        let r = p.retry_with_backoff(
            || {
                calls += 1;
                if calls < 3 { Err("boom") } else { Ok("ok".into()) }
            },
            |_| true,
            |_| sleeps += 1,
        );
        assert_eq!(r.unwrap(), "ok");
        assert_eq!(calls, 3);
        assert_eq!(sleeps, 2);
    }

    #[test]
    fn non_retryable_does_not_sleep() {
        let p = RetryPolicy::default();
        let mut sleeps = 0;
        let r: Result<String, &str> = p.retry_with_backoff(|| Err("nope"), |_| false, |_| sleeps += 1);
        assert!(r.is_err());
        assert_eq!(sleeps, 0);
    }

    #[test]
    fn delay_caps() {
        let p = RetryPolicy::default();
        assert!(p.delay_for(100) <= Duration::from_secs(6));
    }
}

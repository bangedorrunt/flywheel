// governed-by: ADR-0012
//! 5/30/3 per-pin budget gate — verbatim port of `src/doorbell.rs:112-184`.
//!
//! Rolling token bucket: 3 prompts per 30-min window, refilled continuously.
//! RAM-only in Phase 1 (known hole, asserted by the Phase-1 crash test);
//! becomes durable model state with journal v2 in Phase 2 (D4.5, bd-asr2.9).

use std::collections::BTreeMap;

/// Per-pin budget the dispatcher enforces.
#[derive(Debug, Clone)]
pub struct PinBudget {
    pub budget_minutes: u32,
    pub max_prompts: u32,
    /// unix secs of each granted prompt (rolling window)
    pub prompts: Vec<u64>,
    pub started_unix: u64,
}

impl PinBudget {
    pub fn adr_default(started_unix: u64) -> Self {
        Self::new(30, 3, started_unix)
    }

    pub fn new(budget_minutes: u32, max_prompts: u32, started_unix: u64) -> Self {
        Self {
            budget_minutes,
            max_prompts,
            prompts: Vec::new(),
            started_unix,
        }
    }

    fn window_secs(&self) -> u64 {
        u64::from(self.budget_minutes) * 60
    }

    pub fn allows(&self, now_unix: u64) -> bool {
        let window = self.window_secs();
        let recent = self
            .prompts
            .iter()
            .filter(|t| now_unix.saturating_sub(**t) < window)
            .count();
        (recent as u32) < self.max_prompts
    }

    pub fn record_at(&mut self, now_unix: u64) {
        let window = self.window_secs();
        self.prompts
            .retain(|t| now_unix.saturating_sub(*t) < window);
        self.prompts.push(now_unix);
    }
}

/// Drop injects that exceed the pin's budget (rolling). Pure.
pub fn take_allowed(
    injects: Vec<(String, String)>,
    budgets: &mut BTreeMap<String, PinBudget>,
    now_unix: u64,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (pin, msg) in injects {
        let b = budgets
            .entry(pin.clone())
            .or_insert_with(|| PinBudget::adr_default(now_unix));
        if b.allows(now_unix) {
            b.record_at(now_unix);
            out.push((pin, msg));
        }
    }
    out
}

#[cfg(test)]
    #[test]
    fn three_per_pin_per_window_then_cut() {
        let mut m = BTreeMap::new();
        let one = |pin: &str| vec![(pin.to_string(), "x".into())];
        // 3 prompts for one pin pass; the 4th inside the window drops.
        for _ in 0..3 {
            assert_eq!(take_allowed(one("p"), &mut m, 1000).len(), 1);
        }
        assert!(take_allowed(one("p"), &mut m, 1001).is_empty());
        // A different pin is unaffected (per-pin buckets).
        assert_eq!(take_allowed(one("q"), &mut m, 1001).len(), 1);
        // Window slides: 30 min later the pin passes again.
        assert_eq!(take_allowed(one("p"), &mut m, 1000 + 1800).len(), 1);
    }

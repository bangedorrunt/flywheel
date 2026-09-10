// governed-by: ADR-0012
//! Heard-or-deaf tracking — verbatim port of `src/doorbell.rs:574-666`.
//! A ring not heard within 120s goes unanswered (digest names it, S3).

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone)]
struct PendingRing {
    npub: String,
    injected_unix: u64,
}

/// Heard-or-deaf: a ring that is not heard is a conflict-shaped event.
#[derive(Debug, Default, Clone)]
pub struct HearTracker {
    pending: BTreeMap<String, PendingRing>,
    unanswered: BTreeSet<String>,
    /// pin → npub, kept after expire so a late post still clears deaf.
    npubs: BTreeMap<String, String>,
}

impl HearTracker {
    pub fn note_inject(&mut self, pin: &str, npub: Option<&str>, now: u64) {
        self.unanswered.remove(pin);
        let npub = npub.unwrap_or("").to_lowercase();
        if !npub.is_empty() {
            self.npubs.insert(pin.to_string(), npub.clone());
        }
        self.pending.insert(
            pin.to_string(),
            PendingRing {
                npub,
                injected_unix: now,
            },
        );
    }

    /// Fill npubs learned later (whois on the 1s tick).
    pub fn remember_npub(&mut self, pin: &str, npub: Option<&str>) {
        let Some(n) = npub.filter(|s| !s.is_empty()) else {
            return;
        };
        let n = n.to_lowercase();
        self.npubs.insert(pin.to_string(), n.clone());
        if let Some(ring) = self.pending.get_mut(pin) {
            ring.npub = n;
        }
    }

    pub fn note_heard_pin(&mut self, pin: &str) {
        self.pending.remove(pin);
        self.unanswered.remove(pin);
    }

    pub fn note_heard_pubkey(&mut self, pubkey: &str) {
        let key = pubkey.to_lowercase();
        let mut pins: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, r)| !r.npub.is_empty() && r.npub == key)
            .map(|(p, _)| p.clone())
            .collect();
        pins.extend(
            self.npubs
                .iter()
                .filter(|(_, n)| *n == &key)
                .map(|(p, _)| p.clone()),
        );
        pins.sort();
        pins.dedup();
        for pin in pins {
            self.note_heard_pin(&pin);
        }
    }

    /// Move timed-out pending pins to unanswered. Returns newly expired pins.
    pub fn expire(&mut self, now: u64, timeout_secs: u64) -> Vec<String> {
        let mut expired = Vec::new();
        self.pending.retain(|pin, ring| {
            if now.saturating_sub(ring.injected_unix) >= timeout_secs {
                expired.push(pin.clone());
                false
            } else {
                true
            }
        });
        for pin in &expired {
            self.unanswered.insert(pin.clone());
        }
        expired
    }

    pub fn unanswered_pins(&self) -> Vec<String> {
        self.unanswered.iter().cloned().collect()
    }

    pub fn seed_unanswered(&mut self, pins: impl IntoIterator<Item = String>) {
        self.unanswered.extend(pins);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heard_clears_and_late_pubkey_clears_deaf() {
        let mut h = HearTracker::default();
        h.note_inject("p", Some("NPUB"), 100);
        h.note_heard_pin("p");
        assert!(h.unanswered_pins().is_empty());
        h.note_inject("q", Some("npub"), 100);
        assert_eq!(h.expire(100 + 120, 120), vec!["q".to_string()]);
        // Late post by pubkey clears the deaf pin via remembered npub.
        h.note_heard_pubkey("NPUB-x");
        h.note_heard_pubkey("npub");
        assert!(h.unanswered_pins().is_empty());
    }
}

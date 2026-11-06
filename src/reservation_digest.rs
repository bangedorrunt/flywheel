//! Pure reservation-conflict digest (bd-coordination-awareness).
//!
//! The doorbell host pushes an advisory notice when two different holders
//! have *live* leases that overlap and at least one is exclusive. Detection
//! is a read of the same board `toron reserve conflicts` exposes; the
//! protocol never waits on a lease (180 s heartbeats make a dead holder
//! self-free).
//!
//! Pure: rows in, conflicts out. No clock, no I/O, no store.

use std::collections::BTreeSet;

use toron_core::mailbox::guard::pattern_matches;
use toron_core::mailbox::reservations::Reservation;

/// One advisory conflict between two holders.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Conflict {
    /// Stable episode key (sorted holders + paths): the doorbell rings once
    /// per episode, not once per sweep.
    pub key: String,
    pub holder_a: String,
    pub holder_b: String,
    pub path_a: String,
    pub path_b: String,
}

impl Conflict {
    /// The line typed to a holder (the other holder named first).
    pub fn line_for(&self, holder: &str) -> String {
        let (other, path, other_path) = if holder == self.holder_a {
            (&self.holder_b, &self.path_a, &self.path_b)
        } else {
            (&self.holder_a, &self.path_b, &self.path_a)
        };
        format!(
            "⟦reserve⟧ conflict with {other} on `{path}` (theirs `{other_path}`) — re-plan/split the \
             file set, or `toron reserve request-release` / `flywheel collab`; never wait on a lease"
        )
    }
}

/// Live overlapping exclusive pairs between different holders, deduped.
///
/// - expired rows are ignored (`expires_ts_us <= now_us`);
/// - same-holder pairs are not conflicts;
/// - shared+shared is not a write block (shared leases do not stop a writer);
/// - overlap is symmetric (`pattern_matches` either direction), so
///   `src/*.rs` and `src/main.rs` conflict.
pub fn live_conflicts(rows: &[Reservation], now_us: i64) -> Vec<Conflict> {
    let live: Vec<&Reservation> = rows
        .iter()
        .filter(|r| r.expires_ts_us.is_none_or(|e| e > now_us))
        .filter(|r| r.exclusive)
        .collect();
    let mut out = BTreeSet::new();
    for (i, a) in live.iter().enumerate() {
        for b in live.iter().skip(i + 1) {
            if a.agent_name == b.agent_name {
                continue;
            }
            let overlap = pattern_matches(&a.path_pattern, &b.path_pattern)
                || pattern_matches(&b.path_pattern, &a.path_pattern);
            if !overlap {
                continue;
            }
            let (holder_a, path_a, holder_b, path_b) =
                if (a.agent_name.as_str(), a.path_pattern.as_str())
                    <= (b.agent_name.as_str(), b.path_pattern.as_str())
                {
                    (
                        a.agent_name.clone(),
                        a.path_pattern.clone(),
                        b.agent_name.clone(),
                        b.path_pattern.clone(),
                    )
                } else {
                    (
                        b.agent_name.clone(),
                        b.path_pattern.clone(),
                        a.agent_name.clone(),
                        a.path_pattern.clone(),
                    )
                };
            let key = format!("{holder_a}|{path_a}||{holder_b}|{path_b}");
            out.insert(Conflict {
                key,
                holder_a,
                holder_b,
                path_a,
                path_b,
            });
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        id: &str,
        holder: &str,
        path: &str,
        exclusive: bool,
        expires: Option<i64>,
    ) -> Reservation {
        Reservation {
            id: id.into(),
            agent_name: holder.into(),
            agent_pubkey: String::new(),
            path_pattern: path.into(),
            exclusive,
            reason: "test".into(),
            expires_ts_us: expires,
            status: toron_core::mailbox::reservations::ReservationStatus::Held,
            epoch: 0,
        }
    }

    #[test]
    fn exclusive_overlap_between_two_holders_is_a_conflict() {
        let rows = [
            row("a", "Alpha", "src/main.rs", true, Some(2_000)),
            row("b", "Beta", "src/*.rs", true, Some(2_000)),
        ];
        let got = live_conflicts(&rows, 1_000);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].line_for("Alpha").contains("Beta"));
        assert!(got[0].line_for("Alpha").contains("request-release"));
    }

    #[test]
    fn shared_shared_same_holder_and_expired_are_not_conflicts() {
        let rows = [
            row("a", "Alpha", "src/main.rs", false, Some(2_000)),
            row("b", "Beta", "src/main.rs", false, Some(2_000)),
        ];
        assert!(live_conflicts(&rows, 1_000).is_empty(), "shared+shared");
        let rows = [
            row("a", "Alpha", "src/main.rs", true, Some(2_000)),
            row("b", "Alpha", "src/lib.rs", true, Some(2_000)),
        ];
        assert!(live_conflicts(&rows, 1_000).is_empty(), "same holder");
        let rows = [
            row("a", "Alpha", "src/main.rs", true, Some(500)),
            row("b", "Beta", "src/main.rs", true, Some(2_000)),
        ];
        assert!(live_conflicts(&rows, 1_000).is_empty(), "expired row");
    }

    #[test]
    fn symmetric_patterns_dedupe_to_one_episode_key() {
        let rows = [
            row("a", "Alpha", "src/*.rs", true, None),
            row("b", "Beta", "src/main.rs", true, None),
            row("c", "Beta", "src/main.rs", true, None),
        ];
        let got = live_conflicts(&rows, 1_000);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].holder_a, "Alpha");
    }
}

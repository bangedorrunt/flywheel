// governed-by: ADR-0012
//! Beads-ledger producer sensor (D7, bd-asr2.15, flag-gated).
//!
//! The mechanism rev 1 assumed but never had: hold expiry, wave-gate
//! transitions, and ledger closes are lazy in `br` (noticed on the next
//! `br` command), so a sleepless daemon would miss them. This sensor polls
//! the br sqlite read-only and emits `Fact` Msgs into the brain.
//!
//! READ-ONLY CONTRACT: `sqlite3 --readonly` + SELECT only. The poller never
//! writes the ledger — no INSERT/UPDATE/DELETE/DROP/DDL, no `br` mutating
//! verb, no JSONL flush. WAL + busy_timeout make concurrent reads safe.
//! `READONLY_ARGV0` + [`READONLY_PREFIXES`] pin the invocation shape; the
//! allowlist test fails the build if a write verb ever appears.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 30s poll cadence (D7: hold expiry wakes within one poll).
pub const POLL_SECS: u64 = 30;

/// The only binary the poller ever spawns.
pub const READONLY_ARGV0: &str = "sqlite3";

/// SELECT (+ dot-commands for timeouts) is the whole vocabulary.
pub const READONLY_PREFIXES: &[&str] = &["SELECT ", "select ", ".timeout "];

/// Holds query: open captain holds with expiry (S1/E10/E17).
pub const HOLDS_QUERY: &str = "SELECT issue_id, corr, expires_at FROM captain_holds WHERE open = 1";

/// Issue change feed: id + status + updated stamp.
pub const ISSUES_QUERY: &str = "SELECT id, status, updated_at FROM issues";

/// One open hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenHold {
    pub issue_id: String,
    pub corr: String,
    pub expires_at: Option<String>,
}

/// One issue row for change-detect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRow {
    pub id: String,
    pub status: String,
    pub updated_at: String,
}

/// Poller snapshot: previous pass state for change-detect. Hold rows carry
/// the pass timestamp they were last seen at, so "newly expired" means
/// expired-now-but-not-expired-then (never re-fires, never misses).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub holds: BTreeMap<(String, String), (Option<String>, String)>,
    pub issues: BTreeMap<String, (String, String)>,
}
/// Deltas that become `Fact` summaries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerDelta {
    /// Newly expired holds (were unexpired/absent, now past `now_rfc3339`).
    pub expired_holds: Vec<(String, String)>,
    /// Issues whose status changed since the last pass.
    pub status_changed: Vec<(String, String, String)>,
    /// Issues updated (any field) since the last pass, excluding pure
    /// status flips already reported above.
    pub touched: Vec<String>,
}

impl LedgerDelta {
    pub fn is_empty(&self) -> bool {
        self.expired_holds.is_empty() && self.status_changed.is_empty() && self.touched.is_empty()
    }

    /// Fact summaries for the brain (one per delta; the tick classifies).
    pub fn fact_summaries(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (issue, corr) in &self.expired_holds {
            out.push(format!("hold expired: {issue} (corr {corr})"));
        }
        for (id, from, to) in &self.status_changed {
            out.push(format!("bead {id} status {from} -> {to}"));
        }
        for id in &self.touched {
            out.push(format!("bead {id} updated"));
        }
        out
    }
}

/// Pure change-detect over one poll's rows (no I/O — unit-tested).
/// `now_rfc3339`: lexicographic compare works on same-shape RFC3339 stamps.
pub fn detect_changes(
    prev: &Snapshot,
    holds: &[OpenHold],
    issues: &[IssueRow],
    now_rfc3339: &str,
) -> (Snapshot, LedgerDelta) {
    let mut next = Snapshot::default();
    let mut delta = LedgerDelta::default();
    for h in holds {
        let key = (h.issue_id.clone(), h.corr.clone());
        let was_expired = prev
            .holds
            .get(&key)
            .and_then(|(exp, seen)| exp.as_deref().map(|e| (e, seen)))
            .is_some_and(|(exp, seen)| exp <= seen.as_str());
        let is_expired = h
            .expires_at
            .as_deref()
            .is_some_and(|exp| exp <= now_rfc3339);
        if is_expired && !was_expired {
            delta
                .expired_holds
                .push((h.issue_id.clone(), h.corr.clone()));
        }
        next.holds
            .insert(key, (h.expires_at.clone(), now_rfc3339.to_string()));
    }
    for issue in issues {
        if let Some((old_status, old_updated)) = prev.issues.get(&issue.id) {
            if old_status != &issue.status {
                delta.status_changed.push((
                    issue.id.clone(),
                    old_status.clone(),
                    issue.status.clone(),
                ));
            } else if old_updated != &issue.updated_at {
                delta.touched.push(issue.id.clone());
            }
        }
        next.issues.insert(
            issue.id.clone(),
            (issue.status.clone(), issue.updated_at.clone()),
        );
    }
    (next, delta)
}

/// Run one read-only poll against `db_path`. Returns row sets; all errors
/// are `None` (a failed poll skips the pass loudly via stderr, never
/// half-state: the snapshot only advances on a complete read).
pub fn poll_once(db_path: &Path) -> Option<(Vec<OpenHold>, Vec<IssueRow>)> {
    let holds_out = readonly_query(db_path, HOLDS_QUERY)?;
    let issues_out = readonly_query(db_path, ISSUES_QUERY)?;
    let mut holds = Vec::new();
    for line in holds_out.lines() {
        let mut cols = line.split('|');
        holds.push(OpenHold {
            issue_id: cols.next().unwrap_or("").to_string(),
            corr: cols.next().unwrap_or("").to_string(),
            expires_at: cols.next().filter(|s| !s.is_empty()).map(str::to_string),
        });
    }
    let mut issues = Vec::new();
    for line in issues_out.lines() {
        let mut cols = line.split('|');
        issues.push(IssueRow {
            id: cols.next().unwrap_or("").to_string(),
            status: cols.next().unwrap_or("").to_string(),
            updated_at: cols.next().unwrap_or("").to_string(),
        });
    }
    Some((holds, issues))
}

fn readonly_query(db_path: &Path, sql: &str) -> Option<String> {
    debug_assert!(
        READONLY_PREFIXES.iter().any(|p| sql.starts_with(p)),
        "poller query outside the read-only allowlist: {sql}"
    );
    let out = std::process::Command::new(READONLY_ARGV0)
        .arg("--readonly")
        .arg(db_path)
        .arg(sql)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Beads project db path (`<repo>/.beads/beads.db`, if present).
pub fn beads_db(repo: &Path) -> Option<PathBuf> {
    let p = repo.join(".beads").join("beads.db");
    p.exists().then(|| p)
}
/// Spawn the 30s poll loop (D7): read-only ledger poll, Fact Msgs into
/// the brain. `br` is never written — see the read-only contract above.
/// Skips passes loudly (stderr) on unreadable state, never half-state.
pub fn spawn_beads_poller(
    tx: tokio::sync::mpsc::Sender<crate::BrainMsg>,
    repo_dir: PathBuf,
) -> crate::SensorHandle {
    let task_name = "beads-poller".to_string();
    let handle = tokio::spawn(async move {
        let mut snapshot = Snapshot::default();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(POLL_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some(db) = beads_db(&repo_dir) else {
                continue;
            };
            let Some((holds, issues)) = poll_once(&db) else {
                eprintln!("doorbell-rt: beads poll unreadable, skipping pass");
                continue;
            };
            let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let (next, delta) = detect_changes(&snapshot, &holds, &issues, &now);
            snapshot = next;
            for summary in delta.fact_summaries() {
                if tx.send(crate::BrainMsg::Fact { summary }).await.is_err() {
                    // Brain gone: report the edge death loudly, then exit.
                    let _ = tx
                        .send(crate::BrainMsg::Degraded {
                            source: task_name.clone(),
                            reason: "poller brain closed".into(),
                        })
                        .await;
                    return;
                }
            }
        }
    });
    crate::SensorHandle {
        name: "beads-poller".into(),
        handle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_readonly_statements_ever() {
        // The allowlist is the contract: any write verb fails the build.
        for q in [HOLDS_QUERY, ISSUES_QUERY] {
            assert!(
                READONLY_PREFIXES.iter().any(|p| q.starts_with(p)),
                "query outside allowlist: {q}"
            );
            for verb in [
                "INSERT", "UPDATE", "DELETE", "DROP", "ALTER", "CREATE", "VACUUM",
            ] {
                let hit = q
                    .to_ascii_uppercase()
                    .split(|c: char| !c.is_alphanumeric())
                    .any(|token| token == verb);
                assert!(!hit, "write verb in poller query: {q}");
            }
        }
        assert_eq!(READONLY_ARGV0, "sqlite3");
    }

    #[test]
    fn hold_expiry_fires_once_on_transition() {
        let prev = Snapshot::default();
        let holds = vec![OpenHold {
            issue_id: "bd-9x".into(),
            corr: "c1".into(),
            expires_at: Some("2026-09-10T03:00:00Z".into()),
        }];
        // Before expiry: silent.
        let (snap, d0) = detect_changes(&prev, &holds, &[], "2026-09-10T02:59:00Z");
        assert!(d0.is_empty());
        // After expiry: exactly one Fact.
        let (_snap2, d1) = detect_changes(&snap, &holds, &[], "2026-09-10T03:00:30Z");
        assert_eq!(
            d1.expired_holds,
            vec![("bd-9x".to_string(), "c1".to_string())]
        );
        assert_eq!(
            d1.fact_summaries(),
            vec!["hold expired: bd-9x (corr c1)".to_string()]
        );
        // Next pass: no re-fire (already expired in snapshot).
        let (_snap3, d2) = detect_changes(&_snap2, &holds, &[], "2026-09-10T03:01:00Z");
        assert!(d2.expired_holds.is_empty(), "expiry fires once");
    }

    #[test]
    fn status_flips_and_touches_detected() {
        let prev = Snapshot {
            holds: Default::default(),
            issues: [("bd-a".to_string(), ("open".to_string(), "t0".to_string()))]
                .into_iter()
                .collect(),
        };
        let issues = vec![
            IssueRow {
                id: "bd-a".into(),
                status: "closed".into(),
                updated_at: "t1".into(),
            },
            IssueRow {
                id: "bd-b".into(),
                status: "open".into(),
                updated_at: "t1".into(),
            },
        ];
        let (snap, d) = detect_changes(&prev, &[], &issues, "t9");
        assert_eq!(
            d.status_changed,
            vec![("bd-a".to_string(), "open".to_string(), "closed".to_string())]
        );
        assert!(d.touched.is_empty(), "new rows are not touches");
        // Second pass with only a timestamp bump: touch, not flip.
        let issues2 = vec![IssueRow {
            id: "bd-a".into(),
            status: "closed".into(),
            updated_at: "t2".into(),
        }];
        let (_snap2, d2) = detect_changes(&snap, &[], &issues2, "t9");
        assert!(d2.status_changed.is_empty());
        assert_eq!(d2.touched, vec!["bd-a".to_string()]);
    }
}

// governed-by: ADR-0012
//! Beads-ledger producer sensor (D7, bd-asr2.15, flag-gated).
//!
//! The mechanism rev 1 assumed but never had: hold expiry, wave-gate
//! transitions, and ledger closes are lazy in `br` (noticed on the next
//! `br` command), so a sleepless daemon would miss them. This sensor polls
//! the br sqlite read-only and emits `Fact` Msgs into the brain.
//!
//! READ-ONLY CONTRACT: SELECT only, and the db is never modified — no
//! INSERT/UPDATE/DELETE/DROP/DDL, no `br` mutating verb, no JSONL flush, no
//! sidecar creation. WAL + busy_timeout make concurrent reads safe.
//! `READONLY_ARGV0` + [`READONLY_PREFIXES`] pin the invocation shape; the
//! allowlist test fails the build if a write verb ever appears.
//!
//! The open shape has TWO faces (bd-ux-papercuts-t2u5, found live): with a
//! live `-wal`, plain `--readonly`; with no `-wal` (a cleanly closed br —
//! the common state), SQLite REFUSES a read-only open because it may not
//! create the missing `-shm`, so the poll reads through `immutable=1` —
//! exact when no wal exists, stale if one did, hence the gate.

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

/// Issue change feed: id + status + updated stamp + blocked flag. The join
/// is the `br ready` predicate (open AND not in the blocked cache) — the
/// poller reads the SAME materialized view `br ready` reads, so a ready
/// appearance cannot disagree with the board.
pub const ISSUES_QUERY: &str = "SELECT i.id, i.status, i.updated_at, (b.issue_id IS NOT NULL) FROM issues i LEFT JOIN blocked_issues_cache b ON b.issue_id = i.id";

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
    /// in `blocked_issues_cache` (the same gate `br ready` applies)
    pub blocked: bool,
}

/// Last-seen issue state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueState {
    pub status: String,
    pub updated_at: String,
    pub blocked: bool,
}

/// Poller snapshot: previous pass state for change-detect. Hold rows carry
/// the pass timestamp they were last seen at, so "newly expired" means
/// expired-now-but-not-expired-then (never re-fires, never misses).
///
/// `primed` is the baseline gate: the first pass only records the board
/// (a daemon restart must not re-announce every open bead as "ready").
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub holds: BTreeMap<(String, String), (Option<String>, String)>,
    pub issues: BTreeMap<String, IssueState>,
    pub primed: bool,
}
/// Deltas that become `Fact` summaries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerDelta {
    /// Newly expired holds (were unexpired/absent, now past `now_rfc3339`).
    pub expired_holds: Vec<(String, String)>,
    /// Issues whose status changed since the last pass.
    pub status_changed: Vec<(String, String, String)>,
    /// Ready-bead appearances (bd-ux-papercuts-t2u5): a bead that was not
    /// ready before and is ready now — newly present (open, unblocked) or
    /// unblocked in place. Gated by `primed` so only real transitions fire.
    pub ready_appeared: Vec<String>,
    /// Issues updated (any field) since the last pass, excluding pure
    /// status flips and ready appearances already reported above.
    pub touched: Vec<String>,
}

impl LedgerDelta {
    pub fn is_empty(&self) -> bool {
        self.expired_holds.is_empty()
            && self.status_changed.is_empty()
            && self.ready_appeared.is_empty()
            && self.touched.is_empty()
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
        for id in &self.ready_appeared {
            out.push(format!("bead {id} ready"));
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
        let ready_now = issue.status == "open" && !issue.blocked;
        match prev.issues.get(&issue.id) {
            Some(old) => {
                if old.status != issue.status {
                    delta.status_changed.push((
                        issue.id.clone(),
                        old.status.clone(),
                        issue.status.clone(),
                    ));
                } else if old.updated_at != issue.updated_at {
                    delta.touched.push(issue.id.clone());
                }
                // A bead that was blocked and is now open+unblocked BECAME
                // ready. One transition, one fact: a ready appearance
                // outranks the weaker `updated` touch.
                if prev.primed && ready_now && old.blocked && old.status == issue.status {
                    delta.ready_appeared.push(issue.id.clone());
                    delta.touched.retain(|t| t != &issue.id);
                }
            }
            // New row: a fresh open+unblocked bead is a ready appearance.
            None => {
                if prev.primed && ready_now {
                    delta.ready_appeared.push(issue.id.clone());
                }
                // First pass (`!primed`) stays silent: it is the baseline.
            }
        }
        next.issues.insert(
            issue.id.clone(),
            IssueState {
                status: issue.status.clone(),
                updated_at: issue.updated_at.clone(),
                blocked: issue.blocked,
            },
        );
    }
    next.primed = true;
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
            blocked: cols.next().map(|c| c.trim() == "1").unwrap_or(false),
        });
    }
    Some((holds, issues))
}

/// `<db>-wal`, the sidecar whose presence decides the open shape.
fn wal_sidecar(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push("-wal");
    PathBuf::from(s)
}

/// Percent-escape the three characters that would break a `file:` URI.
fn uri_escape(path: &str) -> String {
    path.replace('%', "%25")
        .replace('?', "%3F")
        .replace('#', "%23")
}

fn readonly_query(db_path: &Path, sql: &str) -> Option<String> {
    debug_assert!(
        READONLY_PREFIXES.iter().any(|p| sql.starts_with(p)),
        "poller query outside the read-only allowlist: {sql}"
    );
    let mut cmd = std::process::Command::new(READONLY_ARGV0);
    if wal_sidecar(db_path).exists() {
        // Live (or un-recovered) WAL: read through it plainly. A plain
        // read-only open still works because a writer's -shm exists.
        cmd.arg("--readonly").arg(db_path);
    } else {
        // Cleanly closed WAL db: the wal is gone, so every committed page
        // lives in the main file, and `--readonly` cannot open it at all
        // (SQLite may not create the missing -shm — found live 2026-10-04).
        // immutable=1 reads exactly this state; it would read STALE with a
        // wal present, which is why it is gated on the sidecar above.
        cmd.arg(format!(
            "file:{}?immutable=1",
            uri_escape(&db_path.to_string_lossy())
        ));
    }
    let out = cmd.arg(sql).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Beads project db path (`<repo>/.beads/beads.db`, if present).
pub fn beads_db(repo: &Path) -> Option<PathBuf> {
    let p = repo.join(".beads").join("beads.db");
    p.exists().then_some(p)
}

/// Repo dir for the beads poller when hosting `project` (bd-ux-papercuts-t2u5).
/// An absolute project key IS the repo (flywheel's `project_dir` shape); a
/// slug resolves under `workspace_root`, where flywheel's default config
/// (`~/workspace`) puts it. `None` = no board to poll — callers skip the
/// sensor and say so, never point it at an unrelated cwd.
pub fn beads_repo_for(project: &str, workspace_root: &Path) -> Option<PathBuf> {
    let p = Path::new(project);
    let cand = if p.is_absolute() {
        p.to_path_buf()
    } else {
        workspace_root.join(project)
    };
    cand.join(".beads").is_dir().then_some(cand)
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

/// External re-ring cadence (E9: a foreign close wakes within ~one pass).
pub const RERING_SECS: u64 = 5;

/// Foreign issue status lookup (read-only; same allowlist — the bead id is
/// single-quoted into the predicate, so ids containing `'` are refused).
pub const FOREIGN_STATUS_PREFIX: &str = "SELECT status FROM issues WHERE id = '";

/// Parse an external-block corr (`external:<project>:<bead>`).
pub fn parse_external_corr(corr: &str) -> Option<(String, String)> {
    let rest = corr.strip_prefix("external:")?;
    let (proj, bead) = rest.split_once(':')?;
    if proj.is_empty() || bead.is_empty() || bead.contains('\'') {
        return None;
    }
    Some((proj.to_string(), bead.to_string()))
}

/// Pure re-ring detect: open holds parked behind `external:<proj>:<bead>`
/// whose foreign issue reads `closed` and that have not rung yet. Returns
/// Fact summaries; `rung` gains the fired corrs (fire-once per process —
/// restart may re-ring, at-least-once per P2 honesty).
pub fn detect_external_closed(
    holds: &[OpenHold],
    status_of: &dyn Fn(&str, &str) -> Option<String>,
    rung: &mut std::collections::HashSet<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for h in holds {
        let Some((fproj, fbead)) = parse_external_corr(&h.corr) else {
            continue;
        };
        if rung.contains(&h.corr) {
            continue;
        }
        if status_of(&fproj, &fbead).as_deref() != Some("closed") {
            continue;
        }
        rung.insert(h.corr.clone());
        out.push(format!(
            "external {fproj}:{fbead} closed — re-ring {} (corr {})",
            h.issue_id, h.corr
        ));
    }
    out
}

/// Spawn the external re-ring loop (E9, bd-4gco): read-only own-holds poll
/// plus read-only foreign-status lookups, Fact Msgs into the brain. Never
/// writes any ledger (resolving the hold is the waiter's job); unknown
/// foreign projects are skipped loudly-once per pass.
pub fn spawn_external_rering(
    tx: tokio::sync::mpsc::Sender<crate::BrainMsg>,
    repo_dir: PathBuf,
    foreign: Vec<(String, PathBuf)>,
) -> crate::SensorHandle {
    let task_name = "external-rering".to_string();
    let handle = tokio::spawn(async move {
        let mut rung = std::collections::HashSet::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(RERING_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some(db) = beads_db(&repo_dir) else {
                continue;
            };
            let Some((holds, _)) = poll_once(&db) else {
                eprintln!("doorbell-rt: rering poll unreadable, skipping pass");
                continue;
            };
            let status_of = |fp: &str, fb: &str| -> Option<String> {
                let fdir = foreign.iter().find(|(k, _)| k == fp)?.1.clone();
                let fdb = beads_db(&fdir)?;
                let sql = format!("{FOREIGN_STATUS_PREFIX}{fb}'");
                readonly_query(&fdb, &sql).map(|s| s.trim().to_string())
            };
            for summary in detect_external_closed(&holds, &status_of, &mut rung) {
                if tx.send(crate::BrainMsg::Fact { summary }).await.is_err() {
                    let _ = tx
                        .send(crate::BrainMsg::Degraded {
                            source: task_name.clone(),
                            reason: "rering brain closed".into(),
                        })
                        .await;
                    return;
                }
            }
        }
    });
    crate::SensorHandle {
        name: "external-rering".into(),
        handle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_readonly_statements_ever() {
        for q in [HOLDS_QUERY, ISSUES_QUERY, FOREIGN_STATUS_PREFIX] {
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
            issues: [(
                "bd-a".to_string(),
                IssueState {
                    status: "open".to_string(),
                    updated_at: "t0".to_string(),
                    blocked: false,
                },
            )]
            .into_iter()
            .collect(),
            primed: true,
        };
        let issues = vec![
            IssueRow {
                id: "bd-a".into(),
                status: "closed".into(),
                updated_at: "t1".into(),
                blocked: false,
            },
            IssueRow {
                id: "bd-b".into(),
                status: "open".into(),
                updated_at: "t1".into(),
                blocked: false,
            },
        ];
        let (snap, d) = detect_changes(&prev, &[], &issues, "t9");
        assert_eq!(
            d.status_changed,
            vec![("bd-a".to_string(), "open".to_string(), "closed".to_string())]
        );
        assert!(d.touched.is_empty(), "new rows are not touches");
        assert_eq!(
            d.ready_appeared,
            vec!["bd-b".to_string()],
            "a fresh open+unblocked row is a ready appearance"
        );
        // Second pass with only a timestamp bump: touch, not flip.
        let issues2 = vec![IssueRow {
            id: "bd-a".into(),
            status: "closed".into(),
            updated_at: "t2".into(),
            blocked: false,
        }];
        let (_snap2, d2) = detect_changes(&snap, &[], &issues2, "t9");
        assert!(d2.status_changed.is_empty());
        assert_eq!(d2.touched, vec!["bd-a".to_string()]);
    }

    fn issue(id: &str, status: &str, updated_at: &str, blocked: bool) -> IssueRow {
        IssueRow {
            id: id.into(),
            status: status.into(),
            updated_at: updated_at.into(),
            blocked,
        }
    }

    // bd-ux-papercuts-t2u5: the ready-bead sensor. The first pass is a
    // baseline (no restart spam); afterwards a new open+unblocked bead is
    // exactly one `bead <id> ready` fact, and it never re-fires.
    #[test]
    fn ready_appearance_after_prime_fires_once() {
        let seed = vec![issue("bd-old", "closed", "t0", false)];
        let (snap, d0) = detect_changes(&Snapshot::default(), &[], &seed, "t1");
        assert!(d0.is_empty(), "first pass primes silently: {d0:?}");
        assert!(snap.primed);
        let board = vec![
            issue("bd-old", "closed", "t0", false),
            issue("bd-new", "open", "t2", false),
        ];
        let (snap2, d1) = detect_changes(&snap, &[], &board, "t2");
        assert_eq!(d1.ready_appeared, vec!["bd-new".to_string()]);
        assert_eq!(d1.fact_summaries(), vec!["bead bd-new ready".to_string()]);
        // Same board next pass: silence — an appearance is a transition.
        let (_snap3, d2) = detect_changes(&snap2, &[], &board, "t3");
        assert!(d2.is_empty(), "appearance fires once: {d2:?}");
    }

    // A bead that appears BLOCKED is not ready; the moment it unblocks it is.
    #[test]
    fn blocked_bead_is_silent_until_unblocked() {
        let (snap, _) = detect_changes(&Snapshot::default(), &[], &[], "t0");
        let blocked = vec![issue("bd-x", "open", "t1", true)];
        let (snap2, d1) = detect_changes(&snap, &[], &blocked, "t1");
        assert!(d1.is_empty(), "blocked is not ready: {d1:?}");
        let unblocked = vec![issue("bd-x", "open", "t2", false)];
        let (_snap3, d2) = detect_changes(&snap2, &[], &unblocked, "t2");
        assert_eq!(d2.ready_appeared, vec!["bd-x".to_string()]);
        assert!(d2.touched.is_empty(), "one transition, one fact: {d2:?}");
        assert_eq!(d2.fact_summaries(), vec!["bead bd-x ready".to_string()]);
    }

    // The first pass over an existing board must not announce it as new.
    #[test]
    fn prime_pass_does_not_announce_existing_board() {
        let board = vec![
            issue("bd-a", "open", "t0", false),
            issue("bd-b", "open", "t0", false),
            issue("bd-c", "closed", "t0", false),
        ];
        let (_snap, d) = detect_changes(&Snapshot::default(), &[], &board, "t1");
        assert!(d.is_empty(), "baseline is silent: {d:?}");
    }

    #[test]
    fn beads_repo_for_absolute_key_is_the_repo() {
        let root = std::env::temp_dir().join(format!("dbll-repo-abs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("proj");
        std::fs::create_dir_all(repo.join(".beads")).unwrap();
        assert_eq!(
            beads_repo_for(repo.to_str().unwrap(), &root),
            Some(repo.clone()),
            "an absolute project key IS the repo"
        );
        assert_eq!(
            beads_repo_for("missing", &root),
            None,
            "a slug without a board resolves to nothing"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn beads_repo_for_slug_resolves_under_workspace_root() {
        let root = std::env::temp_dir().join(format!("dbll-repo-slug-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("slugproj");
        std::fs::create_dir_all(repo.join(".beads")).unwrap();
        assert_eq!(beads_repo_for("slugproj", &root), Some(repo.clone()));
        let _ = std::fs::remove_dir_all(&root);
    }

    // bd-ux-papercuts-t2u5, found live: a cleanly closed br leaves a WAL db
    // with NO -shm, and SQLite refuses a read-only open of exactly that state
    // ("unable to open database file") — every quiet project skipped every
    // pass. The poller must read it via immutable=1, and must not
    // materialize sidecars doing it.
    #[test]
    fn readonly_query_reads_cleanly_closed_wal_db() {
        let base = std::env::temp_dir().join(format!("dbll-walro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let db = base.join("beads.db");
        let st = std::process::Command::new("sqlite3")
            .arg(&db)
            .arg("PRAGMA journal_mode=WAL; CREATE TABLE t(v TEXT); INSERT INTO t VALUES('x');")
            .status()
            .expect("sqlite3 CLI builds the fixture");
        assert!(st.success(), "fixture setup failed");
        // The raw CLI leaves a size-0 -wal/-shm pair; br removes both on
        // clean close, so the sidecar-free db is the state a quiet project
        // actually sits in (that live state is what the daemon failed on).
        let wal = wal_sidecar(&db);
        let _ = std::fs::remove_file(&wal);
        let _ = std::fs::remove_file(db.with_file_name("beads.db-shm"));
        assert!(!wal.exists(), "fixture must model the sidecar-free state");
        let out = readonly_query(&db, "SELECT v FROM t");
        assert_eq!(out.as_deref(), Some("x\n"));
        assert!(!wal.exists(), "a read must not materialize a wal");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn external_corr_parses_and_rejects_quotes() {
        assert_eq!(
            parse_external_corr("external:fp:bd-1"),
            Some(("fp".to_string(), "bd-1".to_string()))
        );
        assert_eq!(parse_external_corr("local:c1"), None);
        assert_eq!(parse_external_corr("external:only"), None);
        assert_eq!(parse_external_corr("external:p:b'd-1"), None);
    }

    #[test]
    fn external_closed_fires_once_open_skipped() {
        let holds = vec![
            OpenHold {
                issue_id: "bd-a".into(),
                corr: "external:fp:bd-1".into(),
                expires_at: None,
            },
            OpenHold {
                issue_id: "bd-b".into(),
                corr: "external:fp:bd-2".into(),
                expires_at: None,
            },
            OpenHold {
                issue_id: "bd-c".into(),
                corr: "local:c9".into(),
                expires_at: None,
            },
        ];
        let status_of = |_: &str, bead: &str| -> Option<String> {
            Some(if bead == "bd-1" {
                "closed".into()
            } else {
                "open".into()
            })
        };
        let mut rung = std::collections::HashSet::new();
        let out = detect_external_closed(&holds, &status_of, &mut rung);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("external fp:bd-1 closed"),
            "re-ring names foreign close"
        );
        assert!(out[0].contains("bd-a"), "re-ring names the waiter");
        // Second pass: rung, silent.
        let out2 = detect_external_closed(&holds, &status_of, &mut rung);
        assert!(out2.is_empty(), "fires once");
    }

    fn mk_beads_db(dir: &std::path::Path, holds_sql: &str, issues_sql: &str) {
        let beads = dir.join(".beads");
        std::fs::create_dir_all(&beads).unwrap();
        let db = beads.join("beads.db");
        let setup = format!(
            "CREATE TABLE captain_holds(issue_id TEXT, corr TEXT, expires_at TEXT, open INTEGER);\
             CREATE TABLE issues(id TEXT, status TEXT, updated_at TEXT);\
             CREATE TABLE blocked_issues_cache(issue_id TEXT PRIMARY KEY, blocked_by TEXT NOT NULL DEFAULT '');\
             {holds_sql}{issues_sql}"
        );
        let st = std::process::Command::new("sqlite3")
            .arg(&db)
            .arg(&setup)
            .status()
            .expect("sqlite3 CLI builds scratch ledger");
        assert!(st.success(), "scratch ledger setup failed");
    }

    #[tokio::test]
    async fn e9_foreign_close_rerings_within_one_pass() {
        // E9 live at sensor level: real sqlite ledgers, real 5s loop, no
        // swarm. Foreign bd-1 flips open -> closed mid-run; exactly one
        // Fact must arrive within ~2 passes.
        let base = std::env::temp_dir().join(format!("dbll-e9-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let own = base.join("own");
        let fp = base.join("fp");
        mk_beads_db(
            &own,
            "INSERT INTO captain_holds VALUES('bd-a','external:fp:bd-1','',1);",
            "INSERT INTO issues VALUES('bd-a','blocked','t0');",
        );
        mk_beads_db(&fp, "", "INSERT INTO issues VALUES('bd-1','open','t0');");
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let _sensor = spawn_external_rering(tx, own.clone(), vec![("fp".to_string(), fp.clone())]);
        // Pass 1 (foreign open): silence for one full cadence.
        let quiet =
            tokio::time::timeout(std::time::Duration::from_secs(RERING_SECS + 2), rx.recv()).await;
        assert!(quiet.is_err(), "no wake while foreign is open");
        // Foreign closes legally mid-run.
        let fdb = fp.join(".beads").join("beads.db");
        let st = std::process::Command::new("sqlite3")
            .arg(&fdb)
            .arg("UPDATE issues SET status='closed' WHERE id='bd-1';")
            .status()
            .unwrap();
        assert!(st.success());
        // One wake within ~2 passes of the close.
        let got = tokio::time::timeout(
            std::time::Duration::from_secs(2 * RERING_SECS + 3),
            rx.recv(),
        )
        .await
        .expect("E9: no re-ring wake within two passes of foreign close");
        let msg = got.expect("sensor died");
        match msg {
            crate::BrainMsg::Fact { summary } => {
                assert!(
                    summary.contains("external fp:bd-1 closed"),
                    "wake names foreign close: {summary}"
                );
                assert!(summary.contains("bd-a"), "wake names waiter: {summary}");
            }
            other => panic!("expected Fact wake, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn e10_hold_expiry_wakes_with_zero_swarm() {
        // E10/S1 live at sensor level: real sqlite ledger with an already-
        // expired hold, real 30s loop, no swarm. One Fact within ~one pass.
        let base = std::env::temp_dir().join(format!("dbll-e10-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let own = base.join("own");
        mk_beads_db(
            &own,
            "INSERT INTO captain_holds VALUES('bd-9x','c1','2026-09-10T03:00:00Z',1);",
            "INSERT INTO issues VALUES('bd-9x','blocked','t0');",
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let _sensor = spawn_beads_poller(tx, own.clone());
        let got = tokio::time::timeout(std::time::Duration::from_secs(POLL_SECS + 10), rx.recv())
            .await
            .expect("E10: no expiry wake within one pass");
        match got.expect("sensor died") {
            crate::BrainMsg::Fact { summary } => {
                assert!(summary.contains("bd-9x"), "wake names hold: {summary}");
                assert!(summary.contains("c1"), "wake carries corr: {summary}");
            }
            other => panic!("expected Fact wake, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}

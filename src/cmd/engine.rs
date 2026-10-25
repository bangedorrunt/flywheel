//! Engine-store maintenance.
//!
//! `resonate serve` owns the sqlite schema, and it has no retention flag: every
//! settled promise stays forever. At ~1.8k promises a day the file reached
//! 256 MB, 90% of it rows the engine will never read again. This module only
//! deletes rows the server itself marks settled (`settled_at` set) and never
//! invents a migration, so an upstream schema change fails the guard loudly.
//!
//! Safety: the server must not be answering /health unless `--force` is given,
//! because deleting rows under a live engine can resurrect stale state. The
//! delete is one transaction, a backup copy is written first, and `VACUUM`
//! only runs after it commits.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use serde_json::json;

use crate::config::Config;
use crate::output::print_json;

/// Default horizon: long enough that a replay of an in-flight workflow still
/// finds its promises, short enough that a year of cron ticks is not kept.
const DEFAULT_OLDER_THAN_DAYS: u64 = 30;

#[derive(Subcommand)]
pub(crate) enum EngineCmd {
    /// Count or delete settled promises older than a horizon, then VACUUM.
    ///
    /// Without `--yes` this only reports. Refuses while the engine answers
    /// /health unless `--force` is given.
    Gc {
        /// Delete promises settled before now minus this many days
        #[arg(long, default_value_t = DEFAULT_OLDER_THAN_DAYS)]
        older_than_days: u64,
        /// sqlite file (default: `<state>/resonate.db`)
        #[arg(long)]
        db: Option<PathBuf>,
        /// Report only; writes nothing (the default without --yes)
        #[arg(long)]
        dry_run: bool,
        /// Required to delete
        #[arg(long)]
        yes: bool,
        /// Proceed even when the engine answers /health (unsafe)
        #[arg(long)]
        force: bool,
    },
}

pub(crate) fn cmd_engine(cfg: &Config, cmd: &EngineCmd) -> Result<()> {
    match cmd {
        EngineCmd::Gc {
            older_than_days,
            db,
            dry_run,
            yes,
            force,
        } => {
            let db = db
                .clone()
                .unwrap_or_else(|| crate::config::state_dir().join("resonate.db"));
            if !db.exists() {
                bail!("engine store {} does not exist", db.display());
            }
            let url = cfg
                .resonate_url()
                .unwrap_or_else(|| "http://127.0.0.1:8001".to_string());
            // A remote engine answers /health but does not serve this local
            // file; only a loopback url can be this store's host. A recent
            // write is the fallback signal for a local server on another port.
            if !force && crate::daemon::is_loopback_host(&url) && engine_healthy(&url) {
                bail!(
                    "engine answers {url}/health; stop it before pruning {} (or pass --force, which is unsafe: the live engine can re-serve rows this command deletes)",
                    db.display()
                );
            }
            if !force && touched_within(&db, 300)? {
                bail!(
                    "{} was written in the last 5 minutes; a local engine may be using it. Stop it, then retry (or pass --force, which deletes under a live server)",
                    db.display()
                );
            }
            let cutoff_ms = cutoff_ms(*older_than_days)?;
            let apply = *yes && !*dry_run;
            let report = prune(&db, cutoff_ms, apply)?;
            print_json(&json!({
                "db": report.db.display().to_string(),
                "older_than_days": older_than_days,
                "cutoff_ms": cutoff_ms,
                "prunable_promises": report.promises,
                "size_before": report.size_before,
                "size_after": report.size_after,
                "applied": apply,
                "backup": report.backup.as_ref().map(|p| p.display().to_string()),
                "hint": if apply { serde_json::Value::Null } else { json!("pass --yes to delete") },
            }));
            Ok(())
        }
    }
}

fn touched_within(db: &Path, secs: u64) -> Result<bool> {
    let modified = std::fs::metadata(db)
        .and_then(|m| m.modified())
        .context("read engine store mtime")?;
    Ok(SystemTime::now()
        .duration_since(modified)
        .map(|age| age.as_secs() < secs)
        .unwrap_or(true))
}

/// Health probe shaped like `daemon::resonate_healthy`: 2xx means up.
fn engine_healthy(url: &str) -> bool {
    matches!(
        ureq::get(&format!("{url}/health"))
            .timeout(std::time::Duration::from_secs(2))
            .call(),
        Ok(resp) if (200..300).contains(&resp.status())
    )
}

fn cutoff_ms(days: u64) -> Result<i64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before unix epoch")?
        .as_millis();
    let span = u128::from(days).saturating_mul(86_400_000);
    i64::try_from(now.saturating_sub(span)).context("cutoff does not fit in i64")
}

#[derive(Debug)]
struct GcReport {
    db: PathBuf,
    promises: u64,
    size_before: u64,
    size_after: u64,
    backup: Option<PathBuf>,
}

/// The one destructive step, split out so the test exercises the same SQL the
/// command runs.
fn prune(db: &Path, cutoff_ms: i64, apply: bool) -> Result<GcReport> {
    let columns = sqlite(db, "select name from pragma_table_info('promises');")?;
    let has = |name: &str| columns.lines().any(|line| line.trim() == name);
    if !has("settled_at") || !has("state") {
        bail!(
            "{} has no promises(settled_at, state); refusing to touch a schema this command does not know",
            db.display()
        );
    }

    // A settled promise can still be the `awaited_id` of a callback row whose
    // awaiter has not loaded the value yet; those ids are never pruned.
    let where_clause = format!(
        "WHERE settled_at IS NOT NULL AND settled_at < {cutoff_ms} AND id NOT IN (SELECT awaited_id FROM callbacks)"
    );
    let promises: u64 = sqlite(db, &format!("select count(*) from promises {where_clause}"))?
        .parse()
        .context("count(*) did not parse")?;
    let size_before = db_size(db)?;

    if !apply {
        return Ok(GcReport {
            db: db.to_path_buf(),
            promises,
            size_before,
            size_after: size_before,
            backup: None,
        });
    }

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let backup = db.with_file_name(format!(
        "{}.bak-{stamp}",
        db.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::copy(db, &backup)
        .with_context(|| format!("back up {} to {}", db.display(), backup.display()))?;

    // foreign_keys is per connection in the sqlite3 CLI and defaults OFF, so it
    // must be ON before BEGIN for the tasks/timeouts cascades to fire. A
    // callback row still names an awaited promise whose value an awaiter may
    // load, so those ids are excluded even when the row is settled.
    let script = format!(
        "PRAGMA foreign_keys=ON;\nBEGIN IMMEDIATE;\nDELETE FROM promises {where_clause};\nCOMMIT;\nVACUUM;\nPRAGMA wal_checkpoint(TRUNCATE);\n"
    );
    let mut child = Command::new("sqlite3")
        .arg(db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "run sqlite3 on {} (install sqlite3 to use engine gc)",
                db.display()
            )
        })?;
    {
        let mut stdin = child.stdin.take().context("sqlite3 stdin")?;
        stdin.write_all(script.as_bytes())?;
    }
    let out = child.wait_with_output().context("wait for sqlite3")?;
    if !out.status.success() {
        bail!(
            "sqlite3 {} refused the prune: {}",
            db.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    Ok(GcReport {
        db: db.to_path_buf(),
        promises,
        size_before,
        size_after: db_size(db)?,
        backup: Some(backup),
    })
}

fn db_size(db: &Path) -> Result<u64> {
    let mut total = std::fs::metadata(db).map(|m| m.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        let side = db.with_file_name(format!(
            "{}{suffix}",
            db.file_name().unwrap_or_default().to_string_lossy()
        ));
        total += std::fs::metadata(side).map(|m| m.len()).unwrap_or(0);
    }
    Ok(total)
}

fn sqlite(db: &Path, sql: &str) -> Result<String> {
    let out = Command::new("sqlite3")
        .arg(db)
        .arg(sql)
        .output()
        .with_context(|| format!("run sqlite3 on {}", db.display()))?;
    if !out.status.success() {
        bail!(
            "sqlite3 {} failed: {}",
            db.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store mirrors the vendor's shape: tasks and timeouts cascade from
    /// promises, pending rows must survive, and a row settled after the cutoff
    /// must survive.
    #[test]
    fn prune_removes_only_settled_rows_before_the_cutoff() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("resonate.db");
        let ddl = "\
            CREATE TABLE promises (id TEXT PRIMARY KEY, state TEXT NOT NULL, settled_at BIGINT);
            CREATE TABLE tasks (id TEXT PRIMARY KEY REFERENCES promises(id) ON DELETE CASCADE, state TEXT NOT NULL);
            CREATE TABLE task_timeouts (timeout_at BIGINT NOT NULL, id TEXT PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE);
            CREATE TABLE callbacks (awaited_id TEXT NOT NULL REFERENCES promises(id) ON DELETE CASCADE, awaiter_id TEXT NOT NULL REFERENCES promises(id) ON DELETE CASCADE, ready BOOLEAN NOT NULL DEFAULT false, PRIMARY KEY (awaited_id, awaiter_id));
            INSERT INTO promises VALUES ('old1','resolved',1000),('old2','rejected_timedout',2000),('new','resolved',9000),('pending','pending',NULL);
            INSERT INTO tasks VALUES ('old1','fulfilled'),('old2','fulfilled'),('new','fulfilled'),('pending','pending');
            INSERT INTO task_timeouts VALUES (1,'old1'),(2,'new');
            INSERT INTO callbacks VALUES ('old2','pending',0);
        ";
        sqlite(&db, ddl).unwrap();

        // Cutoff 5000: old1 goes; old2 is settled but a callback still awaits
        // it, so it stays with new and pending.
        let report = prune(&db, 5000, true).unwrap();
        assert_eq!(report.promises, 1);
        assert!(report.backup.as_ref().is_some_and(|p| p.exists()));
        assert_eq!(
            sqlite(&db, "select group_concat(id) from promises order by id;").unwrap(),
            "new,old2,pending"
        );
        assert_eq!(
            sqlite(&db, "select group_concat(id) from tasks order by id;").unwrap(),
            "new,old2,pending",
            "task rows cascade with their promise; old2 is protected"
        );
        assert_eq!(
            sqlite(&db, "select count(*) from task_timeouts;").unwrap(),
            "1"
        );

        // Dry run counts the same rows but writes nothing. Clearing the
        // callback releases old2 for the next prune.
        sqlite(&db, "DELETE FROM callbacks;").unwrap();
        let report = prune(&db, 5000, false).unwrap();
        assert_eq!(
            report.promises, 1,
            "old2 is prunable once nothing awaits it"
        );
        assert_eq!(sqlite(&db, "select count(*) from promises;").unwrap(), "3");
    }

    #[test]
    fn prune_refuses_an_unknown_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("other.db");
        sqlite(&db, "CREATE TABLE promises (id TEXT PRIMARY KEY);").unwrap();
        let err = prune(&db, 0, false).unwrap_err().to_string();
        assert!(err.contains("no promises(settled_at, state)"), "{err}");
    }
}

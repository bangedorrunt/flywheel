//! Headless run logs (ADR-0010 Spec E, P10.1): `runs/<run_id>.jsonl`,
//! one JSON object per run, under the state dir (`~/.local/state/flywheel/runs/`).
//!
//! The leaf `__run-headless` writes; `flywheel runs show <id>` reads; the
//! digest aggregates failures. Full response lives in the entry itself
//! (the sidecar split of the ADR is deferred — entries stay scannable).
//!
//! governed-by: ADR-0010 (Spec E)

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One headless run, as written to `runs/<run_id>.jsonl` (ADR-0010 Spec E
/// run-log schema).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLogEntry {
    pub run_id: String,
    pub ts: String,
    pub workflow: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    pub profile: String,
    pub harness: String,
    pub model: String,
    pub mode: String,
    pub prompt_hash: String,
    pub response: String,
    pub status: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub duration_ms: u64,
    pub exit_code: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The directory holding run logs: `$STATE/flywheel/runs`.
pub fn runs_dir() -> PathBuf {
    crate::config::state_dir().join("runs")
}

/// Append one run entry to `runs/<run_id>.jsonl` (create the dir on first
/// write). Idempotent per run_id: an existing file is replaced, never
/// duplicated.
pub fn write_run(entry: &RunLogEntry) -> Result<()> {
    let dir = runs_dir();
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{}.jsonl", entry.run_id));
    let line = serde_json::to_string(entry)?;
    fs::write(&path, line + "\n").with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// All run entries across `runs/`, newest first (by `ts`).
pub fn read_runs() -> Vec<RunLogEntry> {
    let dir = runs_dir();
    let Ok(rd) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(e) = serde_json::from_str::<RunLogEntry>(line) {
                out.push(e);
            }
        }
    }
    out.sort_by(|a, b| b.ts.cmp(&a.ts));
    out
}

/// Find one run by id.
pub fn find_run(run_id: &str) -> Option<RunLogEntry> {
    let path = runs_dir().join(format!("{run_id}.jsonl"));
    let raw = fs::read_to_string(path).ok()?;
    raw.lines().find_map(|l| serde_json::from_str(l).ok())
}

/// Aggregate: failed runs + ids, since an optional ISO cutoff (inclusive).
pub fn failures_since(cutoff: Option<&str>) -> Vec<RunLogEntry> {
    read_runs()
        .into_iter()
        .filter(|e| e.status == "fail" && cutoff.map(|c| e.ts.as_str() >= c).unwrap_or(true))
        .collect()
}

/// A stable run id for a headless run: `hl-<iso>-<suffix>`.
pub fn new_run_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("hl-{}-{:09}", crate::state::now_iso(), nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(run_id: &str) -> RunLogEntry {
        RunLogEntry {
            run_id: run_id.into(),
            ts: "2026-08-09T10:00:00Z".into(),
            workflow: "beads".into(),
            cron: None,
            profile: "planner".into(),
            harness: "pi".into(),
            model: "claude-sonnet-4".into(),
            mode: "headless".into(),
            prompt_hash: "deadbeef".into(),
            response: "full response text".into(),
            status: "success".into(),
            tokens_in: 120,
            tokens_out: 40,
            duration_ms: 1500,
            exit_code: 0,
            error: None,
        }
    }

    /// An entry survives a write → read roundtrip byte-for-byte (the leaf
    /// writes what `runs show` prints).
    #[test]
    fn write_then_find_roundtrips() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-runlog-{}", uuid::Uuid::new_v4()));
        // env mutation is unsafe in edition 2024 (matches stages.rs tests)
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        let entry = sample_entry("hl-1");
        write_run(&entry).expect("write");
        let got = find_run("hl-1").expect("find");
        assert_eq!(got, entry, "roundtrip must be identical");
    }

    /// Multiple entries across files are all listed, newest first.
    #[test]
    fn read_runs_lists_all_entries_newest_first() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-runlog-{}", uuid::Uuid::new_v4()));
        // env mutation is unsafe in edition 2024 (matches stages.rs tests)
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        let mut old = sample_entry("hl-a");
        old.ts = "2026-08-09T09:00:00Z".into();
        let mut new = sample_entry("hl-b");
        new.ts = "2026-08-09T11:00:00Z".into();
        write_run(&old).expect("write old");
        write_run(&new).expect("write new");
        let all = read_runs();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].run_id, "hl-b", "newest first");
        assert_eq!(all[1].run_id, "hl-a");
    }

    /// Unknown id → None (drives the `runs show` fallback).
    #[test]
    fn find_returns_none_for_unknown() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-runlog-{}", uuid::Uuid::new_v4()));
        // env mutation is unsafe in edition 2024 (matches stages.rs tests)
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        assert!(find_run("nope").is_none());
    }

    /// A failed run with an error is skipped when it is older than the
    /// cutoff, and its error text survives the roundtrip.
    #[test]
    fn failures_since_respects_cutoff_and_keeps_error() {
        let _g = crate::config::lock_env();
        let dir = std::env::temp_dir().join(format!("fw-runlog-{}", uuid::Uuid::new_v4()));
        // env mutation is unsafe in edition 2024 (matches stages.rs tests)
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };
        let mut fail_old = sample_entry("hl-fail-old");
        fail_old.ts = "2026-08-09T08:00:00Z".into();
        fail_old.status = "fail".into();
        fail_old.error = Some("model 404".into());
        let mut fail_new = sample_entry("hl-fail-new");
        fail_new.ts = "2026-08-09T12:00:00Z".into();
        fail_new.status = "fail".into();
        fail_new.error = Some("timeout".into());
        let mut ok = sample_entry("hl-ok");
        ok.ts = "2026-08-09T12:30:00Z".into();
        write_run(&fail_old).expect("write fail old");
        write_run(&fail_new).expect("write fail new");
        write_run(&ok).expect("write ok");
        let failures = failures_since(Some("2026-08-09T10:00:00Z"));
        assert_eq!(failures.len(), 1, "only the recent failure counts");
        assert_eq!(failures[0].run_id, "hl-fail-new");
        assert_eq!(failures[0].error.as_deref(), Some("timeout"));
    }

    /// Run ids are unique across calls (the run-log filename collision
    /// guard — two runs must never share a file).
    #[test]
    fn run_ids_are_unique() {
        let a = new_run_id();
        let b = new_run_id();
        assert_ne!(a, b);
        assert!(a.starts_with("hl-"), "prefixed for the runs view");
        assert!(!a.is_empty());
    }
}

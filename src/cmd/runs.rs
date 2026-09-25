//! `flywheel runs` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::Config;
use crate::output::print_json;

/// Reserved bookkeeping logs that live in the state dir but are NOT project
/// run logs. Their row schema differs (e.g. `miss-ledger` rows are
/// reclassify records: `ts`/`effect_key`/`goal`, with no `kind`/`project`/
/// `data`). `runs list` sweeps `*.jsonl` by filename, so an unfiltered sweep
/// parses these as projects and drops their rows as malformed (bd-… kind-less
/// drop). Excluded by name so they never masquerade as a project.
const RESERVED_NON_PROJECT_LOGS: &[&str] = &["miss-ledger"];

/// Project names derived from state-dir `*.jsonl` filenames, minus the
/// reserved bookkeeping logs. Pure: takes the raw filenames, returns the
/// sorted, filtered project names. Test seam for the sweep.
fn project_log_names(filenames: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = filenames
        .into_iter()
        .filter_map(|name| {
            let stem = name.strip_suffix(".jsonl")?;
            if RESERVED_NON_PROJECT_LOGS.contains(&stem) {
                return None;
            }
            Some(stem.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Runs view (ADR-0005 P6.3): the captain's daily dashboard over workflow /
/// pipeline / loop runs recorded in the project log.
#[derive(Subcommand)]
pub(crate) enum RunsCmd {
    /// List runs across projects: state + owner + per-run model/token cost
    List {
        /// One JSON array for agent drivers (bd-cron-run-no-live-u9tg)
        #[arg(long)]
        json: bool,
    },
    /// Terminalize an abandoned run as failed after operator evidence
    Reconcile {
        /// logical run id from the state log
        run_id: String,
        /// why the run is being reconciled; recorded in the terminal row
        #[arg(long)]
        reason: String,
        /// required confirmation for this state-log mutation
        #[arg(long)]
        yes: bool,
    },
    /// Show one run's step trace / outputs
    Show {
        /// run id (the log entry id)
        id: String,
    },
    /// Runs parked on approval / spawn-await (pending)
    Pending,
    /// Digest: failures + parked + broken > N runs, since a cutoff
    Digest {
        /// only consider entries after this time (ISO or "N[smhd]" ago)
        #[arg(long)]
        since: Option<String>,
        /// a workflow/pipeline is "broken" after this many consecutive failures
        #[arg(long, default_value_t = 3)]
        broken_after: u32,
    },
}

pub(crate) fn cmd_runs(_cfg: &Config, cmd: &RunsCmd) -> Result<()> {
    use crate::state::{EventKind, LogEntry};

    /// All projects with a log under the state dir (reserved bookkeeping logs
    /// excluded — see [`project_log_names`]).
    fn projects_with_logs() -> Vec<String> {
        let dir = crate::config::state_dir();
        let names: Vec<String> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        project_log_names(names)
    }

    /// All run-kind log entries across projects, newest first.
    fn all_run_entries() -> Result<Vec<(String, LogEntry)>> {
        let mut out = Vec::new();
        for project in projects_with_logs() {
            for e in crate::state::read_log(&project)? {
                match e.kind {
                    EventKind::Workflow | EventKind::Pipeline | EventKind::Loop => {
                        out.push((project.clone(), e));
                    }
                    _ => {}
                }
            }
        }
        out.sort_by(|a, b| b.1.ts.cmp(&a.1.ts));
        Ok(crate::latest_run_entries(out))
    }

    /// A run is pending when its payload carries a suspend/park marker.
    fn is_pending(e: &LogEntry) -> bool {
        e.data
            .get("status")
            .and_then(Value::as_str)
            .map(|s| matches!(s, "pending" | "awaiting_approval" | "parked"))
            .unwrap_or(false)
    }

    /// Append an operator-evidenced terminal row for a still-running logical run.
    /// The latest state is checked immediately before the append, so a completed
    /// run cannot be relabeled as failed by a stale reconciliation request.
    fn reconcile_run(run_id: &str, reason: &str) -> Result<()> {
        let reason = reason.trim();
        if reason.is_empty() {
            bail!("reconcile requires a non-empty --reason");
        }
        for project in projects_with_logs() {
            let entries = crate::state::read_log(&project)?;
            let latest = entries
                .iter()
                .filter(|entry| {
                    entry.kind == EventKind::Workflow
                        && entry.data.get("run_id").and_then(Value::as_str) == Some(run_id)
                        && !crate::is_annotation_row(entry)
                })
                .max_by_key(|entry| entry.ts.as_str());
            let Some(latest) = latest else {
                continue;
            };
            let status = latest
                .data
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            if status != "running" {
                bail!(
                    "run {run_id} in project {project} is already {status}; refusing reconciliation"
                );
            }
            crate::state::log_event(
                &project,
                EventKind::Workflow,
                json!({
                    "workflow": latest.data.get("workflow").cloned().unwrap_or(Value::Null),
                    "status": "failed",
                    "run_id": run_id,
                    "mode": latest.data.get("mode").cloned().unwrap_or(Value::Null),
                    "crew": latest.data.get("crew").cloned().unwrap_or_else(|| json!({})),
                    "error": reason,
                    "reconciled": true,
                    "reconciliation": {
                        "source": "operator",
                        "reason": reason,
                    },
                }),
            )?;
            println!("reconciled {project}:{run_id} as failed (operator evidence recorded)");
            return Ok(());
        }
        bail!("run not found: {run_id}")
    }

    match cmd {
        RunsCmd::List { json } => {
            let mut runs = all_run_entries()?;
            // Headless run logs (ADR-0010 Spec E, P10.1/P10.4): the leaf
            // writes runs/<run_id>.jsonl — list them alongside the state-log
            // runs so cron outcomes surface in the same dashboard.
            for hl in crate::run_log::read_runs() {
                runs.push((
                    hl.run_id.clone(),
                    crate::state::LogEntry {
                        ts: hl.ts.clone(),
                        kind: crate::state::EventKind::Workflow,
                        project: String::new(),
                        data: json!({
                            "status": hl.status,
                            "tokens": hl.tokens_in + hl.tokens_out,
                            "model": hl.model,
                            "cron": hl.cron,
                            "workflow": hl.workflow,
                            // ADR-0022 W4: surface the W2 phase fields in the
                            // runs dashboard — per-phase cycle time needs the
                            // column to exist first.
                            "phase": hl.phase,
                            "phase_degraded": hl.phase_degraded,
                        }),
                    },
                ));
            }
            runs.sort_by(|a, b| b.1.ts.cmp(&a.1.ts));
            if runs.is_empty() {
                println!("no runs recorded");
                return Ok(());
            }
            let rows: Vec<Value> = runs
                .iter()
                .map(|(project, e)| crate::run_list_row(project, e))
                .collect();
            if *json {
                // bd-cron-run-no-live-u9tg: one JSON array for drivers —
                // jq-able (.[0].status) instead of per-line pretty objects.
                print_json(&Value::Array(rows.clone()));
                return Ok(());
            }
            for r in &rows {
                println!("{}", serde_json::to_string_pretty(r)?);
            }
            Ok(())
        }
        RunsCmd::Reconcile {
            run_id,
            reason,
            yes,
        } => {
            if !yes {
                bail!("reconcile changes the run ledger; pass --yes after checking --reason");
            }
            reconcile_run(run_id, reason)
        }
        RunsCmd::Show { id } => {
            let runs = all_run_entries()?;
            if let Some((project, e)) = runs
                .iter()
                .find(|(project, e)| format!("{project}:{}", e.ts) == *id)
            {
                print_json(&json!({
                    "id": id,
                    "project": project,
                    "kind": format!("{:?}", e.kind).to_lowercase(),
                    "ts": e.ts,
                    "owner": crate::run_owner(e),
                    "data": e.data,
                }));
                return Ok(());
            }
            // Headless run-log fallback (ADR-0010 Spec E, P10.1): headless
            // runs live in `runs/<run_id>.jsonl`, not the state log.
            if let Some(entry) = crate::run_log::find_run(id) {
                print_json(&json!({
                    "run_id": entry.run_id,
                    "ts": entry.ts,
                    "workflow": entry.workflow,
                    "profile": entry.profile,
                    "harness": entry.harness,
                    "model": entry.model,
                    "mode": entry.mode,
                    "status": entry.status,
                    "tokens_in": entry.tokens_in,
                    "tokens_out": entry.tokens_out,
                    "duration_ms": entry.duration_ms,
                    "prompt_hash": entry.prompt_hash,
                    "response": entry.response,
                    "error": entry.error,
                }));
                return Ok(());
            }
            bail!("run not found: {id} (flywheel runs list | headless run log)")
        }
        RunsCmd::Pending => {
            let runs = all_run_entries()?;
            let pending: Vec<Value> = runs
                .iter()
                .filter(|(_, e)| is_pending(e))
                .map(|(project, e)| {
                    json!({
                        "id": format!("{project}:{}", e.ts),
                        "project": project,
                        "kind": format!("{:?}", e.kind).to_lowercase(),
                        "owner": crate::run_owner(e),
                        "ts": e.ts,
                    })
                })
                .collect();
            if pending.is_empty() {
                println!("no pending runs");
            } else {
                for r in &pending {
                    println!("{}", serde_json::to_string_pretty(r)?);
                }
            }
            Ok(())
        }
        RunsCmd::Digest {
            since,
            broken_after,
        } => {
            let runs = all_run_entries()?;
            let cutoff: Option<String> = match since {
                Some(s)
                    if s.ends_with('s')
                        || s.ends_with('m')
                        || s.ends_with('h')
                        || s.ends_with('d') =>
                {
                    let (num, unit) = s.split_at(s.len() - 1);
                    let n: u64 = num
                        .parse()
                        .map_err(|_| anyhow::anyhow!("bad --since {s}"))?;
                    let secs = match unit {
                        "s" => n,
                        "m" => n * 60,
                        "h" => n * 3600,
                        "d" => n * 86400,
                        _ => unreachable!(),
                    };
                    Some(crate::now_minus(secs))
                }
                Some(s) => Some(s.clone()),
                None => None,
            };
            let recent: Vec<(String, LogEntry)> = match &cutoff {
                Some(c) => runs.iter().filter(|(_, e)| e.ts >= *c).cloned().collect(),
                None => runs.clone(),
            };
            let failed: Vec<Value> = recent
                .iter()
                .filter(|(_, e)| {
                    e.data
                        .get("status")
                        .and_then(Value::as_str)
                        .map(|s| matches!(s, "failed" | "error"))
                        .unwrap_or(false)
                })
                .map(|(project, e)| {
                    json!({
                        "id": format!("{project}:{}", e.ts),
                        "project": project,
                        "owner": crate::run_owner(e),
                        "ts": e.ts,
                    })
                })
                .collect();
            let parked = recent.iter().filter(|(_, e)| is_pending(e)).count();
            let mut per_owner: std::collections::HashMap<String, u32> =
                std::collections::HashMap::new();
            for (_, e) in &recent {
                let owner = crate::run_owner(e);
                let is_fail = e
                    .data
                    .get("status")
                    .and_then(Value::as_str)
                    .map(|s| matches!(s, "failed" | "error"))
                    .unwrap_or(false);
                *per_owner.entry(owner).or_insert(0) += u32::from(is_fail);
            }
            let broken: Vec<Value> = per_owner
                .iter()
                .filter(|(_, n)| **n > *broken_after)
                .map(|(owner, n)| json!({ "owner": owner, "failures": n }))
                .collect();
            // Headless run logs (ADR-0010 Spec E, P10.1): failures since the
            // cutoff are aggregated separately — they live in `runs/`, not
            // the state log.
            let headless_failed: Vec<Value> = crate::run_log::failures_since(cutoff.as_deref())
                .into_iter()
                .map(|e| {
                    json!({
                        "run_id": e.run_id,
                        "ts": e.ts,
                        "workflow": e.workflow,
                        "profile": e.profile,
                        "status": e.status,
                        "error": e.error,
                    })
                })
                .collect();
            print_json(&json!({
                "since": cutoff,
                "runs": recent.len(),
                "failed": failed,
                "parked": parked,
                "broken_gt_n": broken,
                "headless_failed": headless_failed,
            }));
            Ok(())
        }
    }
}

#[cfg(test)]
mod runs_sweep_tests {
    use super::project_log_names;

    /// The sweep must drop reserved bookkeeping logs (their rows are a
    /// different schema and would be silently discarded as malformed) while
    /// keeping every real project, sorted and deduped.
    #[test]
    fn reserved_bookkeeping_logs_are_excluded_from_the_project_sweep() {
        let names = vec![
            "flywheel.jsonl".to_string(),
            "miss-ledger.jsonl".to_string(),
            "bd-cvv9-test.jsonl".to_string(),
            "notalog.txt".to_string(),    // no .jsonl suffix
            "flywheel.jsonl".to_string(), // duplicate
        ];
        let got = project_log_names(names);
        assert_eq!(
            got,
            vec!["bd-cvv9-test".to_string(), "flywheel".to_string()]
        );
        assert!(
            !got.iter().any(|p| p == "miss-ledger"),
            "miss-ledger must never be treated as a project"
        );
        assert!(
            !got.iter().any(|p| p == "notalog"),
            "non-jsonl files are not projects"
        );
    }

    /// An empty sweep is empty — no phantom projects.
    #[test]
    fn empty_sweep_yields_no_projects() {
        assert!(project_log_names(Vec::new()).is_empty());
    }
}

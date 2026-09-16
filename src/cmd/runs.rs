//! `flywheel runs` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::Config;
use crate::output::print_json;

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

    /// All projects with a log under the state dir.
    fn projects_with_logs() -> Vec<String> {
        let mut out = Vec::new();
        let dir = crate::config::state_dir();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with(".jsonl") {
                    out.push(name.trim_end_matches(".jsonl").to_string());
                }
            }
        }
        out.sort();
        out
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
        Ok(out)
    }

    /// A run is pending when its payload carries a suspend/park marker.
    fn is_pending(e: &LogEntry) -> bool {
        e.data
            .get("status")
            .and_then(Value::as_str)
            .map(|s| matches!(s, "pending" | "awaiting_approval" | "parked"))
            .unwrap_or(false)
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
                    let n: u64 = num.parse().map_err(|_| anyhow::anyhow!("bad --since {s}"))?;
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

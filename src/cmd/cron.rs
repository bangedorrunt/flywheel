//! `flywheel cron` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::config::Config;
use crate::output::print_json;
use crate::state::EventKind;
use crate::{cron, doorbell, log, pane, profile, state, workflow_catalog};

/// `flywheel cron` subcommands (ADR-0012 Spec 1/4, P12.3): the standalone
/// cron surface over cron YAMLs. Crons are YAML files in three tiers —
/// per-project > global > built-in — with a shared CronSpec body on the
/// toron-workflow engine. `lint`/`run` validate + test a cron on demand;
/// `start` drives the durable loops; pause/resume/remove mutate the tier's
/// YAML (built-ins are read-only; shadow by a user YAML instead).
#[derive(Subcommand)]
pub(crate) enum CronCmd {
    /// List resolved crons (per-project > global > built-in) with source +
    /// scope
    List {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Show one cron's resolved definition (spec + source + scope)
    Show {
        /// cron name
        name: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Lint cron YAML(s): engine tier (schedule parse, policy sanity, step/
    /// graph checks) + flywheel tier (owner-profile resolution). Exit 0
    /// clean, 1 on any blocking issue, 2 on a parse failure.
    Lint {
        /// lint only this cron (name), else every resolved cron
        name: Option<String>,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Run a cron's body on demand (testing): lint gate first, then the spec
    /// steps through the shared engine with the spec's mode runner.
    Run {
        /// cron name (resolved tier)
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// REALLY dispatch the body through the spec's mode runner (the same
        /// headless/pane path the durable loop fires) instead of the
        /// structural dry-run (bd-cron-run-no-live-u9tg)
        #[arg(long)]
        live: bool,
    },
    /// Pause a cron (entry preserved; nothing fires) — persists `enabled:
    /// false` in the tier's YAML
    Pause {
        /// cron name
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Resume a paused cron (removes the `enabled: false` marker)
    Resume {
        /// cron name
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Status of every cron: paused | due | running | succeeded | failed
    Status {
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Run history for a cron (state-log scan + failure beads)
    History {
        /// cron name
        name: String,
        /// JSON output for agent drivers
        #[arg(long)]
        json: bool,
    },
    /// Start (or resume) the durable cron loops over the resolved crons.
    /// Health-gate probes + `on_failure: bead` are live. Blocks; Ctrl-C to
    /// stop.
    Start {
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
    /// Remove a cron definition (user tiers only; built-ins are read-only).
    /// Removing a project-tier cron reveals the global/built-in definition
    /// it shadowed.
    Remove {
        /// cron name
        name: String,
        /// project whose .flywheel/crons tier applies (default: detect from cwd)
        #[arg(long)]
        project: Option<String>,
    },
}

/// `flywheel cron` — the standalone cron surface (ADR-0012 Spec 1/4,
/// P12.3). Crons are YAML files in three tiers (per-project > global >
/// built-in) with a shared CronSpec body on the toron-workflow engine.
/// Status is derived from the state log + run log.
/// Dispatched from `main()`; moved from crate root (bd-wave5-main-eviction-d5hht).
pub(crate) fn cmd_cron(cfg: &Config, cmd: &CronCmd) -> Result<()> {
    use toron_workflow::lint::{Level, lint_cron};

    /// Lint one cron: engine tier (schedule parse, policy sanity, step/graph
    /// checks — P1) + flywheel tier (owner-profile resolution against the
    /// ProfileCatalog). Returns the blocking (Error-level) count.
    fn lint_one(name: &str, spec: &toron_workflow::schema::CronSpec) -> usize {
        let report = lint_cron(spec);
        let mut blocking = report
            .issues
            .iter()
            .filter(|i| i.level == Level::Error)
            .count();
        for issue in &report.issues {
            eprintln!(
                "[{name}] {:>7} {}",
                if issue.level == Level::Error {
                    "error"
                } else {
                    "warning"
                },
                issue.message
            );
        }
        let profiles = profile::ProfileCatalog::load().unwrap_or_default();
        let owner = spec.owner.trim_start_matches('@');
        if !owner.is_empty() && profiles.get(owner).is_none() {
            eprintln!(
                "[{name}]    error owner profile {owner:?} does not resolve (known: {})",
                profiles.names().join(", ")
            );
            blocking += 1;
        }
        blocking
    }

    /// The resolved cron by name (fail-stop on unknown).
    fn resolve<'a>(rows: &'a [cron::CronRow], name: &str) -> Result<&'a cron::CronRow> {
        rows.iter()
            .find(|r| r.name() == name)
            .ok_or_else(|| anyhow!("no cron named {name:?} (flywheel cron list)"))
    }

    match cmd {
        CronCmd::List { json, project } => {
            let rows = cron::load_crons(cfg, project.as_deref())?;
            if rows.is_empty() {
                println!("no crons defined");
                return Ok(());
            }
            let mut rows: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let (last, in_flight) = cron::last_run_state(row.name());
                    let status = cron::derive_status(row.enabled(), in_flight, last.as_deref());
                    let next_due = cron::next_due(row.cron(), cron::now_ts());
                    cron::cron_row_json(row, status, next_due)
                })
                .collect();
            rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            if *json {
                print_json(&json!({ "crons": rows }));
            } else {
                println!(
                    "{:<28} {:<22} {:<16} {:<7} {:<8} {:<9} {:<6} {:<7}",
                    "NAME", "OWNER", "SCHEDULE", "SCOPE", "SOURCE", "STATUS", "STEPS", "STATE"
                );
                for row in &rows {
                    println!(
                        "{:<28} {:<22} {:<16} {:<7} {:<8} {:<9} {:<6} {}",
                        row["name"].as_str().unwrap_or(""),
                        row["owner"].as_str().unwrap_or(""),
                        row["schedule"].as_str().unwrap_or(""),
                        row["scope"].as_str().unwrap_or(""),
                        row["source"].as_str().unwrap_or(""),
                        row["status"].as_str().unwrap_or(""),
                        row["steps"].as_i64().unwrap_or(0),
                        if row["enabled"].as_bool().unwrap_or(false) {
                            "on"
                        } else {
                            "paused"
                        },
                    );
                }
            }
            Ok(())
        }
        CronCmd::Show {
            name,
            json,
            project,
        } => {
            let rows = cron::load_crons(cfg, project.as_deref())?;
            let row = resolve(&rows, name)?;
            let (last, in_flight) = cron::last_run_state(name);
            let status = cron::derive_status(row.enabled(), in_flight, last.as_deref());
            let next_due = cron::next_due(row.cron(), cron::now_ts());
            if *json {
                print_json(&cron::cron_row_json(row, status, next_due));
            } else {
                println!("name:     {}", row.name());
                println!("schedule: {}", row.cron());
                println!("owner:    {}", row.owner());
                println!(
                    "scope:    {}",
                    match row.scope() {
                        toron_workflow::schema::CronScope::Host => "host",
                        toron_workflow::schema::CronScope::Project => "project",
                    }
                );
                println!("source:   {}", row.source_label());
                println!(
                    "mode:     {}",
                    match row.spec.mode {
                        toron_workflow::schema::CronMode::Headless => "headless",
                        toron_workflow::schema::CronMode::Pane => "pane",
                    }
                );
                println!("status:   {status}");
                if let Some(d) = next_due {
                    println!("next_due: {d}");
                }
                if let Some(p) = &row.spec.policy {
                    println!("policy:   {p:?}");
                }
                if let Some(d) = &row.spec.description {
                    println!("desc:     {d}");
                }
                for (i, s) in row.spec.steps.iter().enumerate() {
                    let n = s.name.clone().unwrap_or_else(|| "<unnamed>".to_string());
                    println!("step {i}:   {n} ({})", s.action.type_name());
                }
            }
            Ok(())
        }
        CronCmd::Lint { name, project } => {
            let rows = cron::load_crons(cfg, project.as_deref())?;
            if let Some(n) = name {
                let row = resolve(&rows, n)?;
                let blocking = lint_one(n, &row.spec);
                if blocking > 0 {
                    std::process::exit(1);
                }
                println!("cron {n} lints clean (source {})", row.source_label());
                Ok(())
            } else {
                let mut blocking_total = 0;
                for row in &rows {
                    blocking_total += lint_one(row.name(), &row.spec);
                }
                if blocking_total > 0 {
                    std::process::exit(1);
                }
                println!("lint clean: {} cron(s)", rows.len());
                Ok(())
            }
        }
        CronCmd::Run {
            name,
            project,
            json,
            live,
        } => {
            let rows = cron::load_crons(cfg, project.as_deref())?;
            let row = resolve(&rows, name)?.clone();
            let source = row.source_label().to_string();
            let blocking = lint_one(name, &row.spec);
            if blocking > 0 {
                bail!("cron {name:?} has {blocking} blocking lint issue(s) — refusing to run");
            }
            let spec = row.spec;
            let wf = toron_workflow::cron::spec_tick_workflow(&spec);
            let rt = tokio::runtime::Runtime::new()?;
            let run_id = format!("cron-run-{}", state::now_iso());
            let workflow_resolver: toron_workflow::WorkflowResolver =
                Arc::new(|name: &str| workflow_catalog::subworkflow_resolver(None)(name));
            let profile_resolver = profile::ProfileCatalog::catalog_profile_resolver();
            if *live {
                let runner = cron::spec_runner(&spec);
                let mode = match spec.mode {
                    toron_workflow::schema::CronMode::Headless => "headless",
                    toron_workflow::schema::CronMode::Pane => "pane",
                };
                let engine = pane::local_engine_journaled(
                    cfg,
                    &rt,
                    runner,
                    profile_resolver,
                    workflow_resolver.clone(),
                )?;
                let result = rt.block_on(async { engine.invoke(&run_id, wf, json!({})).await });
                match result {
                    Ok(output) => {
                        log(
                            "workflow",
                            EventKind::Workflow,
                            json!({
                                "workflow": name,
                                "status": "completed",
                                "run_id": run_id,
                                "cron": name,
                                "live": true,
                            }),
                        );
                        if *json {
                            print_json(&json!({
                                "cron": name,
                                "run_id": run_id,
                                "source": source,
                                "live": true,
                                "dry_run": false,
                                "output": output,
                            }));
                        } else {
                            println!(
                                "cron {name} live run ok (run_id {run_id}, source {source}, mode {mode}) — dispatched via the durable-loop runner; output:"
                            );
                            print_json(&output);
                        }
                        if let Some(p) = project {
                            doorbell::cron_tick_wake(p, name, &run_id, "completed");
                        }
                        Ok(())
                    }
                    Err(e) => {
                        log(
                            "workflow",
                            EventKind::Workflow,
                            json!({
                                "workflow": name,
                                "status": "failed",
                                "run_id": run_id,
                                "cron": name,
                                "live": true,
                                "error": e.to_string(),
                            }),
                        );
                        Err(e)
                    }
                }
            } else {
                let runner = cron::recording_runner();
                let output = rt.block_on(async {
                    let engine = toron_workflow::WorkflowEngine::local_with_resolvers_and_effect(
                        profile_resolver,
                        workflow_resolver,
                        Some(runner),
                    )?;
                    tokio::time::timeout(
                        std::time::Duration::from_secs(25),
                        engine.invoke(&run_id, wf, json!({})),
                    )
                    .await
                    .map_err(|_elapsed| {
                        anyhow::anyhow!(
                            "cron {name} on-demand dry-run timed out after 25s (a delay/approval/spawn step parks the body — use --live for real dispatch)"
                        )
                    })?
                })?;
                log(
                    "workflow",
                    EventKind::Workflow,
                    json!({
                        "workflow": name,
                        "status": "done",
                        "run_id": run_id,
                        "cron": name,
                    }),
                );
                if *json {
                    print_json(&json!({
                        "cron": name,
                        "run_id": run_id,
                        "source": source,
                        "dry_run": true,
                        "output": output,
                    }));
                } else {
                    println!(
                        "cron {name} dry-run ok (run_id {run_id}, source {source}) — no live dispatch; recorded steps:"
                    );
                    print_json(&output);
                }
                Ok(())
            }
        }
        CronCmd::Pause { name, project } => {
            cron::set_cron_enabled(cfg, project.as_deref(), name, false)?;
            println!("paused cron {name}");
            Ok(())
        }
        CronCmd::Resume { name, project } => {
            cron::set_cron_enabled(cfg, project.as_deref(), name, true)?;
            println!("resumed cron {name}");
            Ok(())
        }
        CronCmd::Status { json, project } => {
            let rows = cron::load_crons(cfg, project.as_deref())?;
            if rows.is_empty() {
                println!("no crons defined");
                return Ok(());
            }
            let mut rows: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let (last, in_flight) = cron::last_run_state(row.name());
                    let status = cron::derive_status(row.enabled(), in_flight, last.as_deref());
                    let next_due = cron::next_due(row.cron(), cron::now_ts());
                    json!({
                        "name": row.name(),
                        "status": status,
                        "enabled": row.enabled(),
                        "next_due": next_due,
                        "source": row.source_label(),
                    })
                })
                .collect();
            rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
            if *json {
                print_json(&json!({ "crons": rows }));
            } else {
                println!(
                    "{:<28} {:<9} {:<8} {:<12} {:<8}",
                    "NAME", "STATUS", "STATE", "NEXT_DUE", "SOURCE"
                );
                for row in &rows {
                    println!(
                        "{:<28} {:<9} {:<8} {} {:<8}",
                        row["name"].as_str().unwrap_or(""),
                        row["status"].as_str().unwrap_or(""),
                        if row["enabled"].as_bool().unwrap_or(false) {
                            "on"
                        } else {
                            "paused"
                        },
                        row["next_due"].as_i64().unwrap_or(0),
                        row["source"].as_str().unwrap_or(""),
                    );
                }
            }
            Ok(())
        }
        CronCmd::History { name, json } => {
            let runs = cron::cron_history(name);
            let rows: Vec<Value> = runs
                .iter()
                .map(|r| {
                    json!({
                        "ts": r.ts,
                        "project": r.project,
                        "run_id": r.run_id,
                        "status": r.status,
                        "bead": r.bead,
                    })
                })
                .collect();
            if *json {
                print_json(&json!({ "cron": name, "runs": rows }));
            } else if rows.is_empty() {
                println!("no runs recorded for cron {name}");
            } else {
                for row in &rows {
                    println!(
                        "{} {} {} {}",
                        row["ts"].as_str().unwrap_or(""),
                        row["run_id"].as_str().unwrap_or(""),
                        row["status"].as_str().unwrap_or(""),
                        row["bead"].as_str().unwrap_or(""),
                    );
                }
            }
            Ok(())
        }
        CronCmd::Start { project } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(cron::start_loops(cfg, project.as_deref()))
        }
        CronCmd::Remove { name, project } => {
            cron::remove_cron(cfg, project.as_deref(), name)?;
            println!("removed cron {name}");
            Ok(())
        }
    }
}

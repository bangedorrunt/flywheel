//! `flywheel work` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Result, bail};
use clap::Subcommand;

use crate::config::Config;
use crate::output::print_json;

#[derive(Subcommand)]
pub(crate) enum WorkCmd {
    /// bv --robot-triage: full graph triage with recommendations
    Triage,
    /// bv --robot-next: the single top pick
    Next,
    /// bv --robot-insights: graph metrics
    Insights,
    /// bv --robot-plan: parallel execution tracks
    Plan,
    /// br ready --json: ready beads
    Ready,
    /// bd-bead-create-contract-lint-4tdd: creation-time contract check —
    /// runs the EXACT loop parsers (verify fence + PRINCIPLES for P<=2)
    /// over the ready set and reports per-bead block reasons with the
    /// corrected snippet. Exit non-zero when any bead is not dispatchable.
    Check {
        /// project dir to run `br ready` in (default: current directory)
        project: Option<String>,
    },
}

pub(crate) fn cmd_work(cfg: &Config, cmd: &WorkCmd) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let bv = crate::work::Bv::new(cwd.to_str().unwrap_or("."));
    match cmd {
        WorkCmd::Triage => {
            let v = bv.robot("triage")?;
            print_json(&v);
        }
        WorkCmd::Next => {
            let v = bv.robot("next")?;
            print_json(&v);
        }
        WorkCmd::Insights => {
            let v = bv.robot("insights")?;
            print_json(&v);
        }
        WorkCmd::Plan => {
            let v = bv.robot("plan")?;
            print_json(&v);
        }
        WorkCmd::Ready => {
            let v = bv.br_json(&["ready"])?;
            print_json(&v);
        }
        WorkCmd::Check { project } => cmd_work_check(cfg, project.as_deref())?,
    }
    let _ = cfg;
    Ok(())
}

fn cmd_work_check(cfg: &Config, project: Option<&str>) -> Result<()> {
    // bd-bead-create-contract-lint-4tdd: creation-time contract check —
    // the EXACT loop parsers over the ready set (typed verify presence +
    // principles_block_reason via not_ready_reason), per-bead report with
    // the corrected snippet. Non-zero exit when any bead is not
    // dispatchable, so planner lanes can gate "beads created" on it.
    let dir = match project {
        Some(p) => crate::project_dir(cfg, p),
        None => std::env::current_dir()?,
    };
    let out = std::process::Command::new("br")
        .args(["ready", "--json"])
        .current_dir(&dir)
        .output()
        .map_err(|e| anyhow::anyhow!("br ready: {e}"))?;
    if !out.status.success() {
        bail!(
            "br ready failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // The loop's exact ready parse (array shape, the typed `verify` field
    // br emits — same source of truth as the __ready leaf the durable loop
    // consumes).
    let ready = crate::parse_ready_beads_json(&String::from_utf8_lossy(&out.stdout))?;
    // The check runs the EXACT loop gate over each bead (verify presence +
    // the principles check) — fetch descriptions per bead; the ready set
    // is small.
    let mut issues: Vec<crate::durable::BeadContractIssue> = Vec::new();
    for b in &ready {
        let desc = bead_description(&dir, &b.id);
        if let Some(issue) = crate::durable::contract_issue(b, &desc) {
            issues.push(issue);
        }
    }
    if issues.is_empty() {
        println!("work check: {} ready beads, all dispatchable", ready.len());
        return Ok(());
    }
    for i in &issues {
        println!("BLOCK {} — {}", i.id, i.reason);
        println!("      fix: {}", i.fix);
    }
    bail!(
        "work check: {} of {} ready beads not dispatchable",
        issues.len(),
        ready.len()
    );
}

/// The raw description of a bead via `br show <id> --json` (empty on any
/// failure — a missing description only yields the fence/PRINCIPLES
/// verdicts the ready JSON already expresses).
fn bead_description(dir: &std::path::Path, id: &str) -> String {
    let Ok(out) = std::process::Command::new("br")
        .args(["show", id, "--json"])
        .current_dir(dir)
        .output()
    else {
        return String::new();
    };
    if !out.status.success() {
        return String::new();
    }
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| {
            v.as_array()
                .and_then(|a| a.first())
                .and_then(|b| b.get("description"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

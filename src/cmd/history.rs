//! History-family read handlers (split from `main.rs`, bd-3l8g):
//! timeline / audit / history / changes / resume / analytics.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::config::Config;
use crate::herdr_shim::{AgentStatus, Client};
use crate::output::{dim, kind_paint, print_json, status_paint};
use crate::state::EventKind;

pub(crate) fn cmd_timeline(project: &str) -> Result<()> {
    let entries = crate::state::read_log(project)?;
    if entries.is_empty() {
        println!("no activity logged for {project}");
        return Ok(());
    }
    for e in entries {
        let kind = format!("{:?}", e.kind).to_lowercase();
        let detail = match e.kind {
            EventKind::Send => format!(
                "to {} stage={}",
                e.data.get("to").and_then(Value::as_str).unwrap_or("?"),
                e.data.get("stage").and_then(Value::as_str).unwrap_or("-")
            ),
            EventKind::Spawn => format!(
                "workspace={}",
                e.data
                    .get("workspace_id")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
            ),
            EventKind::Checkpoint => e
                .data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            EventKind::State => format!(
                "state={}",
                e.data.get("state").and_then(Value::as_str).unwrap_or("?")
            ),
            _ => String::new(),
        };
        println!("{} {:>12}  {}", dim(&e.ts), dim(&kind), detail);
    }
    Ok(())
}

pub(crate) fn cmd_audit(project: &str) -> Result<()> {
    let entries = crate::state::read_log(project)?;
    let v: Vec<Value> = entries
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or(json!({})))
        .collect();
    print_json(&json!({ "project": project, "entries": v }));
    Ok(())
}

pub(crate) fn cmd_history(project: &str) -> Result<()> {
    let cps = crate::state::list_checkpoints(project)?;
    if cps.is_empty() {
        println!("no checkpoints for {project}");
        return Ok(());
    }
    // newest first, so the displayed index matches `changes --since N`
    // (0 = latest) — history's [i] and changes' --since agree
    for (i, cp) in cps.iter().rev().enumerate() {
        println!(
            "[{}] {}  {}  {}",
            i,
            dim(&cp.ts),
            dim(&cp.project),
            cp.message
        );
    }
    Ok(())
}

pub(crate) fn cmd_changes(cfg: &Config, project: &str, since: usize) -> Result<()> {
    let cps = crate::state::list_checkpoints(project)?;
    let Some(base) = cps.iter().rev().nth(since) else {
        bail!("no checkpoint at index {since} (have {})", cps.len());
    };
    println!("baseline: {} — {}", dim(&base.ts), base.message);
    // current state
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = crate::find_workspace(&mut c, project)?;
    let agents = c
        .agent_list()?
        .into_iter()
        .filter(|a| a.workspace_id == ws_id)
        .collect::<Vec<_>>();
    for a in &agents {
        let prev = base
            .agents
            .iter()
            .find(|p| p.name == a.name.as_deref().unwrap_or(""));
        let prev_st = prev.map(|p| p.status.as_str()).unwrap_or("new");
        let now_st = a.agent_status.as_str();
        if prev_st != now_st {
            println!(
                "{}  {} → {}",
                dim(a.name.as_deref().unwrap_or("?")),
                dim(prev_st),
                status_paint(
                    crate::herdr_shim::parse_agent_status(now_st).unwrap_or(AgentStatus::Unknown)
                )
            );
        }
    }
    let head = crate::state::git_head(&crate::project_dir_str(cfg, project)?);
    if let Ok(h) = head {
        println!("git head: {}", dim(&h));
    }
    Ok(())
}

pub(crate) fn cmd_resume(cfg: &Config, project: &str, recipe: Option<&str>) -> Result<()> {
    let latest = crate::state::latest_checkpoint(project)?;
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws = match crate::find_workspace(&mut c, project) {
        Ok(ws_id) => {
            println!("swarm alive: workspace {}", dim(&ws_id));
            Some(ws_id)
        }
        Err(_) => None,
    };
    if ws.is_none() {
        if let Some(r) = recipe {
            println!("respawn with recipe {r}…");
            crate::cmd_spawn(
                cfg,
                project,
                crate::SpawnOpts {
                    recipe: Some(r.to_string()),
                    ..Default::default()
                },
            )?;
        } else {
            bail!(
                "workspace gone (run: flywheel spawn {project} --recipe <name> or flywheel resume {project} --recipe daily)"
            );
        }
    }
    if let Some(cp) = latest {
        println!("last checkpoint: {} — {}", dim(&cp.ts), cp.message);
        for a in &cp.agents {
            println!("  {}  {}", kind_paint(&a.kind), status_paint(a.status));
        }
    }
    crate::log(project, EventKind::Resume, json!({}));
    Ok(())
}

pub(crate) fn cmd_analytics(project: &str) -> Result<()> {
    let a = crate::state::analytics(project)?;
    print_json(&serde_json::to_value(&a)?);
    Ok(())
}

//! `status` / `progress` / `snapshot` / `helm` verbs (split from `main.rs`,
//! bd-wave5). All four project through `snapshot::assemble`, the one world
//! read — no second snapshot shape here.

use std::collections::{BTreeMap, HashSet};
use std::io::IsTerminal;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::config::Config;
use crate::hc::{AgentInfo, Client};
use crate::output::{dim, kind_paint, print_json, status_paint};

pub(crate) fn cmd_snapshot(cfg: &Config, project: &str) -> Result<()> {
    crate::check_project_spelling(project, "snapshot")?;
    // Wave11: one world read — ws_label derived inside assemble.
    let snap = crate::snapshot::assemble(cfg, project, crate::doorbell_now_unix());
    print_json(&serde_json::to_value(&snap)?);
    Ok(())
}

pub(crate) fn cmd_helm(cfg: &Config, project: &str) -> Result<()> {
    crate::check_project_spelling(project, "helm")?;
    let now = crate::doorbell_now_unix();
    // Wave11: one world read — ws_label derived inside assemble.
    let snap = crate::snapshot::assemble(cfg, project, now);
    print!("{}", crate::snapshot::render_helm(&snap, now));
    let (holder, whois_stale) = crate::lookup_captain_holder(project);
    let pane_alive = holder
        .as_ref()
        .map(|h| snap.pins.iter().any(|p| p.pin == *h))
        .unwrap_or(false);
    if let Some(line) = crate::captain_fold_health_line(
        project,
        holder,
        whois_stale,
        pane_alive,
        snap.dispatcher == "up",
        None,
    ) {
        eprintln!("{line}");
    }
    Ok(())
}

pub(crate) fn cmd_status(cfg: &Config, project: Option<&str>) -> Result<()> {
    crate::ensure_project_opt(project, "status")?;
    let Some(project) = project else {
        return crate::cmd_list(cfg, false);
    };
    let (ws_id, agents, dead): (String, Vec<AgentInfo>, Vec<(String, String)>) =
        if crate::pane::rex_door_selected(cfg) {
            let backend = crate::backend::factory::multiplexer_backend(cfg)?;
            let ws_id = crate::find_workspace_dyn(&*backend, project)?;
            let agents = backend
                .agent_list()?
                .into_iter()
                .filter(|a| a.workspace_id == ws_id)
                .collect::<Vec<_>>();
            (ws_id, agents, Vec::new())
        } else {
            let mut c = Client::connect(Some(&cfg.socket_path()))?;
            let ws_id = crate::find_workspace(&mut c, project)?;
            let agents = c
                .agent_list()?
                .into_iter()
                .filter(|a| a.workspace_id == ws_id)
                .collect::<Vec<_>>();
            let dead = crate::pane::dead_registry_rows(project, &mut c);
            (ws_id, agents, dead)
        };
    if agents.is_empty() {
        bail!("no agents in {project} — is the swarm spawned?");
    }
    let key = crate::project_dir_str(cfg, project)?;
    let registry =
        crate::state::load_spawn_registry(&crate::project_dir(cfg, project).to_string_lossy());
    let mail = crate::cmd::mail::mail_or_die(cfg).ok();
    // Wave11: one world read — ws_label derived inside assemble.
    let snap = crate::snapshot::assemble(cfg, project, crate::doorbell_now_unix());
    // Wave11 residual: doorbell/facts/integrations still read directly.
    let ws_label =
        crate::state::load_spawn_meta(project).unwrap_or_else(|| crate::swarm_label(project));
    let actual_by_pane: std::collections::HashMap<String, String> = {
        agents
            .iter()
            .filter_map(|a| {
                let mail = mail.as_ref()?;
                mail.resolve_pane_identity(&key, &a.pane_id)
                    .get("agent_name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| (a.pane_id.clone(), s.to_string()))
            })
            .collect()
    };
    let identity_divergent: Vec<String> = agents
        .iter()
        .filter_map(|a| {
            let expected = registry
                .iter()
                .find(|e| e.pane_id == a.pane_id)
                .and_then(|e| e.mail_identity.as_deref());
            let observed = actual_by_pane.get(&a.pane_id).map(String::as_str);
            crate::identity_divergence(expected, observed)
                .map(|m| format!("pane {}: {m}", a.pane_id))
        })
        .collect();
    for line in &identity_divergent {
        eprintln!("flywheel: identity_divergent {line}");
    }
    let rows: Vec<Value> = agents
        .iter()
        .map(|a| {
            let pending = crate::is_still_booting(a);
            let plane_status = a.agent_status.as_str();
            let verdict = crate::workspace::verdict_for(None, plane_status, plane_status != "dead")
                .status_token();
            let expected_pin = registry
                .iter()
                .find(|e| e.pane_id == a.pane_id)
                .and_then(|e| e.mail_identity.clone());
            let observed_pin = actual_by_pane.get(&a.pane_id).cloned();
            let divergent =
                crate::identity_divergence(expected_pin.as_deref(), observed_pin.as_deref())
                    .is_some();
            let terminal_name = a.name.clone().unwrap_or_else(|| a.pane_id.clone());
            let who = expected_pin
                .as_deref()
                .and_then(|pin| mail.as_ref()?.whois(&key, pin).ok());
            let npub = who
                .as_ref()
                .and_then(|w| w.get("npub").and_then(Value::as_str))
                .map(str::to_string);
            let last_active_ts_us = who
                .as_ref()
                .and_then(|w| w.get("last_active_ts_us").and_then(Value::as_u64));
            let snap_pin = snap.pins.iter().find(|p| p.pin == terminal_name);
            json!({
                "name": a.name, "kind": a.agent,
                "status": crate::cmd::send::status_label(pending, plane_status),
                "verdict": verdict,
                "pane_id": a.pane_id, "cwd": a.cwd, "workspace_id": ws_id,
                "launch_pending": pending,
                "identity_pin": expected_pin,
                "observed_pin": observed_pin,
                "identity_divergent": divergent,
                "npub": npub,
                "last_active_ts_us": last_active_ts_us,
                "bead": snap_pin.and_then(|p| p.bead.clone()),
                "reservations": snap_pin.map(|p| p.reservations.clone()).unwrap_or_default(),
            })
        })
        .collect();
    let integration_statuses = crate::read_integration_statuses();
    let integrations: BTreeMap<String, crate::hc::IntegrationStatus> = agents
        .iter()
        .filter_map(|a| a.agent.as_deref())
        .filter(|k| crate::hc::integration_target(k).is_some())
        .collect::<HashSet<_>>()
        .into_iter()
        .map(|k| {
            let st = integration_statuses
                .get(k)
                .cloned()
                .unwrap_or_else(crate::hc::IntegrationStatus::not_installed);
            (k.to_string(), st)
        })
        .collect();
    let dispatcher =
        crate::doorbell::dispatcher_status_line(&crate::config::state_dir(), &ws_label);
    let open_decision_rows: Vec<Value> = snap
        .decisions
        .iter()
        .map(|d| json!({"corr": d.key, "body": d.waits_on, "created_at": d.opened_at}))
        .collect();
    let unanswered = crate::doorbell::read_unanswered(&crate::doorbell::unanswered_file(
        &crate::config::state_dir(),
        &ws_label,
    ));
    let report = crate::doorbell::report_text(&crate::config::state_dir(), &ws_label);
    let wake_state = crate::doorbell::load_wake_state(&crate::doorbell::wake_state_file(
        &crate::config::state_dir(),
        &ws_label,
    ));
    let dispatcher_alive = crate::doorbell::read_pid(&crate::doorbell::pid_file(
        &crate::config::state_dir(),
        &ws_label,
    ))
    .is_some_and(crate::doorbell::pid_alive);
    if !dispatcher_alive {
        let _ = crate::doorbell::ensure_running(
            &std::env::current_exe()?,
            &crate::config::state_dir(),
            &ws_label,
            project,
        );
    }
    let (facts_count, last_fact) =
        crate::facts_count_and_last(&crate::config::state_dir(), project);
    if std::io::stdout().is_terminal() {
        for r in &rows {
            let st = r["status"].as_str().unwrap_or("unknown");
            let st = crate::hc::parse_agent_status(st)
                .map(status_paint)
                .unwrap_or_else(|_| dim(st));
            println!(
                "{}  {}  {}  {}  {}",
                kind_paint(r["kind"].as_str().unwrap_or("?")),
                dim(r["name"].as_str().unwrap_or("?")),
                st,
                dim(r["verdict"].as_str().unwrap_or("Unknown")),
                dim(r["pane_id"].as_str().unwrap_or(""))
            );
        }
        for (k, st) in &integrations {
            println!("  {}", crate::integration_status_line(k, st));
        }
        if dead.is_empty() {
            println!("  {}", dim("registry: no dead rows"));
        } else {
            println!(
                "  {}",
                crate::output::warn(&format!("registry: {} dead row(s)", dead.len()))
            );
            for (pane_id, name) in &dead {
                println!("    {pane_id} {name}");
            }
        }
        println!("{report}");
        println!(
            "{} {} last_wake={} pin={}",
            crate::output::accent("◆ doorbell:"),
            if dispatcher_alive {
                crate::output::ok("up")
            } else {
                crate::output::warn("DOWN (healed)")
            },
            wake_state
                .last_wake_unix
                .map(|t| t.to_string())
                .unwrap_or_else(|| "never".into()),
            wake_state.last_wake_pin.as_deref().unwrap_or("?")
        );
        println!(
            "{} {} last={}",
            crate::output::accent("facts:"),
            facts_count,
            last_fact.as_deref().unwrap_or("none")
        );
        if open_decision_rows.is_empty() {
            println!("OPEN DECISIONS: none");
        } else {
            println!("OPEN DECISIONS");
            for r in &open_decision_rows {
                println!(
                    "- [{}] {}",
                    r["corr"].as_str().unwrap_or("?"),
                    r["body"].as_str().unwrap_or("")
                );
            }
        }
    } else {
        print_json(&json!({
            "workspace_id": ws_id, "agents": rows, "integrations": integrations,
            "dispatcher": dispatcher,
            "unanswered": unanswered,
            "doorbell": {
                "alive": dispatcher_alive,
                "last_wake_unix": wake_state.last_wake_unix,
                "last_wake_pin": wake_state.last_wake_pin,
            },
            "facts": {
                "count": facts_count,
                "last": last_fact,
            },
            "open_decisions": open_decision_rows,
            "identity_divergent": identity_divergent,
        }));
    }
    Ok(())
}

/// Wave11: per-wave counting lives in `snapshot::progress_from_rows` (one pure
/// core). This thin `progress_waves` delegate remains for `loop_mode` tests;
/// live doors use the snapshot projection directly.
#[allow(dead_code)]
pub(crate) fn progress_waves(rows: &[Value], ready_ids: &HashSet<String>) -> Value {
    crate::snapshot::progress_from_rows(rows, ready_ids)
}

/// bd-swarm-cockpit-pczs: `flywheel progress` — beads grouped by their
/// `wave-N` label with the wave currently dispatching. The project comes from
/// the positional / `--project`, or the workspace cwd (same inherit rule as
/// `steer`), so `flywheel progress --json` works from inside a project.
pub(crate) fn cmd_progress(cfg: &Config, project: Option<&str>, json_out: bool) -> Result<()> {
    crate::ensure_project_opt(project, "progress")?;
    let project = crate::cron::detect_project(cfg, project).ok_or_else(|| {
        anyhow!(
            "progress: no project — pass <project>/--project <slug> or run from a \
             workspace/<project> cwd"
        )
    })?;
    crate::check_project_spelling(&project, "progress")?;
    // Wave11: one world read; `progress_view` is the pure serializer.
    let snap = crate::snapshot::assemble(cfg, &project, crate::doorbell_now_unix());
    let out = crate::snapshot::progress_view(&snap);
    if json_out || !std::io::stdout().is_terminal() {
        print_json(&out);
        return Ok(());
    }
    let cell = |v: &Value| v.as_u64().unwrap_or(0);
    println!(
        "{:<8} {:>7} {:>12} {:>6} {:>8}",
        "WAVE", "CLOSED", "IN-PROGRESS", "READY", "BLOCKED"
    );
    for w in out["waves"].as_array().cloned().unwrap_or_default() {
        println!(
            "{:<8} {:>7} {:>12} {:>6} {:>8}",
            format!("wave-{}", cell(&w["wave"])),
            cell(&w["closed"]),
            cell(&w["in_progress"]),
            cell(&w["ready"]),
            cell(&w["blocked"])
        );
    }
    let u = &out["unlabeled"];
    if cell(&u["total"]) > 0 {
        println!(
            "{:<8} {:>7} {:>12} {:>6} {:>8}",
            "-",
            cell(&u["closed"]),
            cell(&u["in_progress"]),
            cell(&u["ready"]),
            cell(&u["blocked"])
        );
    }
    match out["dispatching_wave"].as_u64() {
        Some(w) => println!("dispatching: wave-{w}"),
        None => println!("dispatching: none (no ready or in-progress wave beads)"),
    }
    let unknown = cell(&out["unlabeled"]["unknown"])
        + out["waves"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|w| cell(&w["unknown"]))
            .sum::<u64>();
    if unknown > 0 {
        println!(
            "{} {unknown} row(s) carry no recognized status — not counted as done",
            crate::output::warn("warn:")
        );
    }
    Ok(())
}

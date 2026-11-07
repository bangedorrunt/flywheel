//! `flywheel agent` subcommand types (split from `main.rs`, bd-qbi8).
//! bd-swarm-cockpit-pczs: `tail` reads a pin's recent actions from the
//! session the pane reports (`agent_session`) — the path/id names the exact
//! session, so there is no newest-file guessing. hc 0.1.16 reports no agent
//! session, so on the hc plane this bails loud and names the gap instead of
//! guessing; use `flywheel output` or `hc read`.

use std::io::IsTerminal;

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::Config;
use crate::output::print_json;

#[derive(Subcommand)]
pub(crate) enum AgentCmd {
    /// tail a pin's recent actions (tool calls / commands) from its agent session
    Tail {
        /// the pin (adjective+noun) or terminal name
        pin: String,
        /// project slug (default: the workspace cwd, like `steer`)
        #[arg(long)]
        project: Option<String>,
        /// machine-readable rows
        #[arg(long)]
        json: bool,
        /// how many recent actions to show
        #[arg(long, default_value_t = 20)]
        n: usize,
    },
}

pub(crate) fn cmd_agent(cfg: &Config, cmd: &AgentCmd) -> Result<()> {
    match cmd {
        AgentCmd::Tail {
            pin,
            project,
            json,
            n,
        } => cmd_agent_tail(cfg, pin, project.as_deref(), *json, *n),
    }
}

/// The pane a pin acts in. Live truth first (the pane's `toron_pin` token),
/// then the spawn registry's pinned mail identity — the same
/// name-vs-identity ladder `status` uses, so a numbered terminal name (oc2/pi2)
/// still resolves to its pin. Pure over its inputs.
fn pin_pane<'a>(
    panes: &'a [crate::hc::PaneInfo],
    registry: &[crate::state::SpawnEntry],
    pin: &str,
) -> Option<&'a crate::hc::PaneInfo> {
    if let Some(pane) = panes.iter().find(|p| {
        p.tokens.get("toron_pin").and_then(Value::as_str) == Some(pin)
            || p.display_agent.as_deref() == Some(pin)
    }) {
        return Some(pane);
    }
    let pane_id = registry
        .iter()
        .find(|e| e.mail_identity.as_deref() == Some(pin) || e.name == pin)
        .map(|e| e.pane_id.as_str())?;
    panes.iter().find(|p| p.pane_id == pane_id)
}

fn cmd_agent_tail(
    cfg: &Config,
    pin: &str,
    project: Option<&str>,
    json_out: bool,
    n: usize,
) -> Result<()> {
    if pin.trim().is_empty() {
        bail!("agent tail: pin must not be empty");
    }
    if pin.contains(char::is_whitespace) {
        bail!("agent tail: pin {pin:?} contains whitespace — pins are adjective+noun");
    }
    let n = n.clamp(1, 1000);
    let project = crate::cron::detect_project(cfg, project).ok_or_else(|| {
        anyhow!(
            "agent tail: no project — pass --project <slug> or run from a \
             workspace/<project> cwd"
        )
    })?;
    crate::check_project_spelling(&project, "agent tail")?;
    let dir = crate::project_dir_str(cfg, &project)?;
    let mux = crate::backend::multiplexer_backend(cfg)?;
    let _ws_id = crate::find_workspace_dyn(&*mux, &project)?;
    let panes = mux.pane_list(None)?;
    let registry = crate::state::load_spawn_registry(&dir);
    let Some(pane) = pin_pane(&panes, &registry, pin) else {
        let mut known: Vec<String> = panes
            .iter()
            .filter_map(|p| {
                p.tokens
                    .get("toron_pin")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        known.sort();
        known.dedup();
        bail!(
            "agent tail: no pane for pin {pin:?} in {project} — known pins: {}",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        );
    };
    let Some(session) = pane.agent_session.as_ref() else {
        bail!(
            "agent tail: pane {} ({pin}) reports no agent session — the harness \
             integration did not name one",
            pane.pane_id
        );
    };
    let tail = crate::hc::read_agent_session_tail(session, n)?;
    if json_out || !std::io::stdout().is_terminal() {
        print_json(&json!({
            "pin": pin,
            "pane_id": pane.pane_id,
            "agent": tail["agent"],
            "source": tail["source"],
            "session": tail["session"],
            "events": tail["events"],
        }));
        return Ok(());
    }
    for e in tail["events"].as_array().cloned().unwrap_or_default() {
        let ts = e["ts"].as_str().unwrap_or("-");
        let clock: String = ts
            .split_once('T')
            .map(|(_, t)| t)
            .unwrap_or(ts)
            .chars()
            .take(8)
            .collect();
        println!(
            "{} {} {}",
            crate::output::dim(&clock),
            crate::output::accent(e["tool"].as_str().unwrap_or("?")),
            e["detail"].as_str().unwrap_or("")
        );
    }
    Ok(())
}

#[cfg(test)]
mod plane_tests {
    /// `agent tail` reads live panes; on a rex crew it must ask the serving
    /// plane, not dial the hc socket. Source tripwire: the door builds its
    /// backend from config, so the seam is the assertion.
    #[test]
    fn agent_tail_resolves_the_active_plane() {
        let src = include_str!("agent.rs");
        let resolved = concat!("multiplexer_backend", "(cfg)");
        let hard_dial = concat!("Client", "::connect");
        assert!(
            src.contains(resolved),
            "agent tail must build the active-plane backend"
        );
        assert!(
            !src.contains(hard_dial),
            "agent tail must not hard-dial the hc socket"
        );
    }
}

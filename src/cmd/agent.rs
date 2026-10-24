//! `flywheel agent` subcommand types (split from `main.rs`, bd-qbi8).
//! bd-swarm-cockpit-pczs: `tail` reads a pin's recent actions from the
//! session herdr reports for its pane (`agent_session`) — the path/id names
//! the exact session, so there is no newest-file guessing.

use std::io::IsTerminal;

use anyhow::{Result, anyhow, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::Config;
use crate::herdr::Client;
use crate::output::print_json;

#[derive(Subcommand)]
pub(crate) enum AgentCmd {
    /// tail a pin's recent actions (tool calls / commands) from its agent session
    Tail {
        /// the pin (adjective+noun) or herdr name
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
/// name-vs-identity ladder `status` uses, so a numbered herdr name (oc2/pi2)
/// still resolves to its pin. Pure over its inputs.
fn pin_pane<'a>(
    panes: &'a [crate::herdr::PaneInfo],
    registry: &[crate::state::SpawnEntry],
    pin: &str,
) -> Option<&'a crate::herdr::PaneInfo> {
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
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = crate::find_workspace(&mut c, &project)?;
    let panes = c.pane_list(Some(&ws_id))?;
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
    let tail = crate::herdr::read_agent_session_tail(session, n)?;
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
        // ISO timestamps render as the clock time; a bare date stays as-is
        // (and an opencode `null` stays "-" — not recorded, never fabricated).
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

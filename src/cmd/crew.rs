//! `flywheel crew` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::config::Config;
use crate::herdr_shim::Client;
use crate::output::print_json;
use crate::state::EventKind;
use std::time::Duration;

/// ADR-0010 D7 crew plane: workflow preview + lifecycle verbs.
#[derive(Subcommand)]
pub(crate) enum CrewCmd {
    /// Preview the auto-crew a workflow would spawn (crew: auto)
    Preview {
        /// workflow name (built-in or catalog)
        name: String,
    },
    /// Lifecycle verbs for a spawned agent pane. NO arbitrary-text entry
    /// point: interrupt|exit|relaunch are the whole surface (ADR-0010 D7).
    Control {
        project: String,
        /// the pane target (herdr agent name, kind short, or pane id)
        pin: String,
        #[command(subcommand)]
        verb: ControlVerbCmd,
    },
}

/// The three control verbs (postconditions asserted in src/control.rs).
#[derive(Subcommand)]
pub(crate) enum ControlVerbCmd {
    /// Cancel the current tool call (Escape only — kill-style keys stay off
    /// this plane). Confirmed only when the composer is provably empty.
    Interrupt,
    /// Stop the agent pane. Reservations + dirty files preserved.
    Exit,
    /// Journaled relaunch: checkpoint the spawn record, stop, launch the
    /// same pin with an incarnation token; rollback on launch failure.
    Relaunch,
}

// ---- crew control (ADR-0010 D7) ----

/// `flywheel crew` — preview + lifecycle control plane. Preview mirrors the
/// `workflow crew <name>` report; control has NO arbitrary-text entry point
/// (interrupt|exit|relaunch are the whole surface).
pub(crate) fn cmd_crew(cfg: &Config, cmd: &CrewCmd) -> Result<()> {
    match cmd {
        CrewCmd::Preview { name } => {
            // bd-xmq5: shared merge — the project tier now participates
            // in crew preview like every other resolution path.
            let project_root = crate::project_dir(cfg, "flywheel");
            let all = crate::workflow_catalog::all_workflows_merged(Some(&project_root))?;
            let wf = all
                .get(name.as_str())
                .map(|(wf, _)| wf)
                .ok_or_else(|| anyhow!("unknown workflow {name:?} (flywheel workflow list)"))?;
            let resolve = |sub: &str| all.get(sub).map(|(w, _)| w.clone());
            let packed = crate::packed_from_flag(cfg, wf, &resolve, None, None)?;
            let members: Vec<Value> = packed
                .members
                .iter()
                .map(|(role, count)| json!({ "role": role, "count": count }))
                .collect();
            if members.is_empty() {
                println!("workflow {name} has no crew (crew omitted or no profile refs)");
                return Ok(());
            }
            print_json(&json!({
                "workflow": name,
                "crew": members,
                "budget": packed.budget,
                "cap": packed.cap,
                "unique": packed.unique,
                "aliases": packed.aliases,
                "announce": packed.announce(name),
            }));
            Ok(())
        }
        CrewCmd::Control { project, pin, verb } => cmd_crew_control(cfg, project, pin, verb),
    }
}

/// Marker-prefixed relaunch prime: the worker's handler template tells
/// control-plane prompts from captain payloads by the U+2063 + FLYWHEEL_OP:
/// prefix (byte-checked round-trip in src/control.rs tests).
fn relaunch_prime_line(name: &str, pin: Option<&str>, token: &str) -> String {
    crate::control::op_prompt(&format!(
        "CREW CONTROL relaunch: pane for {name} was recreated by the control plane \
         (incarnation {token}). Re-anchor to your current task and re-ack the \
         doorbell; your mail identity is {}.",
        pin.unwrap_or(name)
    ))
}

fn cmd_crew_control(cfg: &Config, project: &str, pin: &str, verb: &ControlVerbCmd) -> Result<()> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = crate::find_workspace(&mut c, project)?;
    let targets = crate::resolve_agents(&mut c, &ws_id, project, pin)?;
    if targets.len() != 1 {
        bail!(
            "crew control targets ONE pane — '{pin}' resolved {} ({}); name one pin",
            targets.len(),
            targets
                .iter()
                .filter_map(|a| a.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let a = &targets[0];
    let name = a
        .name
        .clone()
        .unwrap_or_else(|| crate::agent_target(a).to_string());
    match *verb {
        ControlVerbCmd::Interrupt => {
            let status = crate::herdr_shim::parse_agent_status(a.agent_status.as_str())
                .unwrap_or(crate::herdr_shim::AgentStatus::Unknown);
            let plan = crate::control::interrupt_plan(status);
            c.agent_send_keys(crate::agent_target(a), plan.keys)?;
            let verdict = if plan.confirmed {
                "confirmed (composer provably empty)"
            } else {
                "UNCONFIRMED (agent was working — re-check the pane)"
            };
            println!("interrupted {name} — cancel {verdict}");
            crate::log(
                project,
                EventKind::Interrupt,
                json!({ "to": pin, "keys": plan.keys, "confirmed": plan.confirmed, "plane": "crew-control" }),
            );
        }
        ControlVerbCmd::Exit => {
            // pane.close — the agent goes with the pane. Exit deliberately
            // touches NOTHING else: reservations (toron leases) and dirty
            // worktree files are preserved (control::EXIT_POSTCONDITION).
            c.call_coded("pane.close", json!({ "pane_id": a.pane_id }))
                .with_context(|| format!("pane.close for {name} ({})", a.pane_id))?;
            println!(
                "exited {name} ({}) — reservations + dirty files preserved",
                a.pane_id
            );
            crate::log(
                project,
                EventKind::State,
                json!({ "op": "crew-control exit", "name": name, "pane_id": a.pane_id }),
            );
        }
        ControlVerbCmd::Relaunch => {
            let registry = crate::state::load_spawn_registry(project);
            let old = registry
                .iter()
                .find(|e| e.name == name || e.pane_id == a.pane_id)
                .cloned()
                .ok_or_else(|| anyhow!("{name} ({}) not in the spawn registry — relaunch reconstructs the launch from the registry profile row", a.pane_id))?;
            let catalog = crate::profile::ProfileCatalog::load()?;
            let profile = old.profile.as_ref().ok_or_else(|| anyhow!(
                "{name} has no profile row — crew-control relaunch reconstructs launch args from the profile; re-spawn instead (flywheel spawn {project})"
            ))?;
            let p = catalog
                .get(profile)
                .ok_or_else(|| anyhow!("unknown profile '{profile}' (flywheel profile list)"))?;
            let kind = old.harness.clone().unwrap_or_else(|| p.harness.clone());
            let args = crate::profile_launch_args(&kind, p, None)?;

            // Journaled transaction: checkpoint → stop → launch → commit/rollback.
            crate::control::save_relaunch_journal(project, &old)?;
            c.call_coded("pane.close", json!({ "pane_id": a.pane_id }))
                .with_context(|| format!("stop old pane for {name} ({})", a.pane_id))?;

            // Fresh pane from the workspace root (same env contract as add).
            let panes = c.pane_list(Some(&ws_id))?;
            let root = panes
                .first()
                .ok_or_else(|| anyhow!("workspace {ws_id} has no panes"))?;
            let mail_identity = old.mail_identity.clone();
            let pane_env = crate::herdr_shim::herdr_integration_env(cfg, mail_identity.as_deref());
            let dir = crate::project_dir(cfg, project);
            let new_pane = crate::herdr_shim::pane_split(
                &mut c,
                &root.pane_id,
                "down",
                Some(&crate::utf8_path(&dir, "workspace dir")?),
                Some(&pane_env),
            )?;
            let pane_id = new_pane.pane_id.clone();
            let shell_ready =
                crate::herdr_shim::wait_shell_prompt(&mut c, &pane_id, Duration::from_secs(15));
            if !shell_ready {
                eprintln!(
                    "flywheel: warn: shell for {name} ({pane_id}) did not render a prompt \
                     within 15s — the launch command may be mangled by the booting shell"
                );
            }
            let launch = crate::start_agent_with_retry(&mut c, &name, &kind, &pane_id, &args);
            match crate::control::relaunch_decide(launch.is_ok()) {
                crate::control::RelaunchAction::Commit => {
                    let _ = launch.with_context(|| format!("launch {name} ({kind})"))?;
                    let _ = crate::herdr_shim::wait_agent_foreground(
                        &mut c,
                        &pane_id,
                        &kind,
                        &name,
                        Duration::from_secs(15),
                    );
                    // Published record wins even when the agent is unconfirmed
                    // (relaunch_decide already committed on launch Ok).
                    let mut entries = crate::state::load_spawn_registry(project);
                    if let Some(e) = entries
                        .iter_mut()
                        .find(|e| e.name == name || e.pane_id == old.pane_id)
                    {
                        e.pane_id = pane_id.clone();
                        e.incarnation += 1;
                    }
                    crate::state::save_spawn_registry(project, &entries)?;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    let token = crate::control::incarnation_token(&old, now);
                    crate::prime_line_via_send_text(
                        &mut c,
                        &pane_id,
                        &relaunch_prime_line(&name, mail_identity.as_deref(), &token),
                    );
                    crate::control::clear_relaunch_journal(project)?;
                    println!(
                        "relaunched {name} → {pane_id} (incarnation {token}; reservations + dirty preserved)"
                    );
                    crate::log(
                        project,
                        EventKind::Respawn,
                        json!({ "op": "crew-control relaunch", "name": name, "pane_id": pane_id, "token": token }),
                    );
                }
                crate::control::RelaunchAction::Rollback => {
                    let journal = crate::control::load_relaunch_journal(project)?
                        .ok_or_else(|| anyhow!("relaunch journal vanished mid-transaction"))?;
                    crate::control::rollback_relaunch(project, &journal)?;
                    crate::control::clear_relaunch_journal(project)?;
                    crate::log(
                        project,
                        EventKind::Respawn,
                        json!({ "op": "crew-control relaunch ROLLED BACK", "name": name, "err": format!("{:#}", launch.unwrap_err()) }),
                    );
                    bail!(
                        "relaunch of {name} failed after the stop — the prior durable record is restored; \
                         the old pane ({}) is closed and the workspace has no agent for it: re-spawn or retry",
                        old.pane_id
                    );
                }
            }
        }
    }
    Ok(())
}

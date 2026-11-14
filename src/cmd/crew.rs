//! `flywheel crew` subcommand types (split from `main.rs`, bd-qbi8).

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use std::process::Command;

use crate::config::Config;
use crate::hc::Client;
use crate::output::print_json;
use crate::state::EventKind;
use std::time::Duration;

/// ADR-0010 D7 crew plane: workflow preview + lifecycle verbs.
#[derive(Subcommand, Debug)]
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
        /// the pane target (hc agent name, kind short, or pane id)
        pin: String,
        #[command(subcommand)]
        verb: ControlVerbCmd,
    },
    /// High-level dispatch: gift-wrap the prompt into toron mail, stamp it on
    /// the mail log, and deliver a compliant wake line into the target pane.
    Prompt {
        /// target agent pin / pane identity
        pin: String,
        /// task prompt body (stamped on the mail log)
        prompt: String,
        /// project slug
        #[arg(long)]
        project: String,
        /// sender identity (mail `--as`)
        #[arg(long = "as")]
        r#as: Option<String>,
        /// mark the dispatch urgent (mail importance + urgent wake line)
        #[arg(long)]
        urgent: bool,
    },
}

/// The three control verbs (postconditions asserted in src/control.rs).
#[derive(Subcommand, Debug)]
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

/// `flywheel crew` — preview + lifecycle control plane. Preview mirrors the
/// `workflow crew <name>` report; control has NO arbitrary-text entry point
/// (interrupt|exit|relaunch are the whole surface).
pub(crate) fn cmd_crew(cfg: &Config, cmd: &CrewCmd) -> Result<()> {
    match cmd {
        CrewCmd::Preview { name } => {
            let project_root = crate::project_dir(cfg, "flywheel");
            let all = crate::workflow_catalog::all_workflows_merged(Some(&project_root))?;
            let picked = crate::workflow_catalog::pick_workflow(&all, name.as_str())
                .map_err(|e| anyhow!("{e}"))?;
            let wf = &picked.0;
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
        CrewCmd::Prompt {
            pin,
            prompt,
            project,
            r#as,
            urgent,
        } => cmd_crew_prompt(cfg, project, pin, prompt, r#as.as_deref(), *urgent),
    }
}

/// The rex arm: resolve through the selected plane. `exit` is real
/// (`pane_close`); the key-injection verbs are hc-only today and must say so
/// with the working alternative, never a false "no swarm".
fn cmd_crew_control_rex(
    cfg: &Config,
    project: &str,
    pin: &str,
    verb: &ControlVerbCmd,
) -> Result<()> {
    let backend = crate::backend::factory::multiplexer_backend(cfg)?;
    let ws_id = crate::find_workspace_dyn(&*backend, project)?;
    let targets = crate::resolve_agents_dyn(&*backend, &ws_id, project, pin)?;
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
    let name = a.name.clone().unwrap_or_else(|| a.pane_id.clone());
    match *verb {
        ControlVerbCmd::Exit => {
            backend
                .pane_close(&a.pane_id)
                .map_err(|e| anyhow!("pane.close for {name} ({}): {e}", a.pane_id))?;
            println!(
                "exited {name} ({}) — reservations + dirty files preserved",
                a.pane_id
            );
            crate::log(
                project,
                EventKind::Interrupt,
                json!({ "to": pin, "verb": "exit", "pane": a.pane_id, "plane": "crew-control" }),
            );
        }
        ControlVerbCmd::Interrupt => bail!(
            "crew control interrupt needs hc key injection; on rex cancel the tool call with              `rex send-key {pane} escape` (pane {name})",
            pane = a.pane_id,
            name = name
        ),
        ControlVerbCmd::Relaunch => bail!(
            "crew control relaunch is hc-only today; on rex use `flywheel recover {project}` to              respawn the same pin after a pane is gone, then `flywheel send` to re-prime"
        ),
    }
    Ok(())
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
    // ADR-0045: the pane plane is rex-first. The hc client below cannot see a
    // rex workspace, so the door answered "no flywheel swarm" while the panes
    // were live. Resolve through the same door `status` trusts.
    if crate::pane::rex_door_selected(cfg) {
        return cmd_crew_control_rex(cfg, project, pin, verb);
    }
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
            let status = crate::hc::parse_agent_status(a.agent_status.as_str())
                .unwrap_or(crate::hc::AgentStatus::Unknown);
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
            c.terminal_kill(&a.pane_id)
                .map(|_| ())
                .with_context(|| format!("terminal.kill for {name} ({})", a.pane_id))?;
            let _ = c.terminal_rm(&a.pane_id);
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
            let plan = crate::profile_launch_plan(&kind, p, None)?;
            let args = plan.args.clone();

            crate::control::save_relaunch_journal(project, &old)?;
            c.terminal_kill(&a.pane_id)
                .map(|_| ())
                .with_context(|| format!("stop old terminal for {name} ({})", a.pane_id))?;
            let _ = c.terminal_rm(&a.pane_id);

            let panes = c.pane_list()?;
            let root = panes
                .first()
                .ok_or_else(|| anyhow!("workspace {ws_id} has no panes"))?;
            let mail_identity = old.mail_identity.clone();
            let mut pane_env = crate::hc::plane_launch_env(cfg, mail_identity.as_deref());
            if let Some(m) = &plan.env_model_pin {
                crate::hc::apply_opencode_model_pin(&mut pane_env, m)?;
            }
            let dir = crate::project_dir(cfg, project);
            let new_pane = crate::hc::pane_split(
                &c,
                &root.pane_id,
                "down",
                Some(&crate::utf8_path(&dir, "workspace dir")?),
                Some(&pane_env),
            )?;
            let pane_id = new_pane.pane_id.clone();
            let shell_ready = crate::hc::wait_shell_prompt(&c, &pane_id, Duration::from_secs(15));
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
                    let _ = crate::hc::wait_agent_foreground(
                        &c,
                        &pane_id,
                        &kind,
                        &name,
                        Duration::from_secs(15),
                    );
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptDispatch {
    pub pin: String,
    pub project: String,
    pub sender: String,
    pub prompt: String,
    pub urgent: bool,
}

impl PromptDispatch {
    pub(crate) fn subject(&self) -> String {
        format!("crew prompt for {}", self.pin)
    }

    pub(crate) fn mail_args(&self) -> Vec<String> {
        let mut args = vec![
            "mail".to_string(),
            "send".to_string(),
            "--project".to_string(),
            self.project.clone(),
            "--as".to_string(),
            self.sender.clone(),
            "--to".to_string(),
            self.pin.clone(),
            "--subject".to_string(),
            self.subject(),
            "--body".to_string(),
            self.prompt.clone(),
        ];
        if self.urgent {
            args.push("--importance".to_string());
            args.push("urgent".to_string());
        }
        args
    }

    pub(crate) fn wake_line(&self, wm: u64) -> String {
        let urgency = if self.urgent {
            crate::pane_write::Urgency::Urgent
        } else {
            crate::pane_write::Urgency::Normal
        };
        crate::pane_write::wake_line(crate::pane_write::Wm(wm), urgency)
    }

    pub(crate) fn confirmation(&self, wm: u64) -> Value {
        json!({
            "pin": self.pin,
            "sent": true,
            "wake": self.wake_line(wm),
            "urgent": self.urgent,
            "project": self.project,
        })
    }
}

fn prompt_sender(cfg: &Config, r#as: Option<&str>) -> String {
    if let Some(s) = r#as.filter(|s| !s.trim().is_empty()) {
        return s.to_string();
    }
    if let Ok(s) = std::env::var("AGENT_NAME")
        && !s.trim().is_empty()
    {
        return s;
    }
    if let Some(name) = cfg
        .agent_mail
        .as_ref()
        .and_then(|m| m.name.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return name.to_string();
    }
    "agent".to_string()
}

fn harness_kind_for_target(project: &str, pane_id: &str, name: Option<&str>) -> String {
    let reg = crate::state::load_spawn_registry(project);
    reg.into_iter()
        .find(|e| e.pane_id == pane_id || name.is_some_and(|n| e.name == n))
        .and_then(|e| e.harness)
        .unwrap_or_default()
}

fn toron_mail_send(dispatch: &PromptDispatch) -> Result<()> {
    let args = dispatch.mail_args();
    let out = Command::new("toron")
        .args(&args)
        .output()
        .with_context(|| "run toron mail send")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        bail!(
            "toron mail send failed for {}: {}{}",
            dispatch.pin,
            stderr.trim(),
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!(" {}", stdout.trim())
            }
        );
    }
    Ok(())
}

fn recipient_wm(project: &str, pin: &str) -> u64 {
    let Ok(mb) = crate::mail::comms_mailbox() else {
        return 0;
    };
    let Ok(store) = crate::comms_send::store() else {
        return 0;
    };
    crate::mail::block_on(async {
        let Ok(npub) = mb.comms_npub(project, pin).await else {
            return 0;
        };
        store.head(&npub).await.unwrap_or(0)
    })
}

/// The line this wake puts on the pane (urgent or normal form).
fn prompt_wake_line(wm: u64, urgent: bool) -> String {
    crate::pane_write::wake_line(
        crate::pane_write::Wm(wm),
        if urgent {
            crate::pane_write::Urgency::Urgent
        } else {
            crate::pane_write::Urgency::Normal
        },
    )
}

/// Ring one crew-prompt wake. `ring_wake` demands the FOLD line — always the
/// normal form — and types from `urgency` itself, so the urgent form must
/// never be handed to it.
fn ring_prompt_wake(
    typer: &dyn crate::pane_write::PaneTyper,
    target: &str,
    wm: u64,
    urgent: bool,
    route: crate::harness_caps::WakeRoute,
) -> Result<crate::inbox_ports::RingDone, String> {
    let urgency = if urgent {
        toron_core::wake::Urgency::Urgent
    } else {
        toron_core::wake::Urgency::Normal
    };
    let fold = crate::pane_write::wake_line(
        crate::pane_write::Wm(wm),
        crate::pane_write::Urgency::Normal,
    );
    crate::inbox_ports::ring_wake(typer, target, wm, &fold, urgency, route)
}

/// The wake line a busy pane refused: the same text the ring would have typed.
fn wake_is_busy(err: &str) -> bool {
    err.contains("not at a prompt") || err.to_ascii_lowercase().contains("busy")
}

/// A busy pane's mail is already delivered; the ring is deferred to the
/// project's live dispatcher, which folds the deferral as a refusal (working
/// pin queues it, the next at-prompt status rings it). The handoff is
/// project-keyed on disk, so the dispatcher's workspace id never has to match.
fn finish_prompt(
    delivered: Result<String, String>,
    defer: impl FnOnce() -> Result<()>,
) -> Result<String> {
    match delivered {
        Ok(typed) => Ok(typed),
        Err(err) if wake_is_busy(&err) => {
            defer()?;
            Ok(QUEUED_WAKE.to_string())
        }
        Err(err) => Err(anyhow!("{err}")),
    }
}

const QUEUED_WAKE: &str = "mail delivered; the doorbell rings at the next prompt";

/// Record the refused wake for the project's dispatcher, in the one line shape
/// the fold treats as a mail wake.
fn defer_busy_wake(project: &str, pin: &str) -> Result<()> {
    crate::doorbell::append_deferred_wake(
        &crate::config::state_dir(),
        project,
        pin,
        &crate::doorbell::deferred_wake_line(),
    )
    .map_err(|e| anyhow!("record the deferred wake: {e}"))
}

fn deliver_prompt_wake_hc(
    cfg: &Config,
    project: &str,
    pin: &str,
    urgent: bool,
    wm: u64,
) -> Result<String> {
    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = crate::find_workspace(&mut c, project)?;
    let targets = crate::resolve_agents(&mut c, &ws_id, project, pin)?;
    if targets.len() != 1 {
        bail!(
            "crew prompt targets ONE pane — '{pin}' resolved {} ({}); name one pin",
            targets.len(),
            targets
                .iter()
                .filter_map(|a| a.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let a = &targets[0];
    let target = crate::agent_target(a).to_string();
    let kind = harness_kind_for_target(project, &a.pane_id, a.name.as_deref());
    let kind = if kind.is_empty() {
        a.agent.as_deref().unwrap_or("").to_string()
    } else {
        kind
    };
    let route = crate::harness_caps::wake_route(&kind, crate::harness_caps::installed_from_env());
    match ring_prompt_wake(&crate::pane_write::HcPanes(&c), &target, wm, urgent, route) {
        Ok(crate::inbox_ports::RingDone::Typed) => Ok(prompt_wake_line(wm, urgent)),
        Ok(crate::inbox_ports::RingDone::NoDoor(no_door)) => {
            bail!(
                "wake line could not be delivered to {pin} ({target}): {}",
                crate::harness_caps::no_door_line(&no_door)
            )
        }
        Err(e) => bail!("wake line delivery to {pin} ({target}) failed: {e}"),
    }
}

#[cfg(feature = "backend-superlogical")]
fn deliver_prompt_wake_rex(
    cfg: &Config,
    project: &str,
    pin: &str,
    urgent: bool,
    wm: u64,
) -> Result<String> {
    let backend = crate::backend::multiplexer_backend(cfg)?;
    let ws_id = crate::find_workspace_dyn(&*backend, project)?;
    let targets = crate::resolve_agents_dyn(&*backend, &ws_id, project, pin)?;
    if targets.len() != 1 {
        bail!(
            "crew prompt targets ONE pane — '{pin}' resolved {} ({}); name one pin",
            targets.len(),
            targets
                .iter()
                .filter_map(|a| a.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let a = &targets[0];
    let target = a.pane_id.clone();
    let kind = harness_kind_for_target(project, &a.pane_id, a.name.as_deref());
    let kind = if kind.is_empty() {
        a.agent.as_deref().unwrap_or("").to_string()
    } else {
        kind
    };
    let route = crate::harness_caps::wake_route(&kind, crate::harness_caps::installed_from_env());
    match ring_prompt_wake(
        &crate::pane_write::RexPanes(&*backend),
        &target,
        wm,
        urgent,
        route,
    ) {
        Ok(crate::inbox_ports::RingDone::Typed) => Ok(prompt_wake_line(wm, urgent)),
        Ok(crate::inbox_ports::RingDone::NoDoor(no_door)) => {
            bail!(
                "wake line could not be delivered to {pin} ({target}): {}",
                crate::harness_caps::no_door_line(&no_door)
            )
        }
        Err(e) => bail!("wake line delivery to {pin} ({target}) failed: {e}"),
    }
}

fn deliver_prompt_wake(
    cfg: &Config,
    project: &str,
    pin: &str,
    urgent: bool,
    wm: u64,
) -> Result<String> {
    if crate::pane::rex_door_selected(cfg) {
        #[cfg(feature = "backend-superlogical")]
        {
            return deliver_prompt_wake_rex(cfg, project, pin, urgent, wm);
        }
        #[cfg(not(feature = "backend-superlogical"))]
        {
            let _ = (cfg, project, pin, urgent, wm);
            bail!("rex backend selected but backend-superlogical feature is not enabled");
        }
    }
    deliver_prompt_wake_hc(cfg, project, pin, urgent, wm)
}

fn cmd_crew_prompt(
    cfg: &Config,
    project: &str,
    pin: &str,
    prompt: &str,
    r#as: Option<&str>,
    urgent: bool,
) -> Result<()> {
    let sender = prompt_sender(cfg, r#as);
    let dispatch = PromptDispatch {
        pin: pin.to_string(),
        project: project.to_string(),
        sender,
        prompt: prompt.to_string(),
        urgent,
    };

    toron_mail_send(&dispatch)?;
    let wm = recipient_wm(project, pin);
    let delivered =
        deliver_prompt_wake(cfg, project, pin, urgent, wm).map_err(|e| format!("{e:#}"));
    let wake = finish_prompt(delivered, || defer_busy_wake(project, pin))?;
    let queued = wake == QUEUED_WAKE;

    crate::log(
        project,
        EventKind::Send,
        json!({
            "op": "crew-prompt",
            "to": pin,
            "urgent": urgent,
            "wake": wake,
            "wake_queued": queued,
            "wm": wm,
        }),
    );
    print_json(&dispatch.confirmation(wm));
    if queued {
        println!("{QUEUED_WAKE}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct Cli {
        #[command(subcommand)]
        cmd: CrewCmd,
    }

    #[test]
    fn crew_prompt_parses_project_as_and_urgent_flags() {
        let cli = Cli::try_parse_from([
            "flywheel",
            "prompt",
            "BlackGorge",
            "implement bead",
            "--project",
            "flywheel",
            "--as",
            "AmberHarbor",
            "--urgent",
        ])
        .expect("parse crew prompt");
        match cli.cmd {
            CrewCmd::Prompt {
                pin,
                prompt,
                project,
                r#as,
                urgent,
            } => {
                assert_eq!(pin, "BlackGorge");
                assert_eq!(prompt, "implement bead");
                assert_eq!(project, "flywheel");
                assert_eq!(r#as.as_deref(), Some("AmberHarbor"));
                assert!(urgent);
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn prompt_dispatch_structure_builds_mail_and_wake() {
        let dispatch = PromptDispatch {
            pin: "BlackGorge".to_string(),
            project: "flywheel".to_string(),
            sender: "AmberHarbor".to_string(),
            prompt: "do the thing".to_string(),
            urgent: true,
        };
        let args = dispatch.mail_args();
        assert_eq!(
            args,
            vec![
                "mail",
                "send",
                "--project",
                "flywheel",
                "--as",
                "AmberHarbor",
                "--to",
                "BlackGorge",
                "--subject",
                "crew prompt for BlackGorge",
                "--body",
                "do the thing",
                "--importance",
                "urgent",
            ]
        );
        let wake = dispatch.wake_line(42);
        assert!(
            wake.contains("toron next"),
            "wake must be compliant: {wake}"
        );
        let confirmation = dispatch.confirmation(42);
        assert_eq!(confirmation["pin"], "BlackGorge");
        assert_eq!(confirmation["sent"], true);
        assert_eq!(confirmation["urgent"], true);
        assert_eq!(confirmation["wake"], wake);
        assert_eq!(confirmation["project"], "flywheel");
    }

    /// A pane that records every boundary write.
    #[derive(Default)]
    struct RecTyper(std::sync::Mutex<Vec<String>>);

    impl crate::pane_write::PaneTyper for RecTyper {
        fn prompt(&self, target: &str, line: &str) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("prompt {target} {line}"));
            Ok(())
        }
        fn keys(&self, target: &str, keys: &[&str]) -> anyhow::Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("keys {target} {}", keys.join("+")));
            Ok(())
        }
        fn close(&self, pane_id: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("close {pane_id}"));
            Ok(())
        }
    }

    fn writes(rec: &RecTyper) -> Vec<String> {
        rec.0.lock().unwrap().clone()
    }

    /// bd-vb96v: `--urgent` must ring. `ring_wake` checks the FOLD line (always
    /// the normal form) and types from `urgency`; handing it the urgent line is
    /// the bug that made every urgent prompt fail after its mail went out.
    #[test]
    fn urgent_prompt_wake_types_the_urgent_line_and_returns_ok() {
        use crate::harness_caps::WakeRoute;
        let rec = RecTyper::default();
        let got = ring_prompt_wake(
            &rec,
            "block:pane-1",
            42,
            true,
            WakeRoute::Inject { interrupt: true },
        )
        .expect("an urgent crew prompt must ring");
        assert_eq!(got, crate::inbox_ports::RingDone::Typed);
        let typed = prompt_wake_line(42, true);
        assert!(typed.contains("URGENT"), "urgent form: {typed}");
        assert_eq!(
            writes(&rec),
            vec![
                "keys block:pane-1 Escape".to_string(),
                format!("prompt block:pane-1 {typed}"),
            ],
            "Interrupt then the urgent wake line"
        );
    }

    #[test]
    fn normal_prompt_wake_types_the_normal_line_only() {
        use crate::harness_caps::WakeRoute;
        let rec = RecTyper::default();
        let got = ring_prompt_wake(
            &rec,
            "block:pane-1",
            42,
            false,
            WakeRoute::Inject { interrupt: true },
        )
        .expect("a normal crew prompt must ring");
        assert_eq!(got, crate::inbox_ports::RingDone::Typed);
        let typed = prompt_wake_line(42, false);
        assert!(typed.contains("toron next"), "wake is compliant: {typed}");
        assert_eq!(writes(&rec), vec![format!("prompt block:pane-1 {typed}")]);
    }

    /// A busy pane's mail is already delivered, so the CLI hands the ring to
    /// the project's dispatcher instead of pretending a journal row would ring.
    #[test]
    fn busy_prompt_hands_the_ring_to_the_dispatcher() {
        let _g = crate::config::test_state_home();
        let project = format!("busy-defer-{}", std::process::id());
        let out = finish_prompt(
            Err(
                "wake line delivery to BlueRidge (block:p1) failed: pane busy: not at a prompt"
                    .into(),
            ),
            || defer_busy_wake(&project, "BlueRidge"),
        )
        .expect("a busy pane must not fail the prompt");
        assert_eq!(out, QUEUED_WAKE);
        assert!(
            out.contains("doorbell rings at the next prompt"),
            "the operator is told who rings: {out}"
        );
        let hosted: std::collections::BTreeSet<String> =
            ["BlueRidge".to_string()].into_iter().collect();
        assert_eq!(
            crate::doorbell::take_deferred_wakes(
                &crate::config::state_dir(),
                &project,
                "session:01a1d257",
                &hosted
            ),
            vec![(
                "BlueRidge".to_string(),
                "[doorbell] plane-P mail. Pull: toron next".to_string()
            )],
            "the refused wake is on the dispatcher's queue as a mail wake"
        );
    }

    #[test]
    fn non_busy_prompt_failure_stays_loud() {
        let mut deferred = false;
        let err = finish_prompt(
            Err("wake line delivery failed: hc connect refused".into()),
            || {
                deferred = true;
                Ok(())
            },
        )
        .expect_err("a transport failure is not a deferral");
        assert!(format!("{err:#}").contains("hc connect refused"));
        assert!(!deferred, "a transport failure must not be deferred");
    }

    /// The doorbell itself rings the deferred wake: one refusal is queued by
    /// the sink/reducer, the pane's next at-prompt status rings exactly once,
    /// and a second idle emits nothing (bd-346jl e2e at the seam the CLI feeds).
    #[test]
    fn a_busy_crew_prompt_rings_once_at_the_next_prompt() {
        let mut model = crate::workspace::WorkspaceModel::default();
        model.doorbell.index.insert_pin("BlueRidge".into(), None);
        let msg = crate::workspace::Msg::PromptRefused {
            pin: "BlueRidge".into(),
            line: "⟦toron⟧ URGENT steer (wm 1) — run: toron next".into(),
        };
        let (model, cmds) = crate::workspace::tick(&model, &msg);
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, crate::workspace::Cmd::Prompt { .. })),
            "a refused prompt does not ring while the pane is busy: {cmds:?}"
        );
        let (model, idle) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        let rings: Vec<&crate::workspace::Cmd> = idle
            .iter()
            .filter(|c| matches!(c, crate::workspace::Cmd::Prompt { .. }))
            .collect();
        assert_eq!(rings.len(), 1, "exactly one ring at the prompt: {idle:?}");
        let (_model, again) = crate::workspace::tick(
            &model,
            &crate::workspace::Msg::PlaneStatus {
                pin: "BlueRidge".into(),
                status: "idle".into(),
            },
        );
        assert!(
            !again
                .iter()
                .any(|c| matches!(c, crate::workspace::Cmd::Prompt { .. })),
            "a second idle emits nothing: {again:?}"
        );
    }

    /// The contract the helper exists for: the fold line is never the urgent
    /// form, so the old passing of `wake_line(.., Urgent)` can only fail.
    #[test]
    fn ring_wake_refuses_an_urgent_fold_line() {
        use crate::harness_caps::WakeRoute;
        let rec = RecTyper::default();
        let err = crate::inbox_ports::ring_wake(
            &rec,
            "block:pane-1",
            42,
            &prompt_wake_line(42, true),
            toron_core::wake::Urgency::Urgent,
            WakeRoute::Inject { interrupt: true },
        )
        .expect_err("the fold must name the normal line");
        assert!(err.contains("fold line differs"), "got: {err}");
        assert!(writes(&rec).is_empty(), "a refused ring types nothing");
    }
}

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
        /// the work-ledger bead this prompt is about. Rides `corr`, so a later
        /// prompt for the SAME bead supersedes the queued one (bd-0uzrq.11).
        #[arg(long)]
        bead: Option<String>,
    },
    /// The crew's structured reply: closes the brief, wakes the captain once
    /// (deferred if the captain is busy), and is safe to replay (bd-0uzrq.4).
    Done {
        /// the brief id the dispatch carried (`Brief id: <id>`)
        brief: String,
        /// free-text proof (kept, but untyped — prefer --receipt-json)
        #[arg(long)]
        receipt: Option<String>,
        /// typed flywheel-crew-receipt.v1 file, or `-` for stdin
        #[arg(long)]
        receipt_json: Option<String>,
        /// project slug
        #[arg(long)]
        project: String,
        /// report as this pin (`--as`)
        #[arg(long = "as")]
        r#as: Option<String>,
    },
    /// Land every bead in a brief's typed receipt via `ship land` (bd-bi0mv).
    Land {
        /// the brief id whose typed receipt names the beads
        brief: String,
        /// the green proof receipt passed to `ship land --receipt`
        #[arg(long)]
        proof_receipt: std::path::PathBuf,
        /// project slug for the reservation release
        #[arg(long)]
        project: Option<String>,
        /// agent pin releasing reservations (`--as`)
        #[arg(long = "as")]
        r#as: Option<String>,
    },
    /// Open briefs per pin, with their age. A silent crew is a row here.
    Inbox {
        /// project slug
        #[arg(long)]
        project: String,
        /// only briefs with no reply yet
        #[arg(long)]
        open: bool,
        /// one crew's outstanding briefs
        #[arg(long)]
        pin: Option<String>,
        #[arg(long)]
        json: bool,
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
            bead,
        } => cmd_crew_prompt(
            cfg,
            project,
            pin,
            prompt,
            r#as.as_deref(),
            *urgent,
            bead.as_deref(),
        ),
        CrewCmd::Done {
            brief,
            receipt,
            receipt_json,
            project,
            r#as,
        } => cmd_crew_done(
            cfg,
            project,
            brief,
            receipt.as_deref(),
            receipt_json.as_deref(),
            r#as.as_deref(),
        ),
        CrewCmd::Land {
            brief,
            proof_receipt,
            project,
            r#as,
        } => cmd_crew_land(
            cfg,
            brief,
            proof_receipt,
            project.as_deref(),
            r#as.as_deref(),
        ),
        CrewCmd::Inbox {
            project,
            open,
            pin,
            json,
        } => cmd_crew_inbox(cfg, project, *open, pin.as_deref(), *json),
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
    /// The bead this prompt is about; rides `corr` so the doorbell can
    /// supersede an older brief about the same work.
    pub bead: Option<String>,
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
        if let Some(bead) = &self.bead {
            args.push("--corr".to_string());
            args.push(bead.clone());
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

/// bd-f888z: the recorded harness session for a target seat, if any. Drives
/// the API wake route for opencode seats.
fn harness_session_for_target(project: &str, pane_id: &str, name: Option<&str>) -> Option<String> {
    crate::state::load_spawn_registry(project)
        .into_iter()
        .find(|e| e.pane_id == pane_id || name.is_some_and(|n| e.name == n))
        .and_then(|e| e.harness_session)
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

/// Ring one crew-prompt wake and CONFIRM the pane turned it into a turn.
/// `ring_wake` demands the FOLD line — always the normal form — and types from
/// `urgency` itself, so the urgent form must never be handed to it.
///
/// The confirmation is the piece bd-0uzrq.16 adds: a typed wake can be written
/// and submitted with nothing behind it (a reset opencode sitting on its
/// welcome screen swallowed the paste live, 2026-11-15 13:47Z), and a writer
/// that trusts its own write reports a wake that never rang. `wait_turn` asks
/// the caller's own status plane, so this stays harness-agnostic.
#[allow(clippy::too_many_arguments)]
fn ring_prompt_wake(
    typer: &dyn crate::pane_write::PaneTyper,
    reader: &dyn crate::pane_read::PaneReader,
    project: &str,
    harness: &str,
    target: &str,
    wm: u64,
    urgent: bool,
    route: crate::harness_caps::WakeRoute,
    probe: &dyn Fn(&str) -> Result<(String, bool)>,
    bound: Duration,
) -> Result<crate::inbox_ports::RingDone, String> {
    // bd-0uzrq.3: the pane belongs to whoever is typing in it. A captain-sent
    // prompt is still a prompt into a shared pane, and --urgent does not
    // override a half-typed line.
    crate::pane_read::defer_if_operator_typing(reader, harness, target, project)?;
    let urgency = if urgent {
        toron_core::wake::Urgency::Urgent
    } else {
        toron_core::wake::Urgency::Normal
    };
    let fold = crate::pane_write::wake_line(
        crate::pane_write::Wm(wm),
        crate::pane_write::Urgency::Normal,
    );
    // Taken BEFORE the write: a pane whose status record is already stuck at
    // `working` (bd-0uzrq.10) would otherwise confirm a wake it never consumed,
    // so the proof is a turn AND a screen that moved off this capture.
    // shortcut: an unreadable pre-capture degrades the proof to "a turn is in
    // flight"; upgrade if a harness is found where capture fails but status is
    // trustworthy.
    let before = probe(target).ok().map(|(screen, _)| screen_digest(&screen));
    let done = crate::inbox_ports::ring_wake(
        typer,
        target,
        wm,
        &fold,
        urgency,
        route,
        crate::pane_screen::InputLine::Unknown,
    )?;
    if done != crate::inbox_ports::RingDone::Typed {
        return Ok(done);
    }
    match wait_turn(target, bound, before, probe) {
        Ok(true) => Ok(done),
        Ok(false) => Err(wake_not_landed(target, bound)),
        Err(e) => Err(format!("{} ({e})", wake_not_landed(target, bound))),
    }
}

/// Poll until the pane reports a turn in flight AND its screen differs from the
/// pre-write capture. Both halves are load-bearing: the screen alone moves on an
/// idle spinner, and the status alone can be a stale `working` record.
fn wait_turn(
    target: &str,
    bound: Duration,
    before: Option<u64>,
    probe: &dyn Fn(&str) -> Result<(String, bool)>,
) -> Result<bool> {
    let deadline = std::time::Instant::now() + bound;
    loop {
        let (screen, in_turn) = probe(target)?;
        let moved = before.is_none_or(|before| screen_digest(&screen) != before);
        if in_turn && moved {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn screen_digest(screen: &str) -> u64 {
    toron_plane::backend::rex_cli::rex_map::screen_prompt_digest(screen)
}

/// The typed failure a pane's silence earns: the line went out, no turn
/// followed, so the wake did not land.
fn wake_not_landed(target: &str, bound: Duration) -> String {
    format!(
        "wake line did not land in {target}: no turn within {}s of the write",
        bound.as_secs()
    )
}

/// The wake line a busy pane refused: the same text the ring would have typed.
pub(crate) fn wake_is_busy(err: &str) -> bool {
    err.contains("not at a prompt") || err.to_ascii_lowercase().contains("busy")
}

/// Both refusals mean the same thing to the caller — the mail is delivered and
/// the ring is not on the pane yet — so both defer to the dispatcher.
pub(crate) fn wake_is_deferrable(err: &str) -> bool {
    wake_is_busy(err) || err.contains("wake line did not land") || wake_is_redundant(err)
}

/// bd-8wdgn: the box already holds our own queued wake line. The mail is
/// stored and only the ring is redundant, so the prompt succeeds with
/// queued:true rather than looking like a failed send.
fn wake_is_redundant(err: &str) -> bool {
    err.contains("a wake line is already queued")
}

/// A busy or unconsumed pane's mail is already delivered; the ring is deferred
/// to the project's live dispatcher, which folds the deferral as a refusal
/// (working pin queues it, the next at-prompt status rings it). The handoff is
/// project-keyed on disk, so the dispatcher's workspace id never has to match.
/// Returns the operator line and, when the ring was deferred, why.
fn finish_prompt(
    delivered: Result<String, String>,
    defer: impl FnOnce() -> Result<()>,
) -> Result<(String, Option<String>)> {
    match delivered {
        Ok(typed) => Ok((typed, None)),
        Err(err) if wake_is_deferrable(&err) => {
            defer()?;
            Ok((QUEUED_WAKE.to_string(), Some(err)))
        }
        Err(err) => Err(anyhow!("{err}")),
    }
}

const QUEUED_WAKE: &str = "mail delivered; the doorbell rings at the next prompt";

/// Record the refused wake for the project's dispatcher, in the one line shape
/// the fold treats as a mail wake.
fn defer_busy_wake(project: &str, pin: &str) -> Result<()> {
    crate::doorbell::append_deferred_wake_with_wm(
        &crate::config::state_dir(),
        project,
        pin,
        &crate::doorbell::deferred_wake_line(),
        Some(crate::doorbell::now_micros()),
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
    let session = harness_session_for_target(project, &a.pane_id, a.name.as_deref());
    let route = crate::harness_caps::wake_route_for(
        &kind,
        session.as_deref(),
        crate::harness_caps::installed_from_env(),
    );
    match ring_prompt_wake(
        &crate::pane_write::HcPanes(&c),
        &crate::pane_read::HcReader(&c),
        project,
        &kind,
        &target,
        wm,
        urgent,
        route,
        &|pane| probe_hc(&c, pane),
        crate::pane_write::WAKE_LANDING_BOUND,
    ) {
        Ok(crate::inbox_ports::RingDone::Typed) => Ok(prompt_wake_line(wm, urgent)),
        Ok(crate::inbox_ports::RingDone::Deferred(why)) => {
            bail!("wake line deferred for {pin} ({target}): {why}")
        }
        Ok(crate::inbox_ports::RingDone::NoDoor(no_door)) => {
            bail!(
                "wake line could not be delivered to {pin} ({target}): {}",
                crate::harness_caps::no_door_line(&no_door)
            )
        }
        Err(e) => bail!("wake line delivery to {pin} ({target}) failed: {e}"),
    }
}

/// Did the typed line become a turn? hc's own `agent_wait` ignores its `until`
/// set (it waits for terminal idle), so read the status and the screen directly:
/// working or blocked means the agent picked the line up.
fn probe_hc(c: &Client, target: &str) -> Result<(String, bool)> {
    let status = c
        .agent_get(target)
        .map_err(|e| anyhow!("hc agent.get {target}: {e}"))?
        .agent_status;
    let in_turn = matches!(
        status,
        crate::hc::AgentStatus::Working | crate::hc::AgentStatus::Blocked
    );
    let screen = c
        .agent_read(target, "screen", Some(40))
        .map_err(|e| anyhow!("hc pane read {target}: {e}"))?;
    Ok((screen, in_turn))
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
    let session = harness_session_for_target(project, &a.pane_id, a.name.as_deref());
    let route = crate::harness_caps::wake_route_for(
        &kind,
        session.as_deref(),
        crate::harness_caps::installed_from_env(),
    );
    match ring_prompt_wake(
        &crate::pane_write::RexPanes(&*backend),
        &crate::pane_read::PlaneReader(&*backend),
        project,
        &kind,
        &target,
        wm,
        urgent,
        route,
        &|pane| probe_rex(&*backend, pane),
        crate::pane_write::WAKE_LANDING_BOUND,
    ) {
        Ok(crate::inbox_ports::RingDone::Typed) => Ok(prompt_wake_line(wm, urgent)),
        Ok(crate::inbox_ports::RingDone::Deferred(why)) => {
            bail!("wake line deferred for {pin} ({target}): {why}")
        }
        Ok(crate::inbox_ports::RingDone::NoDoor(no_door)) => {
            bail!(
                "wake line could not be delivered to {pin} ({target}): {}",
                crate::harness_caps::no_door_line(&no_door)
            )
        }
        Err(e) => bail!("wake line delivery to {pin} ({target}) failed: {e}"),
    }
}

/// rex answers both halves in one round trip: the folded status (`agent_wait`
/// with an empty `until` set is a read) and the block's current screen for the
/// digest.
#[cfg(feature = "backend-superlogical")]
fn probe_rex(
    backend: &dyn crate::backend::MultiplexerBackend,
    target: &str,
) -> Result<(String, bool)> {
    use crate::hc::AgentStatus;
    let status = backend
        .agent_wait(target, &[], Some(0))
        .map_err(|e| anyhow!("rex agent.get {target}: {e}"))?
        .agent_status;
    let in_turn = matches!(status, AgentStatus::Working | AgentStatus::Blocked);
    let read = backend
        .agent_read(target, "screen", 40)
        .map_err(|e| anyhow!("rex pane read {target}: {e}"))?;
    let screen = read
        .get("text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok((screen, in_turn))
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

/// Where the brief book lives: per project, beside the other state.
fn briefs_path(project: &str) -> std::path::PathBuf {
    crate::config::state_dir()
        .join("briefs")
        .join(format!("{}.json", crate::state::state_key(project)))
}

fn load_briefs(project: &str) -> crate::crew_briefs::BriefBook {
    std::fs::read_to_string(briefs_path(project))
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

fn save_briefs(project: &str, book: &crate::crew_briefs::BriefBook) -> Result<()> {
    let path = briefs_path(project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(book)?;
    crate::project_state::write_atomic(&path, body.as_bytes())
        .map_err(|e| anyhow!("save briefs: {e}"))
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Record a dispatch as a brief and append the id the crew replies with.
fn open_brief(project: &str, pin: &str, subject: &str) -> Result<String> {
    let mut book = load_briefs(project);
    let id = format!("{}-{}", pin, now_us());
    let brief = crate::crew_briefs::Brief {
        id: id.clone(),
        pin: pin.to_string(),
        subject: subject.to_string(),
        opened_ts_us: now_us(),
        closed_ts_us: None,
        receipt: None,
        receipt_typed: false,
    };
    book.reduce(crate::crew_briefs::Msg::Opened(brief));
    save_briefs(project, &book)?;
    Ok(id)
}

/// The crew's structured reply (bd-0uzrq.4): close the brief, then wake the
/// captain ONCE. A busy captain defers to the project's dispatcher — the same
/// contract crews get — and a replayed reply emits nothing.
struct LiveReceiptChecks {
    project_dir: std::path::PathBuf,
}

impl crate::crew_receipt::ReceiptChecks for LiveReceiptChecks {
    fn bead_exists(&self, id: &str) -> bool {
        crate::br::Bead::exists(Some(&self.project_dir), id)
    }
    fn commit_exists(&self, repo: &std::path::Path, sha: &str) -> bool {
        crate::cmd::ship::sha_exists_in(repo, sha)
    }
    fn is_ancestor(&self, repo: &std::path::Path, ancestor: &str, descendant: &str) -> bool {
        crate::cmd::ship::is_ancestor_in(repo, ancestor, descendant)
    }
}

fn resolve_receipt(
    cfg: &Config,
    project: &str,
    text: Option<&str>,
    json: Option<&str>,
) -> Result<(String, bool)> {
    match (text, json) {
        (Some(t), None) => Ok((t.to_string(), false)),
        (None, None) => bail!("crew done needs --receipt or --receipt-json"),
        (Some(_), Some(_)) => bail!("--receipt and --receipt-json are mutually exclusive"),
        (None, Some(source)) => {
            use std::io::Read;
            let raw = if source == "-" {
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .map_err(|e| anyhow!("read --receipt-json -: {e}"))?;
                s
            } else {
                std::fs::read_to_string(source)
                    .map_err(|e| anyhow!("read --receipt-json {source}: {e}"))?
            };
            let checks = LiveReceiptChecks {
                project_dir: crate::project_dir(cfg, project),
            };
            let r = crate::crew_receipt::validate(&raw, &cfg.workspace_root(), project, &checks)
                .map_err(|e| anyhow!("invalid typed receipt ({e})"))?;
            Ok((crate::crew_receipt::canonical(&r), true))
        }
    }
}

/// bd-r7852: persist the typed receipt's RED witnesses so `ship land` can
/// count them on a no-mutation proof receipt. A malformed store is replaced by
/// a fresh one rather than trusted.
fn record_mutation_witnesses(rows: &[crate::mutation_witnesses::Witness]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let path = crate::config::state_dir().join(crate::mutation_witnesses::STORE_FILE);
    let mut store = crate::mutation_witnesses::Store::load(&path);
    for witness in rows {
        store.insert(witness.clone());
    }
    store.save(&path)?;
    Ok(())
}

fn cmd_crew_done(
    cfg: &Config,
    project: &str,
    brief: &str,
    receipt_text: Option<&str>,
    receipt_json: Option<&str>,
    r#as: Option<&str>,
) -> Result<()> {
    let (receipt, receipt_typed) = resolve_receipt(cfg, project, receipt_text, receipt_json)?;
    let mut book = load_briefs(project);
    if book.get(brief).is_none() {
        bail!("no such brief: {brief} — see `flywheel crew inbox --open --project {project}`");
    }
    if receipt_typed {
        let parsed = crate::crew_receipt::parse(&receipt)?;
        if parsed.brief_id != brief {
            bail!(
                "receipt.brief_id is {:?}, expected {brief}",
                parsed.brief_id
            );
        }
        let harness_witnesses = match (&parsed.witness_file, &parsed.witnesses) {
            (Some(path), _) => crate::crew_receipt::read_witness_file(std::path::Path::new(path))
                .map_err(|e| anyhow!("invalid witness file ({e})"))?,
            (None, Some(_)) => bail!(
                "hand-written witnesses are refused; set witness_file to a file written by verify-mutations.sh"
            ),
            (None, None) => Vec::new(),
        };
        record_mutation_witnesses(&harness_witnesses)?;
    }
    let effects = book.reduce(crate::crew_briefs::Msg::Done {
        id: brief.to_string(),
        receipt: receipt.clone(),
        now_us: now_us(),
    });
    if let Some(row) = book.briefs.iter_mut().find(|b| b.id == brief) {
        row.receipt_typed = receipt_typed;
    }
    save_briefs(project, &book)?;
    // bd-9nck5: the mail fallback's sender is an IDENTITY, not a record id.
    // The one ladder is --as → AGENT_NAME; when neither is bound, the brief's
    // own pin is the last rung. A brief id has no mail key, so using it made
    // the fallback fail after the pane wake had already failed.
    let reporter = crate::authority::select_agent_identity(r#as, &|k| std::env::var(k).ok());
    let mut wake = "replay: the brief was already closed; nothing rang".to_string();
    for effect in &effects {
        let crate::crew_briefs::Effect::WakeCaptain { id, pin } = effect;
        let reporter = reporter.clone().unwrap_or_else(|| pin.clone());
        let key = crate::project_dir_str(cfg, project)?;
        let Some(captain) = crate::state::captain_pin(project, &key) else {
            wake = "closed; no captain is bound for this project, so nothing rang".to_string();
            continue;
        };
        let wm = recipient_wm(project, &captain);
        let (line, deferred) = done_reply(
            || {
                let delivered = deliver_prompt_wake(cfg, project, &captain, false, wm)
                    .map_err(|e| format!("{e:#}"));
                finish_prompt(delivered, || defer_busy_wake(project, &captain))
            },
            || done_reply_via_mail(cfg, project, &captain, brief, &receipt, &reporter),
        )?;
        wake = match deferred {
            Some(reason) => format!("captain busy ({reason}); {line}"),
            None => line,
        };
        crate::log(
            project,
            EventKind::Send,
            json!({
                "op": "crew-done",
                "brief": id,
                "pin": pin,
                "from": reporter,
                "receipt": receipt,
                "wake": wake,
            }),
        );
    }
    println!("brief {brief} closed; {wake}");
    // bd-t5f0v: advisory only, and worded conditionally — a clean skip is a
    // valid answer and never blocks the close.
    let bead = book
        .get(brief)
        .and_then(|b| b.subject.split_whitespace().find(|w| w.starts_with("bd-")))
        .unwrap_or(brief);
    println!("{}", crate::cmd::ship::lesson_advisory(bead));
    if receipt_typed {
        println!(
            "next: flywheel crew land {brief} --proof-receipt <proof.json> --project {project}"
        );
    }
    Ok(())
}

/// bd-bi0mv: land every bead named in a brief's typed receipt by delegating to
/// `ship land` — there is no second close path.
fn cmd_crew_land(
    cfg: &Config,
    brief_id: &str,
    proof_receipt: &std::path::Path,
    project: Option<&str>,
    as_pin: Option<&str>,
) -> Result<()> {
    let project = project.unwrap_or("flywheel").to_string();
    let book = load_briefs(&project);
    let row = book
        .get(brief_id)
        .ok_or_else(|| anyhow!("no such brief: {brief_id}"))?;
    if !row.receipt_typed {
        bail!("brief {brief_id} has no typed receipt — close it with --receipt-json first");
    }
    let raw = row
        .receipt
        .as_deref()
        .ok_or_else(|| anyhow!("brief {brief_id} has an empty receipt"))?;
    let r = crate::crew_receipt::parse(raw)?;
    crate::crew_receipt::validate_structure(&r)?;
    let workspace_root = cfg.workspace_root();
    land_beads(&r, &workspace_root, &project, |bead, sha, repo| {
        crate::cmd::ship::ship_land(crate::cmd::ship::LandArgs {
            cfg,
            bead,
            sha,
            receipt: proof_receipt,
            project: Some(&project),
            as_pin,
            message: None,
            dir: None,
            repo: Some(repo),
            json: false,
        })
    })?;
    for b in &r.beads {
        println!("next: flywheel ship check {}", b.id);
    }
    Ok(())
}

/// Pure land plan: one call per bead with the SAME resolver validation used,
/// injected so delegation to `ship land` is testable without a live repo.
fn land_beads<F>(
    r: &crate::crew_receipt::CrewReceipt,
    workspace_root: &std::path::Path,
    project: &str,
    mut land: F,
) -> Result<()>
where
    F: FnMut(&str, &str, &std::path::Path) -> Result<()>,
{
    for b in &r.beads {
        let repo = crate::crew_receipt::resolve_repo(workspace_root, project, &b.repo)
            .map_err(|e| anyhow!("invalid typed receipt ({e})"))?;
        land(&b.id, &b.sha, &repo)?;
    }
    Ok(())
}

/// bd-ezlme: how a done reply reaches the captain, as one decision.
///
/// The pane wake is the fast path, but a captain can be a MAIL-ONLY identity —
/// a pin with no agent row (the live case: `crew done` closed the brief and
/// then failed with "no agents match --to SlateHarbor"). A done reply that
/// never rings is the lost hand-off this whole verb exists to prevent, so a
/// failed pane wake falls back to the plane-P mail path instead of inventing a
/// pane-less registry row. Both failing is a typed error, never a silent close.
fn done_reply(
    pane: impl FnOnce() -> Result<(String, Option<String>)>,
    mail: impl FnOnce() -> Result<String>,
) -> Result<(String, Option<String>)> {
    match pane() {
        Ok((line, deferred)) => Ok((line, deferred)),
        Err(pane_error) => match mail() {
            Ok(line) => Ok((line, None)),
            Err(mail_error) => bail!(
                "the brief is closed but the captain heard nothing: pane wake failed ({pane_error}); \
                 mail fallback failed ({mail_error})"
            ),
        },
    }
}

/// The plane-P fallback: mail the done reply to the captain pin.
fn done_reply_via_mail(
    cfg: &Config,
    project: &str,
    captain: &str,
    brief: &str,
    receipt: &str,
    reporter: &str,
) -> Result<String> {
    let mail = crate::cmd::mail::mail_or_die(cfg)?;
    let key = crate::project_dir_str(cfg, project)?;
    let sent = mail.with_name(reporter.to_string()).send(
        &key,
        &[captain.to_string()],
        &format!("[crew] {brief} done"),
        receipt,
        "normal",
        false,
        None,
    )?;
    Ok(format!(
        "reply sent by mail: no live pane route to {captain} ({sent})"
    ))
}

/// `crew inbox --open`: a silent crew is a row with an age.
fn cmd_crew_inbox(
    cfg: &Config,
    project: &str,
    open: bool,
    pin: Option<&str>,
    json: bool,
) -> Result<()> {
    let _ = cfg;
    let book = load_briefs(project);
    let now = now_us();
    let rows = book.open_for(pin, now);
    let receipt_json = |b: &crate::crew_briefs::Brief| -> Value {
        match (b.receipt_typed, b.receipt.as_deref()) {
            (true, Some(raw)) => crate::crew_receipt::parse(raw)
                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null))
                .unwrap_or(Value::Null),
            (true, None) => Value::Null,
            (false, raw) => json!(raw),
        }
    };
    let listed: Vec<Value> = if open {
        rows.iter()
            .map(|r| {
                let b = book.get(&r.id);
                json!({
                    "id": r.id,
                    "pin": r.pin,
                    "subject": r.subject,
                    "age_secs": r.age_secs,
                    "receipt_typed": b.is_some_and(|b| b.receipt_typed),
                    "receipt": b.map(receipt_json).unwrap_or(Value::Null),
                })
            })
            .collect()
    } else {
        book.briefs
            .iter()
            .map(|b| {
                json!({
                    "id": b.id,
                    "pin": b.pin,
                    "subject": b.subject,
                    "age_secs": now.saturating_sub(b.opened_ts_us) / 1_000_000,
                    "closed": b.closed_ts_us.is_some(),
                    "receipt_typed": b.receipt_typed,
                    "receipt": receipt_json(b),
                })
            })
            .collect()
    };
    if json {
        crate::output::print_json(&json!({
            "schema": "flywheel-crew-inbox.v1",
            "project": project,
            "open": rows.len(),
            "briefs": listed,
        }));
        return Ok(());
    }
    if listed.is_empty() {
        println!("no briefs for {project}");
        return Ok(());
    }
    for row in &listed {
        println!("{}", serde_json::to_string(row)?);
    }
    Ok(())
}

pub(crate) fn cmd_crew_prompt(
    cfg: &Config,
    project: &str,
    pin: &str,
    prompt: &str,
    r#as: Option<&str>,
    urgent: bool,
    bead: Option<&str>,
) -> Result<()> {
    let sender = prompt_sender(cfg, r#as);
    // bd-0uzrq.4: the dispatch IS a brief. Its id rides the body so the crew
    // replies with the one verb that closes it, and `crew inbox --open` can
    // show what nobody answered.
    let brief = open_brief(project, pin, prompt.lines().next().unwrap_or(prompt))?;
    let project_dir = crate::project_dir(cfg, project);
    let traps = if project_dir.is_dir() {
        crate::cmd::memory::briefing_traps(prompt, Some(project), bead, Some(&project_dir))
    } else {
        Vec::new()
    };
    let traps_block = crate::cmd::memory::render_traps_section(&traps);
    let traps_suffix = if traps_block.is_empty() {
        String::new()
    } else {
        format!("\n\n{}", traps_block.trim_end())
    };
    let prompt = format!(
        "{prompt}{traps_suffix}\n\n---\nBrief id: {brief}\nReply when done: flywheel crew done {brief} --receipt \"<commit sha + proof receipt + test tail>\" --project {project}"
    );
    let dispatch = PromptDispatch {
        pin: pin.to_string(),
        project: project.to_string(),
        sender,
        prompt,
        urgent,
        bead: bead.map(str::to_string),
    };

    toron_mail_send(&dispatch)?;
    let wm = recipient_wm(project, pin);
    let delivered =
        deliver_prompt_wake(cfg, project, pin, urgent, wm).map_err(|e| format!("{e:#}"));
    let (wake, deferred_reason) = finish_prompt(delivered, || defer_busy_wake(project, pin))?;
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
            "wake_deferred_reason": deferred_reason,
            "wm": wm,
            "bead": bead,
        }),
    );
    print_json(&dispatch.confirmation(wm));
    if queued {
        println!("{QUEUED_WAKE}");
    }
    Ok(())
}

#[cfg(test)]
mod bd_bi0mv_land_tests {
    use super::land_beads;

    fn raw() -> String {
        format!(
            r#"{{"schema":"flywheel-crew-receipt.v1","brief_id":"b","beads":[
                    {{"id":"bd-a","repo":".","sha":"{}"}},
                    {{"id":"bd-b","repo":"../toron","sha":"{}"}}],
                    "fence":{{"sha":"{}","verdict":"green"}},
                    "tests":{{"cmd":"cargo test","tail":"ok"}}}}"#,
            "a".repeat(40),
            "b".repeat(40),
            "c".repeat(40),
        )
    }

    #[test]
    fn land_targets_the_resolved_repo_not_the_callers_cwd() {
        let r = crate::crew_receipt::parse(&raw()).expect("parse");
        let root = std::path::Path::new("/ws");
        let mut calls = Vec::new();
        land_beads(&r, root, "flywheel", |bead, sha, repo| {
            calls.push((bead.to_string(), sha.to_string(), repo.to_path_buf()));
            Ok(())
        })
        .expect("land");
        assert_eq!(
            calls,
            vec![
                (
                    "bd-a".to_string(),
                    "a".repeat(40),
                    std::path::PathBuf::from("/ws/flywheel")
                ),
                (
                    "bd-b".to_string(),
                    "b".repeat(40),
                    std::path::PathBuf::from("/ws/toron")
                ),
            ]
        );
    }

    #[test]
    fn a_failing_land_stops_the_whole_receipt() {
        let r = crate::crew_receipt::parse(&raw()).expect("parse");
        let mut seen = Vec::new();
        let err = land_beads(
            &r,
            std::path::Path::new("/ws"),
            "flywheel",
            |bead, _sha, _repo| {
                seen.push(bead.to_string());
                if bead == "bd-a" {
                    anyhow::bail!("gate failed")
                }
                Ok(())
            },
        )
        .expect_err("first land fails");
        assert!(err.to_string().contains("gate failed"));
        assert_eq!(seen, vec!["bd-a"], "a failed land must not land the rest");
    }
}

#[cfg(test)]
mod bd_ezlme_tests {
    use super::done_reply;

    /// bd-ezlme end to end on the mail side: the fallback actually lands a mail
    /// in the captain's inbox (in-process mailbox, no pane registry row needed).
    #[test]
    fn the_mail_fallback_lands_in_the_captains_inbox() {
        let _g = crate::config::lock_env();
        let cfg = crate::config::Config::default();
        let mail = crate::mail::Mail::from_config(None)
            .expect("mail facade")
            .expect("in-process mailbox");
        let project = format!("crew-done-{}", std::process::id());
        mail.ensure_project(&project).expect("project");
        for name in ["BlueLake", "SlateHarbor"] {
            mail.register_fixture(&project, name, "test", "test", "crew done fallback")
                .expect("register");
        }
        let line = super::done_reply_via_mail(
            &cfg,
            &project,
            "SlateHarbor",
            "brief-ezlme",
            "receipt-ezlme",
            "BlueLake",
        )
        .expect("the fallback mails the captain");
        assert!(line.contains("reply sent by mail"), "{line}");
        let inbox = mail
            .inbox(&project, "SlateHarbor", false, 10)
            .expect("captain inbox");
        let text = format!("{inbox}");
        assert!(
            text.contains("brief-ezlme") && text.contains("receipt-ezlme"),
            "the done reply reaches the captain by mail: {text}"
        );
    }

    /// bd-ezlme: the decision the live failure needed. A pane wake that cannot
    /// resolve the captain (no agent row) must fall back to the plane-P mail,
    /// not close the brief silently; both failing is a typed error naming both.
    #[test]
    fn a_failed_pane_wake_falls_back_to_the_mail_path() {
        let (line, deferred) = done_reply(
            || {
                Err(anyhow::anyhow!(
                    "no agents match --to SlateHarbor (available: shell)"
                ))
            },
            || Ok("reply sent by mail".to_string()),
        )
        .expect("the fallback is not a failure");
        assert!(
            line.contains("reply sent by mail"),
            "the reply says which path delivered it: {line}"
        );
        assert!(deferred.is_none(), "a mailed reply is not a deferred wake");

        // A pane wake that works is still preferred: no mail is sent.
        let (line, _) = done_reply(
            || Ok(("captain rung (prompt sent)".to_string(), None)),
            || panic!("the mail path must not run when the pane wake lands"),
        )
        .expect("the pane path");
        assert!(line.contains("rung"), "{line}");

        // Both dead: the operator hears that the captain heard nothing.
        let err = done_reply(
            || Err(anyhow::anyhow!("no agents match")),
            || Err(anyhow::anyhow!("mail not configured")),
        )
        .expect_err("both paths dead is a failure");
        let text = err.to_string();
        assert!(text.contains("heard nothing"), "{text}");
        assert!(text.contains("no agents match"), "{text}");
        assert!(text.contains("mail not configured"), "{text}");
    }
}

#[cfg(test)]
mod bd_9nck5_tests {
    /// Drive the VERB, not done_reply_via_mail: the old unit test passed the
    /// reporter in by hand and so could never see that the verb derived it
    /// from the brief id. Here no --as is given, and the caller's identity is
    /// varied only through the AGENT_NAME ladder rung.
    fn fallback_sender(brief_pin: &str, agent: Option<&str>) -> String {
        let cfg = crate::config::Config::default();
        let mail = crate::mail::Mail::from_config(None)
            .expect("mail facade")
            .expect("in-process mailbox");
        let project = format!("crew-sender-{brief_pin}");
        mail.ensure_project(&project).expect("project");
        let mut names = vec![brief_pin, "SlateHarbor", &mail.name];
        if let Some(a) = agent {
            names.push(a);
        }
        for name in names {
            mail.register_fixture(&project, name, "test", "test", "bd-9nck5")
                .expect("register");
        }
        crate::state::save_captain_alias(&project, "SlateHarbor").expect("captain alias");
        let brief = super::open_brief(&project, brief_pin, "bd-9nck5 witness").expect("brief");

        match agent {
            Some(a) => unsafe { std::env::set_var("AGENT_NAME", a) },
            None => unsafe { std::env::remove_var("AGENT_NAME") },
        }
        super::cmd_crew_done(&cfg, &project, &brief, Some("receipt-9nck5"), None, None)
            .expect("the done verb must mail the captain when no pane route exists");

        let inbox = mail
            .inbox(&project, "SlateHarbor", false, 10)
            .expect("inbox");
        let rows = crate::mail::inbox_json_rows(&inbox);
        let subject = format!("[crew] {brief} done");
        rows.as_array()
            .and_then(|a| a.iter().find(|r| r["subject"].as_str() == Some(&subject)))
            .and_then(|r| r["sender"].as_str())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn the_done_fallback_sender_uses_the_identity_ladder() {
        let _g = crate::config::lock_env();
        let prev = std::env::var("AGENT_NAME").ok();

        // No --as and no AGENT_NAME: the brief's own pin is the sender. The
        // brief ID is a record id, not an identity, and has no mail key.
        assert_eq!(
            fallback_sender("BriefPinOne", None),
            "BriefPinOne",
            "the brief's pin must be the last rung, never the brief id"
        );
        // AGENT_NAME outranks the brief's pin.
        assert_eq!(
            fallback_sender("BriefPinTwo", Some("AmbientPin")),
            "AmbientPin",
            "AGENT_NAME must outrank the brief's pin"
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("AGENT_NAME", v) },
            None => unsafe { std::env::remove_var("AGENT_NAME") },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn crew_done_witnesses_are_written_to_the_store() {
        let _guard = crate::config::lock_env();
        let mut receipt = crate::crew_receipt::parse(&format!(
            r#"{{"schema":"flywheel-crew-receipt.v1","brief_id":"b","beads":[{{"id":"bd-x","repo":".","sha":"{}"}}],"fence":{{"sha":"{}","verdict":"green"}},"tests":{{"cmd":"c","tail":"t"}}}}"#,
            "a".repeat(40),
            "a".repeat(40)
        ))
        .expect("parse receipt");
        receipt.witnesses = Some(vec![crate::mutation_witnesses::Witness {
            spec: "scripts/mutations/demo.spec.sh".to_string(),
            sha: "a".repeat(40),
            verdict: "RED".to_string(),
            key: "k1".to_string(),
        }]);
        let rows = receipt.witnesses.clone().unwrap_or_default();
        record_mutation_witnesses(&rows).expect("record witnesses");
        let path = crate::config::state_dir().join(crate::mutation_witnesses::STORE_FILE);
        let store = crate::mutation_witnesses::Store::load(&path);
        let rows = store.for_key("k1");
        assert_eq!(rows.len(), 1, "the witness is persisted");
        assert_eq!(rows[0].spec, "scripts/mutations/demo.spec.sh");
    }

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
                bead,
            } => {
                assert!(bead.is_none(), "no --bead in this argv");
                assert_eq!(pin, "BlackGorge");
                assert_eq!(prompt, "implement bead");
                assert_eq!(project, "flywheel");
                assert_eq!(r#as.as_deref(), Some("AmberHarbor"));
                assert!(urgent);
            }
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    /// bd-0uzrq.11: `--bead` rides `corr`, and `corr` is what the doorbell
    /// supersedes on when a row has no thread. A captain's correction about the
    /// same bead therefore replaces the queued brief instead of ringing beside
    /// it.
    #[test]
    fn a_bead_prompt_carries_corr_and_the_parsed_flag_reaches_it() {
        let cli = Cli::try_parse_from([
            "flywheel",
            "prompt",
            "BlackGorge",
            "correct the brief",
            "--project",
            "flywheel",
            "--bead",
            "bd-0uzrq.11",
        ])
        .expect("parse crew prompt --bead");
        let bead = match cli.cmd {
            CrewCmd::Prompt { bead, .. } => bead,
            other => panic!("expected Prompt, got {other:?}"),
        };
        assert_eq!(bead.as_deref(), Some("bd-0uzrq.11"));

        let dispatch = PromptDispatch {
            pin: "BlackGorge".to_string(),
            project: "flywheel".to_string(),
            sender: "AmberHarbor".to_string(),
            prompt: "correct the brief".to_string(),
            urgent: false,
            bead: Some("bd-0uzrq.11".to_string()),
        };
        let args = dispatch.mail_args();
        let corr_at = args.iter().position(|a| a == "--corr");
        assert_eq!(
            corr_at.map(|at| args[at + 1].as_str()),
            Some("bd-0uzrq.11"),
            "the bead rides --corr: {args:?}"
        );
    }

    #[test]
    fn prompt_dispatch_structure_builds_mail_and_wake() {
        let dispatch = PromptDispatch {
            pin: "BlackGorge".to_string(),
            project: "flywheel".to_string(),
            sender: "AmberHarbor".to_string(),
            prompt: "do the thing".to_string(),
            urgent: true,
            bead: None,
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

    /// A pane that really takes the line: the first read is the pre-write
    /// capture, every later read is the turn the wake started.
    fn landed_probe() -> impl Fn(&str) -> Result<(String, bool)> {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        move |_| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(if n == 0 {
                (
                    "at the prompt: …/flywheel:adr-0021-stage-contracts".to_string(),
                    false,
                )
            } else {
                ("es...upt   1 shell  770.3K (77%)".to_string(), true)
            })
        }
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
            &QuietPane,
            "crew-probe",
            "opencode",
            "block:pane-1",
            42,
            true,
            WakeRoute::Inject { interrupt: true },
            &landed_probe(),
            Duration::ZERO,
        )
        .expect("an urgent crew prompt must ring");
        assert_eq!(got, crate::inbox_ports::RingDone::Typed);
        let typed = prompt_wake_line(42, true);
        assert!(typed.contains("Urgent mail"), "urgent form: {typed}");
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
            &QuietPane,
            "crew-probe",
            "opencode",
            "block:pane-1",
            42,
            false,
            WakeRoute::Inject { interrupt: true },
            &landed_probe(),
            Duration::ZERO,
        )
        .expect("a normal crew prompt must ring");
        assert_eq!(got, crate::inbox_ports::RingDone::Typed);
        let typed = prompt_wake_line(42, false);
        assert!(typed.contains("toron next"), "wake is compliant: {typed}");
        assert_eq!(writes(&rec), vec![format!("prompt block:pane-1 {typed}")]);
    }

    /// A pane whose box holds the operator's half-typed line.
    struct TypedPane;

    impl crate::pane_read::PaneReader for TypedPane {
        fn tail(&self, _pane_id: &str, _lines: u32) -> Result<String, String> {
            std::fs::read_to_string(format!(
                "{}/tests/fixtures/pane-screens/opencode/partial-input.txt",
                env!("CARGO_MANIFEST_DIR")
            ))
            .map_err(|e| e.to_string())
        }
    }

    /// bd-0uzrq.3: a captain-sent prompt is still a prompt into a shared pane.
    /// `--urgent` does not override the operator's half-typed line.
    #[test]
    fn pane_screen_crew_prompt_defers_on_a_typed_box() {
        use crate::harness_caps::WakeRoute;
        let rec = RecTyper::default();
        let err = ring_prompt_wake(
            &rec,
            &TypedPane,
            "crew-probe",
            "opencode",
            "block:pane-typed",
            42,
            true,
            WakeRoute::Inject { interrupt: true },
            &landed_probe(),
            Duration::ZERO,
        )
        .expect_err("operator text must defer, urgent or not");
        assert!(
            err.starts_with("deferred: operator is typing in "),
            "the refusal names the pin: {err}"
        );
        assert!(
            writes(&rec).is_empty(),
            "a deferred prompt types nothing, not even the interrupt"
        );
    }

    /// A pane with nothing in its input line (no capture for this test).
    struct QuietPane;

    impl crate::pane_read::PaneReader for QuietPane {
        fn tail(&self, _pane_id: &str, _lines: u32) -> Result<String, String> {
            Ok(String::new())
        }
    }

    /// bd-0uzrq.16, the live failure in a unit shape: a reset opencode sitting
    /// on its welcome screen swallowed the pasted wake on 2026-11-15 13:47Z —
    /// the write returned Ok, the input box stayed empty, no turn started, and
    /// the captain's brief never rang. The writer must see the silence.
    #[test]
    fn a_welcome_screen_that_swallows_the_paste_is_a_typed_failure() {
        use crate::harness_caps::WakeRoute;
        let rec = RecTyper::default();
        let err = ring_prompt_wake(
            &rec,
            &QuietPane,
            "crew-probe",
            "opencode",
            "block:01a1d6a1-fresh",
            42,
            false,
            WakeRoute::Inject { interrupt: false },
            &|_| Ok(("welcome: Ask anything".to_string(), false)),
            Duration::ZERO,
        )
        .expect_err("a pane that never turns must not report a landed wake");
        assert!(err.contains("wake line did not land"), "got: {err}");
        assert_eq!(
            writes(&rec),
            vec![format!(
                "prompt block:01a1d6a1-fresh {}",
                prompt_wake_line(42, false)
            )],
            "the line is typed either way — the failure is that nothing consumed it"
        );
    }

    /// bd-8wdgn: when the box already holds our own queued wake line, the mail
    /// is stored and only the ring is redundant. The prompt SUCCEEDS carrying
    /// queued:true (exit 0), so a scripted send is not reported as a failure.
    #[test]
    fn an_already_queued_wake_line_succeeds_with_queued_true() {
        let _g = crate::config::test_state_home();
        let mut deferred = false;
        let out = finish_prompt(
            Err("deferred: a wake line is already queued for BlueRidge; nothing was typed, and the mail stays queued".to_string()),
            || {
                deferred = true;
                Ok(())
            },
        )
        .expect("a redundant wake is not a failed send");
        assert_eq!(out.0, QUEUED_WAKE, "the operator line says the mail landed");
        assert!(
            out.1
                .as_deref()
                .is_some_and(|why| why.contains("already queued")),
            "and the journal names the wake line: {out:?}"
        );
        assert!(deferred, "the deferral is recorded for the dispatcher");
    }

    /// And an unconsumed wake must not vanish: it goes back to the queue as a
    /// deferral, with the reason on the journal.
    #[test]
    fn an_unconsumed_wake_defers_to_the_dispatcher_with_its_reason() {
        let _g = crate::config::test_state_home();
        let project = format!("landing-defer-{}", std::process::id());
        let out = finish_prompt(
            Err(wake_not_landed("block:p1", Duration::from_secs(8))),
            || defer_busy_wake(&project, "BlueRidge"),
        )
        .expect("an unconsumed wake must not fail the whole prompt");
        assert_eq!(out.0, QUEUED_WAKE);
        assert!(
            out.1
                .as_deref()
                .is_some_and(|why| why.contains("did not land")),
            "the journal names the typed failure: {out:?}"
        );
        let hosted: std::collections::BTreeSet<String> =
            ["BlueRidge".to_string()].into_iter().collect();
        assert_eq!(
            crate::doorbell::take_deferred_wakes(&crate::config::state_dir(), &project, &hosted),
            vec![(
                "BlueRidge".to_string(),
                "[wake] New mail. Read it with: toron next".to_string()
            )],
            "the unconsumed wake is on the dispatcher's queue, not lost"
        );
    }

    /// The .10 stale record is the trap this guard exists for: a pane can report
    /// `working` while its screen never moved, so status alone would confirm a
    /// wake that landed nowhere.
    #[test]
    fn a_stale_working_record_cannot_confirm_a_silent_wake() {
        let screen = "…/work…/flywheel:adr-0021-stage-contracts";
        let before = screen_digest(screen);
        let started = wait_turn("block:p1", Duration::ZERO, Some(before), &|_| {
            Ok((screen.to_string(), true))
        })
        .expect("probe");
        assert!(!started, "an identical screen is not proof the line landed");
    }

    /// The other half: the box kept the text (so the screen moved) but no turn
    /// started — the Enter was lost, not the paste.
    #[test]
    fn a_moved_screen_without_a_turn_is_not_a_landing() {
        let started = wait_turn(
            "block:p1",
            Duration::ZERO,
            Some(screen_digest("before")),
            &|_| Ok(("⟦toron⟧ typed but not submitted".to_string(), false)),
        )
        .expect("probe");
        assert!(!started, "text sitting in the box is not a turn");
    }

    /// And the shape that does land both ways confirms on the first poll.
    #[test]
    fn a_turn_on_a_moved_screen_confirms_the_wake() {
        let started = wait_turn(
            "block:p1",
            Duration::ZERO,
            Some(screen_digest("before")),
            &|_| Ok(("es...upt 1 shell 770.3K (77%)".to_string(), true)),
        )
        .expect("probe");
        assert!(started, "a turn on a screen that moved is a landed wake");
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
        assert_eq!(out.0, QUEUED_WAKE);
        assert!(
            out.1.as_deref().is_some_and(|why| why.contains("busy")),
            "the journal gets the refusal: {out:?}"
        );
        let out = out.0;
        assert!(
            out.contains("doorbell rings at the next prompt"),
            "the operator is told who rings: {out}"
        );
        let hosted: std::collections::BTreeSet<String> =
            ["BlueRidge".to_string()].into_iter().collect();
        assert_eq!(
            crate::doorbell::take_deferred_wakes(&crate::config::state_dir(), &project, &hosted),
            vec![(
                "BlueRidge".to_string(),
                "[wake] New mail. Read it with: toron next".to_string()
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
            line: "[wake] Urgent mail. Read it with: toron next".into(),
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
            crate::pane_screen::InputLine::Empty,
        )
        .expect_err("the fold must name the normal line");
        assert!(err.contains("fold line differs"), "got: {err}");
        assert!(writes(&rec).is_empty(), "a refused ring types nothing");
    }
}

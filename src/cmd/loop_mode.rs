//! `flywheel loop` subcommand types (split from `main.rs`, bd-qbi8).

use clap::Subcommand;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use crate::config::Config;
use crate::hc::{AgentInfo, AgentStatus, Client};
use crate::output::{self, dim, error, kind_paint, print_json, status_paint};
use crate::{backend, durable, state, supervisor_host};

/// Subcommands of `flywheel loop` (bd-fw-stale-loop-processes-gac
/// restructure): `run` (default), `status`, `list`, `prune`.
#[derive(Subcommand)]
pub(crate) enum LoopCmd {
    /// Run the monitoring loop for a project.
    Run {
        project: String,
        /// poll interval in seconds (default 300 = 5 min); the SETTLED/base
        /// cadence — used when nobody is working
        #[arg(long, default_value = "300")]
        interval: u64,
        /// fast cadence while the swarm is working/blocked/unknown or right
        /// after a dispatch (default 30s; floor 5s — values below are clamped
        /// with a warning). Only affects the durable workflow; the sync loop
        /// (--once/--local) polls at --interval.
        #[arg(long, default_value = "30")]
        interval_active: u64,
        /// comma-separated instruction queue (stage names p1..p8 or raw prompts)
        #[arg(long)]
        queue: Option<String>,
        /// use a workflow from workflows.toml as the instruction queue
        #[arg(long)]
        workflow: Option<String>,
        /// text sent to idle agents when the queue is empty (default: none)
        #[arg(long)]
        nudge: Option<String>,
        /// overall max runtime in ms before failing (default: none)
        #[arg(long)]
        timeout_ms: Option<u64>,
        /// single poll pass then exit (cron-friendly)
        #[arg(long)]
        once: bool,
        /// blocked-agent policy: continue|stop|mail (default continue)
        #[arg(long, default_value = "continue")]
        on_blocked: String,
        /// agents to manage (default all)
        #[arg(long, default_value = "all")]
        to: String,
        /// run the sync polling loop instead of the durable engine-backed workflow
        #[arg(long)]
        local: bool,
        /// bead-driven mode: source work from `br ready` each poll instead of
        /// a static queue — feeds the swarm until the plan/beads are done
        #[arg(long)]
        work: bool,
        /// bd-gnej: disable the wave-gate (wave-N label ordering) — for
        /// graphs that do not use wave labels. Default: gating ON.
        #[arg(long)]
        no_wave_gates: bool,
        /// ADR-0006 D2 (requires --work): wave-scoped dispatch — the ready
        /// set is `br ready` ∩ beads labeled `wave-N`, held (error naming
        /// the blockers) while any lower wave-M (M<N) bead is open or
        /// in_progress. Label-based, transitional until beads ADR-0001
        /// schema-18 ships the typed `wave` field.
        #[arg(long)]
        wave_label: Option<String>,
        /// send prompts without landing confirmation (escape hatch; confirm
        /// is the default). Only affects the durable workflow.
        #[arg(long)]
        no_confirm: bool,
        /// confirm timeout in ms for `__send --confirm` (default: leaf default 45s)
        #[arg(long)]
        confirm_timeout_ms: Option<u64>,
        /// run the loop as a WorkflowEngine durable function (bd-e0xk
        /// coexistence path): dispatches run as `loop-dispatch` engine
        /// workflows instead of the imperative body's send leaf
        #[arg(long)]
        engine: bool,
        /// take over the project from a live loop process instead of refusing
        #[arg(long)]
        force: bool,
        /// bd-swarm-loop-durable-panic-atlo: replay a killed run's pending
        /// invocation checkpoints. Default is FRESH — a pending invocation
        /// belongs to a dead owner, and replaying its checkpoints against a
        /// newer binary's leaf shapes storms serialization errors. Conflicts
        /// with --force (fresh takeover).
        #[arg(long, conflicts_with = "force")]
        resume: bool,
        /// ADR-0019 Spec 3.3 (operator-only, logged): allow `loop run` in a
        /// workspace that carries an extra linked worktree (deliberate human
        /// experiment). Requires the operator key; the loop still never
        /// dispatches `--work` INTO the linked tree.
        #[arg(long)]
        allow_worktree: bool,
        /// bd-p4oc strict vacuous gate: refuse 0-test VERIFY (also FLYWHEEL_VERIFY_STRICT=1)
        #[arg(long)]
        strict: bool,
    },
    /// bd-swarm-loop-durable-panic-atlo: own the once-pass cadence the
    /// operator would otherwise hand-roll (`while true; do loop-mode run
    /// --once; sleep N; done`). Each pass is a CHILD process, so a crash
    /// cannot kill the supervisor; stops after 3 consecutive failed passes
    /// with one actionable line.
    Supervise {
        project: String,
        /// bead-driven passes: forward --work to `loop-mode run`
        #[arg(long)]
        work: bool,
        /// seconds between passes (floor 5)
        #[arg(long, default_value = "60")]
        interval: u64,
        /// agents to manage (forwarded to `loop-mode run`)
        #[arg(long, default_value = "all")]
        to: String,
        /// bd-supervise-dead-stop-escalation-rweq: dead-stop window in minutes.
        /// No pane working AND no commit past this window with work remaining
        /// emits ONE loud escalation line and pages the captain once, not per
        /// pass; a recovering fleet clears it without a second.
        /// bd-pane-wedge-detect-respawn-pnyi: the same window gates the
        /// per-pane wedge probe (process alive but parked on a fatal error
        /// line) — one page per wedge, cleared when a commit lands.
        #[arg(long, default_value_t = crate::durable::DEAD_STOP_DEFAULT_MINUTES)]
        stall_minutes: u64,
        /// Nested lead. Its own log and lease, beside the root host.
        /// A lead relaunch passes the child id here.
        #[arg(long)]
        context: Option<String>,
    },
    /// Query the state of a durable loop invocation from local state
    Status { project: String },
    /// List registered loop processes (pid registry) with live/dead status
    List,
    /// Remove pid-registry entries whose process is dead (stale cleanup)
    Prune,
    /// Infrastructure diagnostics + repair (bd-eign.4): engine-store capability,
    /// stale loop rows, orphaned loop PIDs, loops.json consistency. Human
    /// report by default, --json for machine. --fix drops stale rows + prunes
    /// orphaned pid entries.
    Doctor {
        /// auto-repair: reject stale invocations, prune orphaned pids
        #[arg(long)]
        fix: bool,
        /// machine-readable JSON health report
        #[arg(long)]
        json: bool,
    },
    /// Re-drive the recorded Msg journal through tick, assert identical actions (ADR-0025 Spec 3)
    Replay { project: String },
    /// Print (msg, state-delta, actions) triple for a journal seq (ADR-0025 Spec 3)
    Why { project: String, seq: usize },
}

pub(crate) fn cmd_loop_list() -> Result<()> {
    let pids = state::load_loop_pids();
    if pids.is_empty() {
        println!("no registered loops");
        return Ok(());
    }
    let rows: Vec<Value> = pids
        .iter()
        .map(|(project, e)| {
            json!({
                "project": project,
                "pid": e.pid,
                "started_at": e.started_at,
                "invocation": e.invocation,
                "alive": state::pid_alive(e.pid),
            })
        })
        .collect();
    print_json(&json!({ "loops": rows }));
    Ok(())
}

/// `flywheel loop prune`: drop pid-registry entries whose process is dead.
pub(crate) fn cmd_loop_prune() -> Result<()> {
    let mut pids = state::load_loop_pids();
    let before = pids.len();
    let dead: Vec<String> = pids
        .iter()
        .filter(|(_, e)| !state::pid_alive(e.pid))
        .map(|(p, _)| p.clone())
        .collect();
    for p in &dead {
        pids.remove(p);
    }
    state::save_loop_pids(&pids)?;
    print_json(&json!({
        "removed": dead,
        "removed_count": dead.len(),
        "remaining": pids.len(),
        "pruned": before - pids.len(),
    }));
    Ok(())
}

/// bd-pane-wedge-detect-respawn-pnyi: read every registered pin's pane for a
/// wedge — the agent process still UP, parked on a fatal error line, with no
/// commit movement inside the window. The verdict is `durable::pane_wedge_action`
/// (pure); this is only the live read. Soft-fail everywhere: an unreachable
/// hc, an empty registry, or an unreadable pane yields no wedges — never a
/// fabricated page.
fn probe_pane_wedges(
    cfg: &Config,
    project: &str,
    project_dir: &std::path::Path,
    now: u64,
    quiet_secs: u64,
    already_paged: &std::collections::HashSet<String>,
) -> Vec<durable::PaneWedgeAction> {
    let entries = state::load_spawn_registry(project);
    if entries.is_empty() {
        return Vec::new();
    }
    let Ok(c) = Client::connect(Some(&cfg.socket_path())) else {
        return Vec::new();
    };
    let Ok(agents) = c.agent_list() else {
        return Vec::new();
    };
    let commit_age = crate::cmd::captain::last_commit_age_secs(project_dir, now);
    let mut out = Vec::new();
    for e in &entries {
        let Some(pin) = e.mail_identity.as_deref().filter(|p| !p.trim().is_empty()) else {
            continue;
        };
        let Some(a) = agents.iter().find(|a| a.pane_id == e.pane_id) else {
            continue;
        };
        let kind = a.agent.as_deref().unwrap_or("");
        let alive = crate::hc::pane_process_info(&c, &e.pane_id)
            .ok()
            .and_then(|info| crate::hc::pane_agent_alive(&info, kind));
        let tail = crate::hc::pane_recent_text(&c, &e.pane_id).ok();
        let action = durable::pane_wedge_action(
            &durable::PaneWedgeInputs {
                pin,
                pane_id: &e.pane_id,
                process_alive: alive,
                recent_output: tail.as_deref(),
                last_commit_age_secs: commit_age,
                quiet_secs,
            },
            already_paged.contains(pin),
        );
        if !matches!(action, durable::PaneWedgeAction::Quiet) {
            out.push(action);
        }
    }
    out
}

/// `flywheel loop-mode supervise` (bd-swarm-loop-durable-panic-atlo): own
/// the once-pass cadence the operator would otherwise hand-roll. Each pass is
/// a CHILD process (`loop-mode run --once`), so a crashed pass cannot kill the
/// supervisor; 3 consecutive failed passes stop it with one actionable line.
///
/// bd-supervise-dead-stop-escalation-rweq: each pass also probes for a dead
/// stop — no pane working AND no commit for `stall_minutes` with work
/// remaining. A dead stop is invisible to the passes (they keep firing with
/// nothing to do), so this emits ONE loud escalation line and pages the captain
/// once per stall, never once per pass; a recovering fleet clears it silently.
pub(crate) fn cmd_loop_supervise(
    cfg: &Config,
    project: &str,
    work: bool,
    interval_secs: u64,
    to: &str,
    stall_minutes: u64,
    context: Option<&str>,
) -> Result<()> {
    if interval_secs < 5 {
        bail!("--interval must be >= 5 seconds");
    }
    supervisor_host::run_supervise(
        cfg,
        project,
        work,
        interval_secs,
        to,
        stall_minutes,
        context,
    )
}

/// Page the captain about a dead stop. Best-effort but LOUD: a failed page
/// never swallows the escalation — the line on stderr stands on its own, and
/// the failure names itself so the operator knows the page did not land.
fn page_captain_dead_stop(cfg: &Config, project: &str, line: &str) {
    page_captain_mail(cfg, project, "[loop-mode supervise] dead stop", line)
}

/// Page the captain on `subject`. Best-effort but LOUD: a failed page never
/// swallows the escalation, and the failure names itself so the operator
/// knows the page did not land.
pub(crate) fn page_captain_mail(cfg: &Config, project: &str, subject: &str, line: &str) {
    let Ok(key) = crate::project_dir_str(cfg, project) else {
        eprintln!(
            "flywheel: captain page FAILED (resolve project dir) — escalation line above stands"
        );
        return;
    };
    let mail = match crate::cmd::mail::mail_as(cfg, &key, None) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "flywheel: captain page FAILED (mail actor: {e:#}) — escalation line above stands"
            );
            return;
        }
    };
    let target = crate::resolve_mail_target_registry(project, "captain");
    let thread = format!("loop-supervise-{project}");
    match mail.send(
        &key,
        &[target],
        subject,
        line,
        "urgent",
        true,
        Some(&thread),
    ) {
        Ok(_) => eprintln!("flywheel: paged captain on {thread}"),
        Err(e) => eprintln!("flywheel: captain page FAILED ({e:#}) — escalation line above stands"),
    }
}

/// `flywheel loop doctor`: health report for loop infrastructure (bd-eign.4).
/// Logic lives in durable::loop_doctor; this is a thin runtime + render stub.
pub(crate) fn cmd_loop_doctor(cfg: &Config, fix: bool, json: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let mut report = rt.block_on(durable::loop_doctor(cfg, fix))?;
    let mut dead_doorbells: Vec<String> = Vec::new();
    let db_dir = crate::config::state_dir().join("doorbell");
    if let Ok(entries) = std::fs::read_dir(&db_dir) {
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("gen") {
                continue;
            }
            let Some(label) = path.file_stem().and_then(|x| x.to_str()) else {
                continue;
            };
            let alive = crate::doorbell::read_pid(&crate::doorbell::pid_file(
                &crate::config::state_dir(),
                label,
            ))
            .is_some_and(crate::doorbell::pid_alive);
            if !alive {
                dead_doorbells.push(label.to_string());
            }
        }
    }
    report["doorbell"] =
        json!({ "dead_dispatchers": { "count": dead_doorbells.len(), "entries": dead_doorbells } });
    if json {
        print_json(&report);
        return Ok(());
    }
    let store_ok = report["engine_store"]["ok"] == true;
    println!(
        "Engine store: {}",
        if store_ok {
            output::ok("park + wake")
        } else {
            output::error("UNUSABLE")
        }
    );
    if !store_ok {
        println!(
            "  {}",
            report["engine_store"]["error"]
                .as_str()
                .unwrap_or("unknown error")
        );
    }
    let stale = report["stale_invocations"]["count"].as_u64().unwrap_or(0);
    println!(
        "Stale loop rows: {}",
        if stale == 0 {
            output::ok("none")
        } else {
            output::warn(&stale.to_string())
        }
    );
    for id in report["stale_invocations"]["ids"]
        .as_array()
        .map(|a| a.iter())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
    {
        println!("  {id}");
    }
    let orphaned = report["orphaned_pids"]["count"].as_u64().unwrap_or(0);
    println!(
        "Orphaned loop pids: {}",
        if orphaned == 0 {
            output::ok("none")
        } else {
            output::warn(&orphaned.to_string())
        }
    );
    for e in report["orphaned_pids"]["entries"]
        .as_array()
        .map(|a| a.iter())
        .into_iter()
        .flatten()
    {
        println!(
            "  {} (pid {}, started {})",
            e["project"].as_str().unwrap_or("?"),
            e["pid"].as_u64().unwrap_or(0),
            e["started_at"].as_str().unwrap_or("?")
        );
    }
    let dead_db = report["doorbell"]["dead_dispatchers"]["count"]
        .as_u64()
        .unwrap_or(0);
    println!(
        "Doorbell dispatchers: {}",
        if dead_db == 0 {
            output::ok("all up")
        } else {
            output::warn(&format!("{dead_db} DEAD"))
        }
    );
    for entry in report["doorbell"]["dead_dispatchers"]["entries"]
        .as_array()
        .map(|a| a.iter())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
    {
        println!("  {entry} (heal: flywheel doorbell start <project>)");
    }
    println!(
        "Registry entries: {}",
        report["registry"]["count"].as_u64().unwrap_or(0)
    );
    for e in report["registry"]["entries"]
        .as_array()
        .map(|a| a.iter())
        .into_iter()
        .flatten()
    {
        println!(
            "  {} [{}] {} (pid {} alive={})",
            e["project"].as_str().unwrap_or("?"),
            e["verdict"].as_str().unwrap_or("?"),
            e["invocation"].as_str().unwrap_or("?"),
            e["pid"].as_u64().unwrap_or(0),
            e["process_alive"].as_bool().unwrap_or(false)
        );
    }
    if fix {
        let dropped = report["fixed"]["dropped"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or(0);
        let pruned = report["fixed"]["pruned"]
            .as_array()
            .map(std::vec::Vec::len)
            .unwrap_or(0);
        println!(
            "Fixed: dropped {} stale loop row(s), pruned {} orphaned pid(s)",
            dropped, pruned
        );
    } else {
        println!(
            "{}",
            output::dim("re-run with --fix to drop stale loop rows + prune orphaned pids")
        );
    }
    Ok(())
}

pub(crate) fn cmd_loop_replay(project: &str) -> Result<()> {
    let entries = crate::state::read_loop_journal(project);
    if entries.is_empty() {
        println!(
            "no journal for {project} (state_dir/journals/{}.jsonl)",
            crate::state::state_key(project)
        );
        return Ok(());
    }
    let mut model = crate::durable::LoopModel {
        queue: Vec::new(),
        per_bead_phase: Default::default(),
        wake_suppression: Default::default(),
        done_mailed: Default::default(),
        event_cursor: None,
        escalation: Default::default(),
        respawn: Default::default(),
        mail_health: Default::default(),
        mail_degraded: false,
        heal_states: Default::default(),
        heal_grace: Default::default(),
        heal_threshold: Default::default(),
        heal_budget: Default::default(),
        pending_events: Default::default(),
        states: Default::default(),
        dispatch_pending: Default::default(),
        drain_pending: Default::default(),
    };
    for (idx, entry) in entries.iter().enumerate() {
        let Some(msg_val) = entry.get("msg") else {
            continue;
        };
        let msg: crate::durable::LoopMsg = serde_json::from_value(msg_val.clone())
            .with_context(|| format!("journal {idx} msg decode"))?;
        if let Some(snap) = entry.get("model_snapshot") {
            if let Some(q) = snap.get("queue").and_then(|q| q.as_array()) {
                model.queue = q
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
            }
            if let Some(dp) = snap.get("dispatch_pending").and_then(|v| v.as_array()) {
                model.dispatch_pending = dp
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
            }
            if let Some(pb) = snap.get("per_bead_phase")
                && let Ok(map) = serde_json::from_value::<
                    std::collections::BTreeMap<String, crate::durable::LoopPhase>,
                >(pb.clone())
            {
                model.per_bead_phase = map;
            }
            if let Some(md) = snap
                .get("mail_degraded")
                .and_then(serde_json::Value::as_bool)
            {
                model.mail_degraded = md;
            }
        }
        let (next, actions) = crate::workspace::tick_from_loop_msg(&model, &msg);
        let recorded_actions = entry.get("actions").and_then(|v| v.as_array());
        let replayed_val = serde_json::to_value(&actions).unwrap_or(serde_json::Value::Null);
        let replayed_arr = replayed_val.as_array();
        let drift = match (recorded_actions, replayed_arr) {
            (Some(rec), Some(rep)) => rec != rep,
            (None, _) => false,
            (Some(_), None) => true,
        };
        if drift {
            anyhow::bail!(
                "replay drift at seq {idx}: recorded actions != replayed {} vs {} (msg {:?})",
                entry.get("actions").unwrap_or(&serde_json::Value::Null),
                replayed_val,
                msg
            );
        }
        model = next;
    }
    println!("replay {project}: {} entries, no drift", entries.len());
    Ok(())
}

pub(crate) fn cmd_loop_why(project: &str, seq: usize) -> Result<()> {
    let entries = crate::state::read_loop_journal(project);
    let Some(entry) = entries.get(seq) else {
        anyhow::bail!(
            "no journal entry {seq} for {project} (have {})",
            entries.len()
        );
    };
    let pretty = serde_json::to_string_pretty(entry)?;
    println!("{pretty}");
    Ok(())
}

pub(crate) fn cmd_artifacts(
    run_id: Option<&str>,
    json: bool,
    project: Option<&str>,
    cmd: &Option<crate::cmd::ArtifactsCmd>,
) -> Result<()> {
    if let Some(crate::cmd::ArtifactsCmd::Stitch { run_id, project }) = cmd {
        let proj = crate::project_door(project.as_deref(), "flywheel", "artifacts stitch")?;
        let path = crate::artifact::stitch_artifacts(proj, run_id)?;
        println!("{}", path.display());
        return Ok(());
    }
    let Some(rid) = run_id else {
        anyhow::bail!("flywheel artifacts: pass <run_id> or `stitch <run_id>`");
    };
    let proj = crate::project_door(project, "flywheel", "artifacts")?;
    let base = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("flywheel")
        .join(proj)
        .join("artifacts")
        .join(rid);
    let mut records = Vec::new();
    if base.is_dir() {
        for e in std::fs::read_dir(&base)? {
            let e = e?;
            let p = e.path();
            if p.extension().map(|x| x == "json").unwrap_or(false) {
                let raw = std::fs::read_to_string(&p)?;
                let rec: serde_json::Value = serde_json::from_str(&raw)?;
                records.push(rec);
            }
        }
    }
    if json {
        print_json(&serde_json::json!(records));
    } else {
        for r in &records {
            println!("{}", serde_json::to_string(r)?);
        }
        if records.is_empty() {
            println!("no artifacts for run {rid} in {proj}");
        }
    }
    Ok(())
}

/// What the loop should do for one agent on one poll — the SINGLE
/// definition lives in durable.rs (bd-w4-delete-dup-sbh8); the sync
/// (`--local`/`--once`) path delegates to `durable::plan_actions`.
/// Build the instruction queue from --queue / --workflow.
/// Clamp the active cadence to the 5s floor (consistent with the workflow's
/// `INTERVAL_MIN` in `next_interval_secs`). Pure — the warning is emitted by
/// the caller when the value changes.
fn clamp_active_interval(v: u64) -> u64 {
    v.max(5)
}

fn build_queue(
    queue: Option<&str>,
    workflow: Option<&str>,
    project: Option<&str>,
) -> Result<Vec<String>> {
    let mut items: Vec<String> = Vec::new();
    if let Some(wf) = workflow {
        let mut handled = false;
        {
            let all = crate::workflow_catalog::all_workflows_plain(None)
                .unwrap_or_else(|_| crate::builtin_workflows::builtin_workflows());
            if let Some(_uwf) = all.get(wf) {
                if let Some(proj) = project {
                    let exe = std::env::current_exe()?;
                    let output = std::process::Command::new(exe)
                        .args([
                            "workflow",
                            "run",
                            wf,
                            "--execute",
                            "--mode",
                            "pane",
                            "--project",
                            proj,
                            "--yes",
                            "--json",
                        ])
                        .output()?;
                    if !output.status.success() {
                        anyhow::bail!(
                            "workflow {wf} failed (engine invoke): {}",
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }
                    handled = true;
                } else {
                    anyhow::bail!(
                        "workflow {wf} requires --project <proj> to run via the engine (use 'flywheel workflow run {wf} --execute --mode pane --project <proj>')"
                    );
                }
            }
        }
        if !handled {
            let workflows = crate::config::Workflows::load()?;
            let steps = workflows.workflows.get(wf).ok_or_else(|| {
                anyhow::anyhow!("workflow not found: {wf} (flywheel workflows list)")
            })?;
            for step in steps {
                match step {
                    crate::config::WorkflowStep::Send { stage, msg, .. } => {
                        let text = match (stage.as_deref(), msg.as_deref()) {
                            (Some(s), _) => crate::stage_prompt(s)?,
                            (None, Some(m)) => m.to_string(),
                            (None, None) => bail!("workflow {wf}: send step needs stage or msg"),
                        };
                        items.push(text);
                    }
                    crate::config::WorkflowStep::Wait { .. } => {}
                }
            }
        }
    }
    if let Some(q) = queue {
        for token in q.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if let Ok(p) = crate::stage_prompt(token) {
                items.push(p);
            } else {
                items.push(token.to_string());
            }
        }
    }
    Ok(items)
}

/// Run `br ready --json` in the project dir, returning the ready beads.
/// Caller decides how to treat an error (the sync loop soft-fails).
pub(crate) fn load_ready_beads(cfg: &Config, project: &str) -> Result<Vec<durable::ReadyBead>> {
    let dir = crate::project_dir(cfg, project);
    crate::br::Bead::ready(Some(&dir))
}

/// Run `br list --status in_progress --json` in the project dir.
pub(crate) fn load_inprogress_beads(
    cfg: &Config,
    project: &str,
) -> Result<Vec<durable::ReadyBead>> {
    let dir = crate::project_dir(cfg, project);
    crate::br::Bead::in_progress(Some(&dir))
}

/// Sync-loop `--work` queue refresh (parity with the durable loop's
/// bead-driven mode): resume interrupted in-progress beads first, then hand
/// out fresh ready beads. Each bead is assigned at most once per loop run
/// (the `assigned` set is per-invocation state). Pure — fully testable.
/// bd-g0ge: the window value the loop resume filter gates on — keyed on
/// the live attempt generation when an attempt row exists, else the legacy
/// bead key. Pure: the window and the attempts arrive as values, so the
/// filter and its test read the same lookup.
fn attempt_window_last_us<'a>(
    window: &std::collections::BTreeMap<String, i64>,
    attempts: &'a [durable::WorkAttempt],
    bead_id: &str,
    pane: &str,
) -> (Option<i64>, Option<&'a durable::WorkAttempt>) {
    let live = durable::live_attempt_for_bead(attempts, bead_id);
    let last = match live {
        Some(a) => window
            .get(&crate::state::attempt_resume_key(
                bead_id,
                pane,
                a.generation,
            ))
            .copied(),
        None => window
            .get(&crate::state::resume_key(bead_id, pane))
            .copied(),
    };
    (last, live)
}

fn work_prompts(
    inprog: &[durable::ReadyBead],
    ready: &[durable::ReadyBead],
    assigned: &mut Vec<String>,
    project: &str,
) -> Vec<String> {
    let mut prompts = Vec::new();
    let mut fresh: Vec<&durable::ReadyBead> = Vec::new();
    for bead in inprog {
        if !assigned.contains(&bead.id) {
            prompts.push(durable::brief(&bead.id, &bead.title, project, true));
            fresh.push(bead);
        }
    }
    for bead in ready {
        if !assigned.contains(&bead.id) {
            prompts.push(durable::brief(&bead.id, &bead.title, project, false));
            fresh.push(bead);
        }
    }
    assigned.extend(fresh.into_iter().map(|b| b.id.clone()));
    prompts
}

/// bd-resume-window-lease-sweep-4sa2: live `(reason, holder)` pairs from the
/// project's reservation board. Feeds the resume peer-hold check. A mail
/// hiccup yields an empty board — never a loop crash, and never a false
/// peer-hold (an unread board cannot invent a competing holder).
fn live_reservation_holders(cfg: &Config, project: &str) -> Vec<(String, String)> {
    let Ok(key) = crate::project_dir_str(cfg, project) else {
        return Vec::new();
    };
    let Ok(mail) = crate::cmd::mail::mail_or_die(cfg) else {
        return Vec::new();
    };
    let Ok(v) = mail.list_active(&key, None, None) else {
        return Vec::new();
    };
    v.get("reservations")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let reason = r.get("reason").and_then(Value::as_str)?.to_string();
                    let holder = r
                        .get("holder")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Some((reason, holder))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Live `(path, reason, holder)` rows. The resume fold uses these for
/// `file_blocks_bead`. A mail hiccup is an empty board, never a false block.
pub(crate) fn live_file_rows(cfg: &Config, project: &str) -> Vec<(String, String, String)> {
    let Ok(key) = crate::project_dir_str(cfg, project) else {
        return Vec::new();
    };
    let Ok(mail) = crate::cmd::mail::mail_or_die(cfg) else {
        return Vec::new();
    };
    let Ok(v) = mail.list_active(&key, None, None) else {
        return Vec::new();
    };
    v.get("reservations")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let path = r.get("path").and_then(Value::as_str)?.to_string();
                    let reason = r
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let holder = r
                        .get("holder")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    Some((path, reason, holder))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// bd-reserve-yield-taqf: live `(bead, holder)` yield pairs — toron's
/// structured stand-down (`reserve yield`), read through the same join as the
/// lease board. Same soft-fail contract as the reservation board: a hiccup
/// yields no pairs, and an unread board cannot invent a hold.
fn live_yielded_pairs(cfg: &Config, project: &str) -> Vec<(String, String)> {
    let Ok(mail) = crate::cmd::mail::mail_or_die(cfg) else {
        return Vec::new();
    };
    mail.yielded_pairs(project).unwrap_or_default()
}

/// Unregisters this process's loop-pid entry on exit (any path). Only ever
/// removes the entry when it still belongs to THIS pid — a newer loop that
/// took over (--force) is never unregistered by the older process exiting
/// late (bd-fw-stale-loop-processes-gac).
struct LoopPidGuard {
    project: String,
    pid: u32,
}

impl Drop for LoopPidGuard {
    fn drop(&mut self) {
        crate::state::unregister_loop(&self.project, self.pid);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn cmd_loop(
    cfg: &Config,
    project: &str,
    interval_secs: u64,
    active_interval_secs: u64,
    queue: Option<&str>,
    workflow: Option<&str>,
    nudge: Option<&str>,
    timeout_ms: Option<u64>,
    once: bool,
    local: bool,
    work: bool,
    no_wave_gates: bool,
    wave_label: Option<&str>,
    on_blocked: &str,
    to: &str,
    no_confirm: bool,
    confirm_timeout_ms: Option<u64>,
    engine: bool,
    force: bool,
    resume: bool,
    strict: bool,
) -> Result<()> {
    if interval_secs < 5 {
        bail!("--interval must be >= 5 seconds");
    }
    let strict = strict
        || std::env::var("FLYWHEEL_VERIFY_STRICT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
    let _guard = LoopPidGuard {
        project: project.to_string(),
        pid: std::process::id(),
    };
    if let Some(prev) = state::register_loop(project, "", force)? {
        bail!(
            "a loop for {project} is already running (pid {}, started {}). Inspect: flywheel loop list; cleanup: flywheel loop prune; take over: flywheel loop run {project} --force.",
            prev.pid,
            prev.started_at
        );
    }
    let active_secs = clamp_active_interval(active_interval_secs);
    if active_secs != active_interval_secs {
        eprintln!(
            "flywheel: warning: --interval-active {active_interval_secs}s below the 5s floor — clamping to 5"
        );
    }
    if !matches!(on_blocked, "continue" | "stop" | "mail") {
        bail!("--on-blocked must be continue|stop|mail");
    }
    if let Some(label) = wave_label {
        if !work {
            bail!("--wave-label requires --work");
        }
        if label
            .strip_prefix("wave-")
            .and_then(|n| n.parse::<u32>().ok())
            .is_none()
        {
            bail!("--wave-label must be `wave-<N>` (got `{label}`)");
        }
    }
    let mut queue_items = build_queue(queue, workflow, Some(project))?;

    if !local && !once {
        let max_iterations = timeout_ms.map(|ms| (ms / 1000).div_ceil(interval_secs.max(1)));
        let params = durable::LoopParams {
            project: project.to_string(),
            to: to.to_string(),
            interval_secs,
            active_interval_secs: active_secs,
            queue: queue_items.clone(),
            nudge: nudge.map(String::from),
            on_blocked: on_blocked.to_string(),
            max_iterations,
            work,
            no_wave_gates,
            wave_label: wave_label.map(String::from),
            confirm_sends: !no_confirm,
            confirm_timeout_ms,
            invocation: String::new(),
            unknown_diagnose_threshold: cfg.unknown_diagnose_threshold(),
            respawn_budget: cfg.respawn_budget(),
            reclaim: crate::durable_policy::ReclaimPolicy::from_environment(),
            pane_stall_secs: durable::pane_stall_threshold_from_environment(),
            dispatch_guard_ttl_us: std::env::var("FLYWHEEL_DISPATCH_TTL_SECS")
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|secs| *secs > 0)
                .map(|secs| secs * 1_000_000)
                .unwrap_or(durable::DISPATCH_GUARD_TTL_US),
            mail_health_escalate_at: crate::state::mail_health_escalate_at(),
            pause_resurface_secs: crate::write_liveness::PAUSE_RESURFACE_DEFAULT_SECS,
            walk_timeout_secs: crate::write_liveness::WALK_TIMEOUT_DEFAULT_SECS,
            unknown_detect_grace_polls: cfg.unknown_detect_grace_polls(),
            drain_grace_polls: cfg.drain_grace_polls(),
            claim_nudge_after_polls: cfg.claim_nudge_after_polls(),
            force,
            resume,
            strict,
        };
        let rt = tokio::runtime::Runtime::new()?;
        if engine {
            println!(
                "loop: swarm monitor for {} (--engine routed to the durable keeper; resume: flywheel loop run {project})",
                dim(project)
            );
            let result = rt.block_on(durable::run_engine_loop(cfg, params))?;
            println!("loop: {result}");
            crate::log(
                project,
                state::EventKind::Loop,
                json!({ "status": "complete", "engine": true, "result": result }),
            );
            return Ok(());
        }
        let result = rt.block_on(durable::run_durable_loop(cfg, params))?;
        println!("loop: {result}");
        crate::log(
            project,
            state::EventKind::Loop,
            json!({ "status": "complete", "result": result }),
        );
        return Ok(());
    }

    let mut assigned: Vec<String> = Vec::new();
    let mut not_ready_mailed: std::collections::HashSet<String> = std::collections::HashSet::new();
    if queue_items.is_empty() && nudge.is_none() && !work {
        eprintln!("flywheel: no instructions queued and no --nudge — loop will only monitor");
    }

    let mut c = Client::connect(Some(&cfg.socket_path()))?;
    let ws_id = crate::find_workspace(&mut c, project)?;
    let deadline = timeout_ms.map(|ms| std::time::Instant::now() + Duration::from_millis(ms));

    crate::log(
        project,
        state::EventKind::Loop,
        crate::loop_monitor_notice(interval_secs, queue_items.len(), to, on_blocked),
    );
    println!(
        "loop: monitoring {} (interval {}s, queue {}{})",
        dim(project),
        interval_secs,
        queue_items.len(),
        if nudge.is_some() {
            " + nudge".to_string()
        } else {
            String::new()
        }
    );

    loop {
        if let Some(d) = deadline
            && std::time::Instant::now() >= d
        {
            bail!(
                "loop timed out after {}s (queue remaining: {})",
                timeout_ms.unwrap_or(0) / 1000,
                queue_items.len()
            );
        }

        let agents = c
            .agent_list()?
            .into_iter()
            .filter(|a| a.workspace_id == ws_id)
            .collect::<Vec<_>>();
        let managed: Vec<(String, AgentStatus)> = if to == "all" {
            agents
                .iter()
                .map(|a| (crate::agent_target(a).to_string(), a.agent_status))
                .collect()
        } else {
            let resolved = crate::resolve_agents(&mut c, &ws_id, project, to)?;
            resolved
                .iter()
                .map(|a| (crate::agent_target(a).to_string(), a.agent_status))
                .collect()
        };

        if work {
            let mut inprog_now: Vec<durable::ReadyBead> = Vec::new();
            let window = state::load_resume_window(project);
            let attempts = state::load_work_attempts(project);
            let mut peers = live_reservation_holders(cfg, project);
            peers.extend(live_yielded_pairs(cfg, project));
            let file_rows = live_file_rows(cfg, project);
            let now_us = chrono::Utc::now().timestamp_micros();
            let mut inprog_items: Vec<String> = Vec::new();
            let mut ready_items: Vec<String> = Vec::new();
            let mut ready_now: Vec<durable::ReadyBead> = Vec::new();
            match load_inprogress_beads(cfg, project) {
                Ok(inprog) => {
                    let resumable: Vec<durable::ReadyBead> = inprog
                        .iter()
                        .filter(|b| {
                            let pane = b.assignee.as_deref().unwrap_or("");
                            let (last, live) =
                                attempt_window_last_us(&window, &attempts, &b.id, pane);
                            let intended =
                                durable::path_tokens(&format!("{} {}", b.title, b.description));
                            durable::should_resume_attempt(
                                last,
                                now_us,
                                durable::RESUME_WINDOW_DEFAULT_SECS,
                                durable::peer_held(&b.id, &peers, pane)
                                    || durable::file_blocks_bead(
                                        &b.id, &file_rows, pane, &intended,
                                    ),
                                live,
                            )
                        })
                        .cloned()
                        .collect();
                    let to_record: Vec<(String, String)> = resumable
                        .iter()
                        .filter(|b| !assigned.contains(&b.id))
                        .map(|b| (b.id.clone(), b.assignee.clone().unwrap_or_default()))
                        .collect();
                    let to_resume: Vec<String> =
                        work_prompts(&resumable, &[], &mut assigned, project);
                    for (bead, pane) in &to_record {
                        if let Err(e) = state::record_resume(project, bead, pane, now_us) {
                            eprintln!(
                                "flywheel: resume-window record for {bead} failed (continuing): {e:#}"
                            );
                        }
                        match state::record_attempt_resume(project, bead, pane, None, now_us) {
                            Ok(rec) => {
                                let key = state::attempt_resume_key(bead, pane, rec.generation);
                                if let Err(e) = state::update_locked(
                                    &state::resume_window_file(project),
                                    |map: &mut std::collections::BTreeMap<String, i64>| {
                                        map.insert(key.clone(), now_us);
                                    },
                                ) {
                                    eprintln!(
                                        "flywheel: attempt resume record for {bead} failed (continuing): {e:#}"
                                    );
                                }
                            }
                            Err(e) => eprintln!(
                                "flywheel: attempt resume record for {bead} failed (continuing): {e:#}"
                            ),
                        }
                    }
                    inprog_items.extend(to_resume);
                    inprog_now = inprog;
                    for b in &inprog_now {
                        let pane = b.assignee.as_deref().unwrap_or("");
                        let intended =
                            durable::path_tokens(&format!("{} {}", b.title, b.description));
                        if !durable::file_blocks_bead(&b.id, &file_rows, pane, &intended) {
                            continue;
                        }
                        let Some((holder, pattern)) =
                            durable::file_hold_notice(&b.id, &file_rows, pane, &intended)
                        else {
                            continue;
                        };
                        let hold_key = format!("filehold\x1f{}\x1f{}", b.id, holder);
                        if window.get(&hold_key).is_some_and(|t| {
                            now_us.saturating_sub(*t)
                                < crate::request_receipts::FILE_HOLD_WINDOW_SECS as i64 * 1_000_000
                        }) {
                            continue;
                        }
                        let args = toron_core::mailbox::messaging::SendMessage {
                            project_slug: project.to_string(),
                            sender_name: "captain".to_string(),
                            to: vec![holder.clone()],
                            subject: format!("[file-hold] {}", b.id),
                            body_md: format!("bead={}\npath={}", b.id, pattern),
                            ..Default::default()
                        };
                        match crate::mail::block_on(
                            crate::mail::comms_mailbox()?.send_message(&args),
                        ) {
                            Ok(_) => {
                                if let Err(e) = state::update_locked(
                                    &state::resume_window_file(project),
                                    |map: &mut std::collections::BTreeMap<String, i64>| {
                                        map.insert(hold_key.clone(), now_us);
                                    },
                                ) {
                                    eprintln!(
                                        "flywheel: file-hold watermark for {} failed (continuing): {e:#}",
                                        b.id
                                    );
                                }
                            }
                            Err(e) => eprintln!(
                                "flywheel: file-hold mail for {} to {holder} refused (retry pass): {e:#}",
                                b.id
                            ),
                        }
                    }
                }
                Err(e) => eprintln!("flywheel: br inprogress failed (continuing): {e:#}"),
            }
            match load_ready_beads(cfg, project) {
                Ok(ready) => {
                    let (dispatchable, not_ready) = durable::partition_ready(&ready);
                    let wave_held: Vec<String> = if no_wave_gates {
                        Vec::new()
                    } else {
                        durable::wave_gate_held(&ready, &inprog_now)
                    };
                    if !wave_held.is_empty() {
                        eprintln!(
                            "flywheel: wave-gate: holding {} (earlier wave open; not stuck-owner)",
                            wave_held.join(", ")
                        );
                    }
                    let mut scope_block: Option<String> = None;
                    let scope_ids: Option<std::collections::HashSet<String>> =
                        wave_label.map(|label| {
                            match durable::wave_scope_filter(&ready, &inprog_now, label) {
                                Ok(scoped) => scoped.into_iter().map(|b| b.id).collect(),
                                Err(e) => {
                                    eprintln!("flywheel: {e}");
                                    scope_block = Some(e);
                                    Default::default()
                                }
                            }
                        });
                    let to_assign: Vec<durable::ReadyBead> = dispatchable
                        .into_iter()
                        .filter(|&b| !wave_held.contains(&b.id))
                        .filter(|&b| {
                            let intended =
                                durable::path_tokens(&format!("{} {}", b.title, b.description));
                            !durable::file_blocks_bead(
                                &b.id,
                                &file_rows,
                                b.assignee.as_deref().unwrap_or(""),
                                &intended,
                            )
                        })
                        .filter(|&b| match (&scope_ids, &scope_block) {
                            (Some(ids), _) => ids.contains(&b.id),
                            (None, Some(_)) => false,
                            (None, None) => true,
                        })
                        .cloned()
                        .collect();
                    ready_now.clone_from(&to_assign);
                    let to_assign: Vec<String> =
                        work_prompts(&[], &to_assign, &mut assigned, project);
                    ready_items.extend(to_assign);
                    for bead in &not_ready {
                        if not_ready_mailed.insert(bead.id.clone())
                            && let Err(e) = crate::cmd_internal_mail_not_ready(
                                cfg,
                                project,
                                &bead.id,
                                &bead.title,
                                Some(&if bead.verify.is_none() {
                                    "## VERIFY fence missing, empty, or multi-line".to_string()
                                } else {
                                    durable::principles_block_reason(bead).unwrap_or_default()
                                }),
                            )
                        {
                            eprintln!(
                                "flywheel: mail-not-ready for {} failed (continuing): {e:#}",
                                bead.id
                            );
                        }
                    }
                }
                Err(e) => eprintln!("flywheel: br ready failed (continuing): {e:#}"),
            }

            let live_pins: Vec<String> = managed.iter().map(|(n, _)| n.clone()).collect();
            let idle_pins: Vec<String> = managed
                .iter()
                .filter(|(_, s)| matches!(s, AgentStatus::Idle | AgentStatus::Done))
                .map(|(n, _)| n.clone())
                .collect();
            let starved = state::load_pane_starvation(project);
            let plan = durable::idle_pane_rotation(&durable::IdleRotationInputs {
                live_pins: &live_pins,
                idle_pins: &idle_pins,
                in_progress: &inprog_now,
                ready: &ready_now,
                peers: &peers,
                taken: &assigned,
                starved: &starved,
                resume_window: &window,
                now_us,
                window_secs: durable::RESUME_WINDOW_DEFAULT_SECS,
                exempt_after_passes: cfg.resume_exempt_passes(),
            });
            if let Err(e) = state::record_pane_starvation(project, &plan.starved_after) {
                eprintln!("flywheel: pane-starvation record failed (continuing): {e:#}");
            }
            if !plan.picks.is_empty() {
                let mut pick_items: Vec<String> = Vec::new();
                for pick in &plan.picks {
                    let held = inprog_now.iter().find(|b| b.id == pick.bead);
                    let title = held
                        .or_else(|| ready_now.iter().find(|b| b.id == pick.bead))
                        .map(|b| b.title.as_str())
                        .unwrap_or("");
                    pick_items.push(durable::brief(&pick.bead, title, project, held.is_some()));
                    println!(
                        "{} {} ← {}{}",
                        dim("[loop] idle-pane"),
                        dim(&pick.pin),
                        dim(&pick.bead),
                        if pick.exempt {
                            format!(
                                " (window-exempt: starved {} passes)",
                                starved.get(&pick.pin).copied().unwrap_or(0)
                            )
                        } else {
                            String::new()
                        }
                    );
                    assigned.push(pick.bead.clone());
                }
                queue_items.splice(0..0, pick_items);
            }
            queue_items.extend(inprog_items);
            queue_items.extend(ready_items);
        }

        let d_states: Vec<durable::AgentState> = managed
            .iter()
            .map(|(name, status)| durable::AgentState {
                name: name.clone(),
                status: format!("{status:?}").to_ascii_lowercase(),
                kind: String::new(),
                pane_id: String::new(),
                mail_identity: None,
                nudge_streak: 0,
            })
            .collect();
        let actions = durable::plan_actions(
            &crate::durable::LoopModel {
                queue: queue_items.clone(),
                per_bead_phase: Default::default(),
                wake_suppression: Default::default(),
                done_mailed: Default::default(),
                event_cursor: None,
                escalation: Default::default(),
                respawn: Default::default(),
                mail_health: Default::default(),
                mail_degraded: false,
                heal_states: Default::default(),
                heal_grace: Default::default(),
                heal_threshold: Default::default(),
                heal_budget: Default::default(),
                pending_events: Default::default(),
                states: Default::default(),
                dispatch_pending: Default::default(),
                drain_pending: Default::default(),
            },
            &d_states,
            nudge,
            &HashMap::new(),
            &std::collections::HashSet::new(),
        );
        if on_blocked == "stop" {
            let blocked: Vec<&str> = actions
                .iter()
                .filter_map(|a| match a {
                    durable::LoopAction::Blocked { name } => Some(name.as_str()),
                    _ => None,
                })
                .collect();
            if !blocked.is_empty() {
                bail!("agent(s) {} blocked — stopping loop", blocked.join(", "));
            }
        }
        let mut any_blocked = false;
        let mut any_work = false;
        for action in &actions {
            match action {
                durable::LoopAction::PinStamp { bead_id, pin } => {
                    let exe = std::env::current_exe()?;
                    let out = std::process::Command::new(exe)
                        .args(["__pin-stamp", project, bead_id, pin])
                        .output();
                    if let Ok(o) = out
                        && !o.status.success()
                    {
                        eprintln!("flywheel: __pin-stamp for {bead_id} failed");
                    }
                }
                durable::LoopAction::Send { name, text }
                | durable::LoopAction::DispatchImmediate { name, text } => {
                    let agent_profile =
                        state::load_spawn_registry(&crate::project_dir_str(cfg, project)?)
                            .into_iter()
                            .find(|e| e.name == *name)
                            .and_then(|e| e.profile)
                            .unwrap_or_default();
                    let mut cmd = std::process::Command::new(std::env::current_exe()?);
                    cmd.arg("__send").arg(project).arg(name).arg(text);
                    if !agent_profile.is_empty() {
                        cmd.arg("--profile").arg(&agent_profile);
                    }
                    let out = cmd
                        .output()
                        .map_err(|e| anyhow::anyhow!("__send for {name} failed: {e:#}"))?;
                    let raw = String::from_utf8_lossy(&out.stdout).to_string();
                    let ok = serde_json::from_str::<Value>(raw.trim())
                        .ok()
                        .and_then(|v| v.get("ok").and_then(Value::as_bool))
                        .unwrap_or(false);
                    if !ok {
                        let err_bytes = String::from_utf8_lossy(&out.stderr).to_string();
                        let code = out.status.code().unwrap_or(-1);
                        eprintln!(
                            "{}",
                            crate::cmd::send::format_send_failure(
                                name,
                                raw.trim(),
                                err_bytes.trim(),
                                out.status.success(),
                                code
                            )
                        );
                        queue_items.insert(0, text.clone());
                        continue;
                    }
                    crate::log(
                        project,
                        state::EventKind::Send,
                        json!({ "to": name, "stage": "-", "text_len": text.len(), "wait": false, "source": "loop" }),
                    );
                    println!(
                        "{} → {} {}",
                        dim("[loop] sent"),
                        dim(name),
                        dim(&text.chars().take(60).collect::<String>())
                    );
                    any_work = true;
                }
                durable::LoopAction::Nudge { name } => {
                    let n = nudge.unwrap_or("proceed");
                    if let Err(e) = c.agent_prompt(name, n, None) {
                        eprintln!("flywheel: nudge to {name} failed: {e:#}");
                    } else {
                        crate::log(
                            project,
                            state::EventKind::Send,
                            json!({ "to": name, "nudge": true, "text_len": n.len() }),
                        );
                        println!("{} {name} → (nudge)", dim("[loop] sent"));
                        any_work = true;
                    }
                }
                durable::LoopAction::Blocked { name } => {
                    any_blocked = true;
                    match on_blocked {
                        "mail" => {
                            if let Ok(m) = crate::cmd::mail::mail_or_die(cfg) {
                                let key = crate::project_dir_str(cfg, project)?;
                                let _ = m.ensure_project(&key);
                                let mail_target =
                                    crate::resolve_mail_target_registry(project, name);
                                let _ = m.send(
                                    &key,
                                    std::slice::from_ref(&mail_target),
                                    "[flywheel] you are blocked",
                                    "The swarm loop detected you are blocked. Resolve or report the blocker.",
                                    "high",
                                    true,
                                    Some(&format!("health-{name}")),
                                );
                                println!("{} {name} → mail alert", dim("[loop] blocked"));
                            } else {
                                eprintln!("flywheel: --on-blocked mail needs [mail.toron] config");
                            }
                        }
                        _ => {
                            println!("{} {name}", error("[loop] blocked"));
                        }
                    }
                }
                durable::LoopAction::PausedResurface {
                    name,
                    bead_id,
                    owner,
                    age_secs,
                } => {
                    println!(
                        "{} {}",
                        dim("[loop]"),
                        durable::pause_resurface_line(name, bead_id, owner, *age_secs)
                    );
                    crate::log(
                        project,
                        state::EventKind::Loop,
                        json!({
                            "status": "pause_resurface",
                            "agent": name,
                            "bead": bead_id,
                            "owner": owner,
                            "age_secs": *age_secs,
                        }),
                    );
                }
                durable::LoopAction::Wait => {}
                durable::LoopAction::Diagnose { .. }
                | durable::LoopAction::Respawn { .. }
                | durable::LoopAction::Reclaim { .. } => {}
            }
        }

        let statuses: Vec<AgentStatus> = managed.iter().map(|(_, s)| *s).collect();
        let all_settled = statuses
            .iter()
            .all(|s| matches!(s, AgentStatus::Idle | AgentStatus::Done));
        let queue_drained = queue_items.is_empty();

        if once {
            println!("{}", dim("[loop] --once pass done"));
            if any_blocked && on_blocked == "continue" {
                println!(
                    "{}",
                    dim(
                        "loop: agents blocked (on_blocked=continue; pass --on-blocked stop|mail to control)"
                    )
                );
            }
            return Ok(());
        }

        if queue_drained && all_settled && !any_work {
            println!("{}", dim("[loop] queue drained, swarm settled"));
            crate::log(
                project,
                state::EventKind::Loop,
                json!({ "status": "complete" }),
            );
            return Ok(());
        }

        let wait_for = if let Some(d) = deadline {
            let remaining = d.saturating_duration_since(std::time::Instant::now());
            remaining.min(Duration::from_secs(interval_secs))
        } else {
            Duration::from_secs(interval_secs)
        };
        std::thread::sleep(wait_for);
    }
}

#[cfg(test)]
mod verifier_dispatch_tests {
    use super::*;

    /// Scratch workspace: git + br + one bead with a typed `verify` field.
    fn scratch_vd(dir: &std::path::Path, slug: &str, priority: &str) -> String {
        crate::br::scratch_bead(
            dir,
            "workflow:\n  strict: true\n  statuses: [open, in_progress, closed, deferred, blocked]\n  transitions:\n    open: [in_progress, closed, deferred, blocked]\n    in_progress: [open, closed, deferred]\n    deferred: [open]\n    closed: [open]\n",
            "vd",
            slug,
            priority,
            Some("cargo test -p flywheel verifier"),
            Some("prove-it-works — fixture carries runnable VERIFY fence, not a self-report"),
            None,
        )
    }

    fn status_of(dir: &std::path::Path, bead_id: &str) -> String {
        crate::br::bead_status(dir, bead_id)
    }

    /// A config whose mail backend fails FAST (daemon at port 1) so the
    /// degrade test never stalls on a real relay round-trip.
    fn no_mail_cfg() -> Config {
        Config {
            mail: Some(crate::config::MailSections {
                toron: Some(crate::config::MailServerConfig {
                    url: Some("http://127.0.0.1:1".to_string()),
                    token: None,
                }),
            }),
            ..Default::default()
        }
    }

    fn write_registry(toml: &str) {
        let cfg_dir = crate::config::config_dir();
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("models.toml"), toml).unwrap();
    }

    /// Fixture 3 (ADR-0019 Spec 4): single-family swarm + P0 bead ->
    /// blocked `verifier-family-missing`; idempotent on re-invocation;
    /// never reopened/respawned.
    #[test]
    fn single_family_p0_is_blocked_not_reopened() {
        let _g = crate::config::lock_env();
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("vd-degrade-{}", std::process::id()));
        let bead_id = scratch_vd(&dir, "vd-deg", "0");
        write_registry("[models.\"deepseek/deepseek-v4\"]\n[models.\"deepseek/deepseek-v3\"]\n");
        let out = crate::cmd_internal_verify_dispatch(
            &cfg,
            "vd-test",
            &bead_id,
            crate::VerifyDispatchOpts {
                worker_model: Some("deepseek-v4".to_string()),
                candidates: Some("deepseek-v4,deepseek-v3".to_string()),
                blast: "normal".to_string(),
                ac: "checkable".to_string(),
                dir: Some(dir.clone()),
            },
        );
        assert!(out.is_ok(), "{out:?}");
        assert_eq!(status_of(&dir, &bead_id), "blocked");
        let out2 = crate::cmd_internal_verify_dispatch(
            &cfg,
            "vd-test",
            &bead_id,
            crate::VerifyDispatchOpts {
                worker_model: Some("deepseek-v4".to_string()),
                candidates: Some("deepseek-v4,deepseek-v3".to_string()),
                blast: "normal".to_string(),
                ac: "checkable".to_string(),
                dir: Some(dir.clone()),
            },
        );
        assert!(out2.is_ok(), "{out2:?}");
        assert_eq!(status_of(&dir, &bead_id), "blocked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fixture 1 (decision half): P0 with a second family -> dispatch the
    /// different-family model; the bead is NOT blocked.
    #[test]
    fn p0_with_second_family_dispatches_and_is_not_blocked() {
        let _g = crate::config::lock_env();
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("vd-dispatch-{}", std::process::id()));
        let bead_id = scratch_vd(&dir, "vd-disp", "0");
        write_registry("[models.\"deepseek/deepseek-v4\"]\n[models.\"anthropic/opus-5\"]\n");
        let out = crate::cmd_internal_verify_dispatch(
            &cfg,
            "vd-test",
            &bead_id,
            crate::VerifyDispatchOpts {
                worker_model: Some("deepseek-v4".to_string()),
                candidates: Some("deepseek-v4,opus-5".to_string()),
                blast: "normal".to_string(),
                ac: "checkable".to_string(),
                dir: Some(dir.clone()),
            },
        );
        assert!(out.is_ok(), "{out:?}");
        assert_ne!(status_of(&dir, &bead_id), "blocked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cheap band (P3, normal, loop-runnable): none-required — no side
    /// effect even in a single-family swarm (the loop verifies itself).
    #[test]
    fn cheap_band_is_none_required() {
        let _g = crate::config::lock_env();
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("vd-cheap-{}", std::process::id()));
        let bead_id = scratch_vd(&dir, "vd-cheap", "3");
        write_registry("[models.\"deepseek/deepseek-v4\"]\n[models.\"deepseek/deepseek-v3\"]\n");
        let out = crate::cmd_internal_verify_dispatch(
            &cfg,
            "vd-test",
            &bead_id,
            crate::VerifyDispatchOpts {
                worker_model: Some("deepseek-v4".to_string()),
                candidates: Some("deepseek-v4,deepseek-v3".to_string()),
                blast: "normal".to_string(),
                ac: "checkable".to_string(),
                dir: Some(dir.clone()),
            },
        );
        assert!(out.is_ok(), "{out:?}");
        assert_ne!(status_of(&dir, &bead_id), "blocked");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod loop_run_verify_tests {
    use super::*;

    fn scratch_lrv(dir: &std::path::Path, slug: &str, verify: &str) -> String {
        crate::br::scratch_bead(
            dir,
            "workflow:\n  transitions:\n    open:\n      - closed\n    closed:\n      - closed\n  gates:\n    open -> closed:\n      require_all:\n        - command-verified\n    closed -> closed:\n      require_all:\n        - command-verified\n",
            "lrv",
            slug,
            "3",
            Some(verify),
            None,
            None,
        )
    }

    fn commit_and_sha(dir: &std::path::Path, bead_id: &str) -> String {
        std::fs::write(dir.join("work.txt"), "work\n").unwrap();
        let add = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(add.status.success());
        let cm = std::process::Command::new("git")
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                &format!("feat: lrv ({bead_id})"),
            ])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(cm.status.success());
        let log = std::process::Command::new("git")
            .args([
                "log",
                "--all",
                "-1",
                &format!("--grep={bead_id}"),
                "--format=%H",
            ])
            .current_dir(dir)
            .output()
            .unwrap();
        String::from_utf8_lossy(&log.stdout).trim().to_string()
    }

    fn status_of(dir: &std::path::Path, bead_id: &str) -> String {
        crate::br::bead_status(dir, bead_id)
    }

    fn no_mail_cfg() -> Config {
        Config {
            mail: Some(crate::config::MailSections {
                toron: Some(crate::config::MailServerConfig {
                    url: Some("http://127.0.0.1:1".to_string()),
                    token: None,
                }),
            }),
            ..Default::default()
        }
    }

    /// Fixture: cheap P3 loop-runnable VERIFY -> the loop executes it,
    /// records command-verified (machine receipt + gate row), the ledger is
    /// legal and the bead STAYS closed (never reopened).
    #[test]
    fn cheap_p3_loop_runnable_records_and_stays_closed() {
        let _lock = crate::verdict::lock_loop_reporter();
        crate::verdict::test_reset_loop_reporter();
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("lrv-ok-{}", std::process::id()));
        let bead_id = scratch_lrv(&dir, "lrv-ok", "test -f work.txt");
        let sha = commit_and_sha(&dir, &bead_id);
        crate::br::run_br(
            Some(&dir),
            [
                "close",
                &bead_id,
                "--reason",
                "worker done",
                "--commit-sha",
                &sha,
            ],
        )
        .unwrap();
        let o = crate::loop_run_verify_exec(&cfg, "lrv-test", &bead_id, &sha, Some(60), Some(&dir))
            .unwrap();
        assert_eq!(o.band, "loop-runnable");
        assert_eq!(o.exit, 0, "verify output: {}", o.output_tail);
        assert!(!o.skipped);
        crate::verdict::arm_loop_reporter();
        let receipt =
            crate::verdict::write_command_verified_receipt(&o.dir, &bead_id, &sha, &o.output_tail)
                .unwrap();
        assert_eq!(
            receipt,
            dir.join(format!(
                ".flywheel/receipts/{bead_id}-{sha}.command-verified.txt"
            ))
        );
        let v = crate::verdict::Verdict {
            kind: crate::verdict::VerdictKind::CommandVerified,
            bead_id: bead_id.clone(),
            sha: sha.clone(),
            at: chrono::Utc::now(),
            actor: crate::verdict::AgentName::new("flywheel-toron-loop1"),
            receipt_path: receipt,
            verify_cmd: o.verify,
        };
        crate::verdict::report_pass(&o.dir, &v).unwrap();
        let bead = crate::verdict::bead_from_br_show(&dir, &bead_id).unwrap();
        assert!(
            crate::verdict::ledger_check(&o.dir, &bead, crate::verdict::AcShape::Checkable, &sha)
                .unwrap()
        );
        assert_eq!(status_of(&dir, &bead_id), "closed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// VERIFY failing -> failed-head marker written; the same head is NEVER
    /// re-verified (second invocation skips).
    #[test]
    fn failing_verify_marks_head_and_never_rereuns_same_head() {
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("lrv-fail-{}", std::process::id()));
        let bead_id = scratch_lrv(&dir, "lrv-fail", "test -f does-not-exist.txt");
        let sha = commit_and_sha(&dir, &bead_id);
        let o1 =
            crate::loop_run_verify_exec(&cfg, "lrv-test", &bead_id, &sha, Some(60), Some(&dir))
                .unwrap();
        assert_eq!(o1.band, "loop-runnable");
        assert_ne!(o1.exit, 0, "the fixture VERIFY must fail");
        let marker = dir.join(format!(".flywheel/loop-run-verify-failed/{bead_id}-{sha}"));
        assert!(marker.exists(), "failed-head marker must be written");
        let o2 =
            crate::loop_run_verify_exec(&cfg, "lrv-test", &bead_id, &sha, Some(60), Some(&dir))
                .unwrap();
        assert!(
            o2.skipped,
            "never re-verify the same broken head — second run must skip"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whole-workspace / heavy VERIFY is NOT the loop-runnable band.
    #[test]
    fn heavy_verify_is_non_runnable_band() {
        let cfg = no_mail_cfg();
        let dir = std::env::temp_dir().join(format!("lrv-heavy-{}", std::process::id()));
        let bead_id = scratch_lrv(&dir, "lrv-heavy", "cargo test --workspace");
        let sha = commit_and_sha(&dir, &bead_id);
        let o = crate::loop_run_verify_exec(&cfg, "lrv-test", &bead_id, &sha, Some(60), Some(&dir))
            .unwrap();
        assert_eq!(o.band, "non-runnable");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Durable swarm loop — the monitoring orchestrator as a Resonate workflow.
//!
//! Design (resonate-philosophy): the loop is sequential happy-path code with
//! checkpoints. Every `ctx.run` (poll, send) and `ctx.sleep` is a durable save
//! point — if the flywheel process crashes, the workflow resumes from the last
//! checkpoint on restart with the same invocation id. The queue and the agent
//! states are deterministic across replays because every effect is checkpointed.
//!
//! Leaves shell out to the flywheel binary itself (`__poll` / `__send` hidden
//! subcommands) so the blocking herdr socket client never runs on the async
//! executor and each side effect is a single crash-safe process.

use std::process::Command;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use resonate_sdk::function;
use resonate_sdk::prelude::*;

use crate::config::Config;

/// Input to the durable loop workflow. Immutable per invocation — the workflow
/// id is `swarm-loop:<project>`, so re-running with the same params resumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopParams {
    pub project: String,
    pub to: String,
    pub interval_secs: u64,
    pub queue: Vec<String>,
    pub nudge: Option<String>,
    pub on_blocked: String,
    /// max poll iterations before failing (derived from --timeout-ms;
    /// deterministic across replays, unlike wall-clock deadlines)
    pub max_iterations: Option<u64>,
    /// bead-driven mode: source work from `br ready` instead of a static queue
    pub work: bool,
    /// send every prompt via `__send --confirm` (landing confirmation).
    /// Default true — reliability is the default, --no-confirm is the escape
    /// hatch. `#[serde(default)]` keeps pre-confirm saved params loadable.
    #[serde(default = "default_true")]
    pub confirm_sends: bool,
    /// confirm timeout in ms; None → leaf default 45s.
    #[serde(default)]
    pub confirm_timeout_ms: Option<u64>,
}

fn default_true() -> bool {
    true
}

/// One polled agent state: (name, status string).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub name: String,
    pub status: String,
}

/// One ready bead from `br ready --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyBead {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyArgs {
    pub project: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollKindArgs {
    pub project: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadArgs {
    pub project: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RespawnArgs {
    pub project: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollArgs {
    pub project: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendArgs {
    pub project: String,
    pub name: String,
    pub text: String,
    /// wait for the prompt to land (__send --confirm); default true in the loop
    pub confirm: bool,
    /// confirm timeout in ms; None → leaf default 45s
    pub confirm_timeout_ms: Option<u64>,
}

/// Build the argv for the `__send` leaf invocation.
///
/// Pure (testable without spawning a process). `--confirm` is passed without
/// a value when the timeout is None — the leaf's clap layer applies its own
/// default (45s), keeping one source of truth for the default.
pub fn send_prompt_argv(args: &SendArgs) -> Vec<String> {
    let mut argv = vec![
        "__send".to_string(),
        args.project.clone(),
        args.name.clone(),
        args.text.clone(),
    ];
    if args.confirm {
        argv.push("--confirm".to_string());
        if let Some(ms) = args.confirm_timeout_ms {
            argv.push(ms.to_string());
        }
    }
    argv
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyArgs {
    pub project: String,
    pub name: String,
}

fn map_err(e: anyhow::Error) -> Error {
    Error::Application {
        message: e.to_string(),
    }
}

fn self_exe() -> anyhow::Result<std::path::PathBuf> {
    std::env::current_exe().map_err(anyhow::Error::from)
}

/// Poll the swarm via the flywheel binary itself. Checkpointed.
#[function]
async fn poll_swarm(args: PollArgs) -> Result<Vec<AgentState>> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args(["__poll", &args.project, &args.to])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__poll spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__poll failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|e| map_err(anyhow::anyhow!("__poll bad JSON: {e}: {raw}")))?;
    let pairs = v
        .get("agents")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| map_err(anyhow::anyhow!("__poll missing agents: {raw}")))?;
    let mut out_states = Vec::with_capacity(pairs.len());
    for p in pairs {
        let arr = p
            .as_array()
            .ok_or_else(|| map_err(anyhow::anyhow!("__poll bad entry: {p}")))?;
        out_states.push(AgentState {
            name: arr
                .first()
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            status: arr
                .get(1)
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
        });
    }
    Ok(out_states)
}

/// List ready beads via the flywheel binary (`br ready --json` in the project).
/// Checkpointed.
#[function]
async fn poll_ready(args: ReadyArgs) -> Result<Vec<ReadyBead>> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args(["__ready", &args.project])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__ready spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__ready failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|e| map_err(anyhow::anyhow!("__ready bad JSON: {e}: {raw}")))?;
    let beads = v
        .get("beads")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| map_err(anyhow::anyhow!("__ready missing beads: {raw}")))?;
    let mut out_beads = Vec::with_capacity(beads.len());
    for b in beads {
        out_beads.push(ReadyBead {
            id: b.get("id").and_then(serde_json::Value::as_str).unwrap_or("").to_string(),
            title: b.get("title").and_then(serde_json::Value::as_str).unwrap_or("").to_string(),
        });
    }
    Ok(out_beads)
}

/// The prompt given to an agent for a ready bead: self-contained + lifecycle.
pub fn bead_prompt(id: &str, title: &str) -> String {
    let template = r#"Execute bead {id}: {title}. Follow the bead lifecycle: claim (`br update {id} --claim --actor "$AGENT_NAME" --json`), reserve your files via agent-mail, announce, execute one narrow slice, verify acceptance criteria, close with an honest reason, `br sync --flush-only` and commit `.beads/`. If your session context usage exceeds about 80%, do NOT start new work: finish and sync the current bead, then reply exactly CONTEXT_LIMIT so the orchestrator can respawn you. If {id} is already claimed, pick the next ready bead from `br ready --json` or report back."#;
    template.replace("{id}", id).replace("{title}", title)
}

/// Prompt to resume an in-progress bead after an interruption.
pub fn resume_prompt(id: &str, title: &str) -> String {
    format!(
        "Resume bead {id}: {title}. It is still in progress — continue it: re-check your work, finish, verify acceptance criteria, close with an honest reason, `br sync --flush-only` and commit `.beads/`. If your context is exhausted, reply exactly CONTEXT_LIMIT."
    )
}

/// True when an agent's recent output carries the context-exhaustion marker.
pub fn should_respawn(output: &str) -> bool {
    output.contains("CONTEXT_LIMIT")
}

/// In-progress beads via the flywheel binary (`br list --status in_progress`).
/// Checkpointed.
#[function]
async fn poll_inprogress(args: ReadyArgs) -> Result<Vec<ReadyBead>> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args(["__inprogress", &args.project])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__inprogress spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__inprogress failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|e| map_err(anyhow::anyhow!("__inprogress bad JSON: {e}: {raw}")))?;
    let beads = v
        .get("beads")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| map_err(anyhow::anyhow!("__inprogress missing beads: {raw}")))?;
    Ok(beads
        .iter()
        .map(|b| ReadyBead {
            id: b.get("id").and_then(serde_json::Value::as_str).unwrap_or("").to_string(),
            title: b.get("title").and_then(serde_json::Value::as_str).unwrap_or("").to_string(),
        })
        .collect())
}

/// Agent kind (opencode/pi/grok) for respawn. Checkpointed.
#[function]
async fn poll_kind(args: PollKindArgs) -> Result<String> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args(["__kind", &args.project, &args.name])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__kind spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__kind failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Recent agent output via the flywheel binary. Checkpointed.
#[function]
async fn read_output(args: ReadArgs) -> Result<String> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args(["__read", &args.project, &args.name])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__read spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__read failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Replace an exhausted agent with a fresh instance in a new pane.
/// Checkpointed.
#[function]
async fn respawn_agent(args: RespawnArgs) -> Result<()> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args(["__respawn", &args.project, &args.name, &args.kind])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__respawn spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__respawn failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Outcome of one confirmed `__send` invocation, parsed from the leaf JSON.
/// `landed` is None on the legacy (non-confirm) path where no verdict exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendOutcome {
    pub ok: bool,
    pub name: String,
    #[serde(default)]
    pub landed: Option<bool>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Send one prompt via the flywheel binary. Checkpointed side effect.
/// With confirm, returns the parsed landing verdict; the legacy path returns
/// an outcome with `landed: None`.
#[function]
async fn send_prompt(args: SendArgs) -> Result<SendOutcome> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args(send_prompt_argv(&args))
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__send spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__send failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let outcome: SendOutcome = serde_json::from_str(raw.trim())
        .map_err(|e| map_err(anyhow::anyhow!("__send bad JSON: {e}: {raw}")))?;
    let _ = crate::state::log_event(
        &args.project,
        crate::state::EventKind::Send,
        serde_json::json!({
            "to": args.name,
            "confirm": args.confirm,
            "landed": outcome.landed,
            "status": outcome.status,
            "reason": outcome.reason,
            "source": "loop",
        }),
    );
    Ok(outcome)
}

/// Notify a blocked agent via agent-mail. Checkpointed side effect.
#[function]
async fn notify_blocked(args: NotifyArgs) -> Result<()> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args(["__mail-blocked", &args.project, &args.name])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__mail-blocked spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__mail-blocked failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// Pure planning: given current states, decide actions and drain the queue.
/// Deterministic dispatch plan for one poll: working agents wait, blocked
/// agents report, idle/done agents get the next queue item (or a nudge, or
/// wait). `unknown` is NOT dispatchable by design: herdr cannot classify its
/// lifecycle confidently — a prompt could land in a half-dead session and be
/// silently lost. Unknown agents get `Wait` and never consume the queue.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopAction {
    Send { name: String, text: String },
    Blocked { name: String },
    Nudge { name: String },
    Wait,
}

pub fn plan_actions(states: &[AgentState], queue: &mut Vec<String>, nudge: Option<&str>) -> Vec<LoopAction> {
    let mut actions = Vec::new();
    for s in states {
        match s.status.as_str() {
            "working" => actions.push(LoopAction::Wait),
            "blocked" => actions.push(LoopAction::Blocked { name: s.name.clone() }),
            // never dispatch to an unclassified agent — queue stays intact
            "unknown" => actions.push(LoopAction::Wait),
            _ => {
                // idle / done → feed work or nudge
                if !queue.is_empty() {
                    let text = queue.remove(0); // FIFO: stages in declared order
                    actions.push(LoopAction::Send {
                        name: s.name.clone(),
                        text,
                    });
                } else if nudge.is_some() {
                    actions.push(LoopAction::Nudge { name: s.name.clone() });
                } else {
                    actions.push(LoopAction::Wait);
                }
            }
        }
    }
    actions
}

/// Completion guard: everyone idle/done. `unknown` keeps the loop alive — an
/// unclassified agent may still be doing the last delegated task, so the loop
/// stays on the active cadence.
pub fn all_settled(states: &[AgentState]) -> bool {
    states
        .iter()
        .all(|s| matches!(s.status.as_str(), "idle" | "done"))
}

/// The durable monitoring workflow. Runs until the queue drains and the swarm
/// settles, or a blocked agent stops it (policy).
#[function]
async fn swarm_loop(ctx: &Context, params: LoopParams) -> Result<String> {
    let mut queue = params.queue.clone();
    let interval = Duration::from_secs(params.interval_secs);
    let interval_min = Duration::from_secs(5);
    let mut iteration = 0u64;
    // bead-driven mode tracks which beads were already handed out
    let mut assigned: Vec<String> = Vec::new();

    loop {
        iteration += 1;
        if let Some(max) = params.max_iterations {
            if iteration > max {
                return Err(map_err(anyhow::anyhow!(
                    "loop timed out after {max} polls (queue remaining: {})",
                    queue.len()
                )));
            }
        }

        let states: Vec<AgentState> = ctx
            .run(
                poll_swarm,
                PollArgs {
                    project: params.project.clone(),
                    to: params.to.clone(),
                },
            )
            .await?;

        // bead-driven mode: refresh the instruction queue from the tracker each
        // poll; the assigned-set is deterministic across replays (derives from
        // checkpointed poll results)
        if params.work {
            // interrupted work first: an in-progress bead's agent went idle —
            // resume it before handing out fresh beads
            let inprog: Vec<ReadyBead> = ctx
                .run(
                    poll_inprogress,
                    ReadyArgs {
                        project: params.project.clone(),
                    },
                )
                .await?;
            let to_resume: Vec<&ReadyBead> = inprog
                .iter()
                .filter(|b| !assigned.contains(&b.id))
                .collect();
            for bead in to_resume {
                queue.push(resume_prompt(&bead.id, &bead.title));
                assigned.push(bead.id.clone());
            }

            let ready: Vec<ReadyBead> = ctx
                .run(
                    poll_ready,
                    ReadyArgs {
                        project: params.project.clone(),
                    },
                )
                .await?;
            let to_assign: Vec<&ReadyBead> = ready
                .iter()
                .filter(|b| !assigned.contains(&b.id))
                .collect();
            for bead in to_assign {
                queue.push(bead_prompt(&bead.id, &bead.title));
                assigned.push(bead.id.clone());
            }
        }

        let mut blocked_names: Vec<String> = Vec::new();
        let actions = plan_actions(&states, &mut queue, params.nudge.as_deref());
        for action in &actions {
            if let LoopAction::Blocked { name } = action {
                blocked_names.push(name.clone());
            }
        }

        // with on_blocked=stop, the loop must not dispatch any more work this
        // iteration — check the blocker before any side effect fires
        if !blocked_names.is_empty() && params.on_blocked == "stop" {
            return Ok(format!(
                "stopped: blocked agents {}",
                blocked_names.join(", ")
            ));
        }

        let mut any_work = false;
        for action in &actions {
            match action {
                LoopAction::Send { name, text } => {
                    // context guard: if the idle agent is context-exhausted,
                    // respawn a fresh instance instead of delegating to it
                    let output = ctx
                        .run(
                            read_output,
                            ReadArgs {
                                project: params.project.clone(),
                                name: name.clone(),
                            },
                        )
                        .await?;
                    if should_respawn(&output) {
                        let kind = ctx
                            .run(
                                poll_kind,
                                PollKindArgs {
                                    project: params.project.clone(),
                                    name: name.clone(),
                                },
                            )
                            .await?;
                        ctx.run(
                            respawn_agent,
                            RespawnArgs {
                                project: params.project.clone(),
                                name: name.clone(),
                                kind,
                            },
                        )
                        .await?;
                        // the fresh agent inherits the queue: put the prompt back
                        queue.insert(0, text.clone());
                        continue;
                    }
                    ctx.run(
                        send_prompt,
                        SendArgs {
                            project: params.project.clone(),
                            name: name.clone(),
                            text: text.clone(),
                            confirm: params.confirm_sends,
                            confirm_timeout_ms: params.confirm_timeout_ms,
                        },
                    )
                    .await?;
                    any_work = true;
                }
                LoopAction::Blocked { name } => {
                    if params.on_blocked == "mail" {
                        ctx.run(
                            notify_blocked,
                            NotifyArgs {
                                project: params.project.clone(),
                                name: name.clone(),
                            },
                        )
                        .await?;
                    }
                }
                LoopAction::Nudge { name } => {
                    if let Some(n) = &params.nudge {
                        ctx.run(
                            send_prompt,
                            SendArgs {
                                project: params.project.clone(),
                                name: name.clone(),
                                text: n.clone(),
                                confirm: params.confirm_sends,
                                confirm_timeout_ms: params.confirm_timeout_ms,
                            },
                        )
                        .await?;
                        any_work = true;
                    }
                }
                LoopAction::Wait => {}
            }
        }

        // work mode completion guard: a stranded in-progress bead (interrupted
        // agent) must keep the loop alive — re-prompt it next poll.
        let inprogress_empty = if params.work {
            let inprog: Vec<ReadyBead> = ctx
                .run(
                    poll_inprogress,
                    ReadyArgs {
                        project: params.project.clone(),
                    },
                )
                .await?;
            inprog.is_empty()
        } else {
            true
        };

        // settled = no work was dispatched this iteration AND everyone idle/done.
        // The states vector is pre-action, so an agent that just received the
        // last queue item is still "idle" here — without the any_work guard the
        // loop would declare "complete" while the swarm is mid-work.
        let all_settled = all_settled(&states);

        if queue.is_empty() && all_settled && !any_work && inprogress_empty {
            return Ok("complete".to_string());
        }

        // durable pause — the worker may exit here; the server resumes us
        ctx.sleep(interval.max(interval_min)).await?;
    }
}

/// Register the durable functions and start (or resume) the loop invocation.
/// Returns the workflow result; the process must stay alive while awaiting.
pub async fn run_durable_loop(cfg: &Config, params: LoopParams) -> anyhow::Result<String> {
    let url = cfg
        .resonate_url()
        .unwrap_or_else(|| "http://localhost:8001".to_string());
    let resonate = Resonate::new(ResonateConfig {
        url: Some(url.clone()),
        ..Default::default()
    });

    resonate
        .register(swarm_loop)
        .map_err(|e| anyhow::anyhow!("register swarm_loop: {e}"))?;
    resonate
        .register(poll_swarm)
        .map_err(|e| anyhow::anyhow!("register poll_swarm: {e}"))?;
    resonate
        .register(send_prompt)
        .map_err(|e| anyhow::anyhow!("register send_prompt: {e}"))?;
    resonate
        .register(notify_blocked)
        .map_err(|e| anyhow::anyhow!("register notify_blocked: {e}"))?;
    resonate
        .register(poll_ready)
        .map_err(|e| anyhow::anyhow!("register poll_ready: {e}"))?;
    resonate
        .register(poll_inprogress)
        .map_err(|e| anyhow::anyhow!("register poll_inprogress: {e}"))?;
    resonate
        .register(read_output)
        .map_err(|e| anyhow::anyhow!("register read_output: {e}"))?;
    resonate
        .register(poll_kind)
        .map_err(|e| anyhow::anyhow!("register poll_kind: {e}"))?;
    resonate
        .register(respawn_agent)
        .map_err(|e| anyhow::anyhow!("register respawn_agent: {e}"))?;

    // Resume semantics: the persisted invocation id is reused while its promise
    // is still pending (crash resume); once it settles (complete/blocked/failed),
    // a fresh id starts a new run. SDK 0.6.0 does not support version-based
    // re-invocation, so ids must change.
    let id = match crate::state::load_active_loop(&params.project)? {
        Some(al) if promise_pending(&resonate, &al.id).await? => {
            eprintln!("flywheel: resuming existing loop {}", dim_id(&al.id));
            al.id
        }
        _ => {
            let fresh = format!(
                "swarm-loop:{}:{}",
                params.project,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            crate::state::save_active_loop(
                &params.project,
                &crate::state::ActiveLoop {
                    id: fresh.clone(),
                    params: serde_json::to_value(&params)?,
                    created: crate::state::now_iso(),
                },
            )?;
            fresh
        }
    };

    let task = resonate.run(&id, swarm_loop, params.clone());
    let result: String = task
        .await
        .map_err(|e| anyhow::anyhow!("durable loop invocation failed: {e}"))?;

    let _ = resonate.stop().await;
    Ok(result)
}

fn dim_id(id: &str) -> String {
    crate::output::dim(id)
}

/// True when the promise exists and is still pending.
/// Only a server 404 (no record) counts as "not pending"; genuine query
/// failures propagate — treating a transient Resonate outage as "no record"
/// would start a second concurrent loop and duplicate sends.
async fn promise_pending(resonate: &Resonate, id: &str) -> anyhow::Result<bool> {
    match resonate.promises.get(id).await {
        Ok(rec) => Ok(matches!(rec.state, PromiseState::Pending)),
        Err(Error::ServerError { code: 404, .. }) => Ok(false),
        Err(e) => Err(anyhow::anyhow!("query loop promise {id}: {e}")),
    }
}

/// Query the current state of a durable loop invocation.
pub async fn loop_status(cfg: &Config, project: &str) -> anyhow::Result<serde_json::Value> {
    let url = cfg
        .resonate_url()
        .unwrap_or_else(|| "http://localhost:8001".to_string());
    let resonate = Resonate::new(ResonateConfig {
        url: Some(url),
        ..Default::default()
    });
    let Some(al) = crate::state::load_active_loop(project)? else {
        return Ok(serde_json::json!({
            "project": project,
            "loop": "none recorded — run: flywheel loop <project>"
        }));
    };
    let rec = resonate
        .promises
        .get(&al.id)
        .await
        .map_err(|e| anyhow::anyhow!("query {}: {e}", al.id))?;
    let _ = resonate.stop().await;
    Ok(serde_json::json!({
        "project": project,
        "id": al.id,
        "state": format!("{:?}", rec.state),
        "value": rec.value,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(name: &str, status: &str) -> AgentState {
        AgentState {
            name: name.to_string(),
            status: status.to_string(),
        }
    }

    #[test]
    fn idle_agent_pops_queue() {
        let states = vec![st("a", "idle")];
        let mut q = vec!["p1".to_string(), "p6".to_string()];
        let actions = plan_actions(&states, &mut q, None);
        assert_eq!(q.len(), 1);
        assert!(matches!(&actions[0], LoopAction::Send { name, text } if name == "a" && text == "p1"));
    }

    #[test]
    fn queue_drains_across_idle_agents_in_order() {
        let states = vec![st("a", "idle"), st("b", "done")];
        let mut q = vec!["p1".to_string(), "p6".to_string()];
        let actions = plan_actions(&states, &mut q, None);
        assert!(q.is_empty());
        match (&actions[0], &actions[1]) {
            (LoopAction::Send { name: n1, text: t1 }, LoopAction::Send { name: n2, text: t2 }) => {
                assert_eq!(n1, "a");
                assert_eq!(t1, "p1");
                assert_eq!(n2, "b");
                assert_eq!(t2, "p6");
            }
            other => panic!("expected two sends, got {other:?}"),
        }
    }

    #[test]
    fn working_agent_gets_nothing() {
        let states = vec![st("a", "working")];
        let mut q = vec!["p1".to_string()];
        let actions = plan_actions(&states, &mut q, None);
        assert_eq!(q.len(), 1);
        assert!(matches!(actions[0], LoopAction::Wait));
    }

    #[test]
    fn blocked_agent_reports_and_keeps_queue() {
        let states = vec![st("a", "blocked")];
        let mut q = vec!["p1".to_string()];
        let actions = plan_actions(&states, &mut q, None);
        assert!(matches!(&actions[0], LoopAction::Blocked { name } if name == "a"));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn empty_queue_with_nudge_nudges() {
        let states = vec![st("a", "idle")];
        let mut q: Vec<String> = vec![];
        let actions = plan_actions(&states, &mut q, Some("proceed"));
        assert!(matches!(&actions[0], LoopAction::Nudge { name } if name == "a"));
    }

    #[test]
    fn empty_queue_no_nudge_waits() {
        let states = vec![st("a", "done")];
        let mut q: Vec<String> = vec![];
        let actions = plan_actions(&states, &mut q, None);
        assert!(matches!(actions[0], LoopAction::Wait));
    }

    // ---- unknown agents: never dispatch, never nudge (bd-unknown-skip-dispatch) ----

    #[test]
    fn unknown_agent_keeps_queue() {
        let states = vec![st("a", "unknown")];
        let mut q = vec!["p1".to_string()];
        let actions = plan_actions(&states, &mut q, None);
        assert_eq!(q.len(), 1); // queue intact — unknown must never consume items
        assert!(matches!(actions[0], LoopAction::Wait));
    }

    #[test]
    fn unknown_with_nudge_no_send() {
        let states = vec![st("a", "unknown")];
        let mut q: Vec<String> = vec![];
        let actions = plan_actions(&states, &mut q, Some("proceed"));
        assert!(matches!(actions[0], LoopAction::Wait)); // no Nudge, no Send
    }

    #[test]
    fn unknown_not_settled() {
        // an unknown agent keeps the loop alive: the completion guard only
        // matches idle|done
        let states = vec![st("a", "unknown")];
        assert!(!all_settled(&states));
        let settled = vec![st("a", "idle"), st("b", "done")];
        assert!(all_settled(&settled));
    }

    #[test]
    fn resume_prompt_mentions_continuation() {
        let p = resume_prompt("bd-7", "finish the parser");
        assert!(p.contains("bd-7"));
        assert!(p.contains("Resume"));
        assert!(p.contains("CONTEXT_LIMIT"));
    }

    #[test]
    fn context_marker_detection() {
        assert!(should_respawn("...CONTEXT_LIMIT"));
        assert!(should_respawn("CONTEXT_LIMIT\nall done"));
        assert!(!should_respawn("all done"));
    }

    #[test]
    fn bead_prompt_is_self_contained() {
        let p = bead_prompt("bd-12", "fix auth flow");
        assert!(p.contains("bd-12"));
        assert!(p.contains("--claim"));
        assert!(p.contains("br sync --flush-only"));
    }

    #[test]
    fn params_serde_roundtrip() {
        let p = LoopParams {
            project: "notetoself".into(),
            to: "all".into(),
            interval_secs: 300,
            queue: vec!["p1".into()],
            nudge: None,
            on_blocked: "continue".into(),
            max_iterations: Some(10),
            work: false,
            confirm_sends: true,
            confirm_timeout_ms: Some(45_000),
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: LoopParams = serde_json::from_value(v).unwrap();
        assert_eq!(back.project, "notetoself");
        assert_eq!(back.interval_secs, 300);
        assert_eq!(back.max_iterations, Some(10));
        assert!(back.confirm_sends);
        assert_eq!(back.confirm_timeout_ms, Some(45_000));
    }

    #[test]
    fn params_confirm_defaults_true_when_absent() {
        // pre-confirm saved params (loops.json) must still load: confirm_sends
        // defaults to true, timeout to None (leaf default 45s).
        let v = serde_json::json!({
            "project": "legacy",
            "to": "all",
            "interval_secs": 300,
            "queue": [],
            "nudge": null,
            "on_blocked": "continue",
            "max_iterations": null,
            "work": false,
        });
        let p: LoopParams = serde_json::from_value(v).unwrap();
        assert!(p.confirm_sends, "confirm_sends must default true for legacy params");
        assert_eq!(p.confirm_timeout_ms, None);
    }

    #[test]
    fn send_args_serde_roundtrip_with_confirm() {
        let a = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: true,
            confirm_timeout_ms: Some(45_000),
        };
        let v = serde_json::to_value(&a).unwrap();
        let back: SendArgs = serde_json::from_value(v).unwrap();
        assert!(back.confirm);
        assert_eq!(back.confirm_timeout_ms, Some(45_000));
    }

    #[test]
    fn send_prompt_argv_confirm_flag() {
        // confirm + explicit timeout → `--confirm 45000`
        let args = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: true,
            confirm_timeout_ms: Some(45_000),
        };
        assert_eq!(
            send_prompt_argv(&args),
            vec!["__send", "p", "a", "t", "--confirm", "45000"]
        );
        // confirm + no timeout → bare `--confirm` (leaf applies its 45s default)
        let args_no_ms = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: true,
            confirm_timeout_ms: None,
        };
        assert_eq!(
            send_prompt_argv(&args_no_ms),
            vec!["__send", "p", "a", "t", "--confirm"]
        );
        // no confirm → legacy argv, byte-identical to the old builder
        let args_legacy = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: false,
            confirm_timeout_ms: None,
        };
        assert_eq!(
            send_prompt_argv(&args_legacy),
            vec!["__send", "p", "a", "t"]
        );
    }

    #[test]
    fn send_outcome_parses_confirm_and_legacy_json() {
        // confirm path: verdict present
        let v: SendOutcome = serde_json::from_str(
            r#"{"ok": true, "name": "a", "landed": true, "status": "working", "reason": "landed"}"#,
        )
        .unwrap();
        assert_eq!(v.landed, Some(true));
        assert_eq!(v.status.as_deref(), Some("working"));
        assert_eq!(v.reason.as_deref(), Some("landed"));

        // legacy path: no verdict fields → None defaults
        let legacy: SendOutcome =
            serde_json::from_str(r#"{"ok": true, "name": "a"}"#).unwrap();
        assert_eq!(legacy.landed, None);
        assert_eq!(legacy.reason, None);
    }
}

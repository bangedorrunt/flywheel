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

/// Send one prompt via the flywheel binary. Checkpointed side effect.
#[function]
async fn send_prompt(args: SendArgs) -> Result<()> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args(["__send", &args.project, &args.name, &args.text])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__send spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__send failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
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
/// Deterministic across replays (states are checkpointed per iteration).
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
            _ => {
                // idle / done / unknown → feed work or nudge
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
        let all_settled = states
            .iter()
            .all(|s| matches!(s.status.as_str(), "idle" | "done"));

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
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: LoopParams = serde_json::from_value(v).unwrap();
        assert_eq!(back.project, "notetoself");
        assert_eq!(back.interval_secs, 300);
        assert_eq!(back.max_iterations, Some(10));
    }
}

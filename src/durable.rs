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
    /// settled/base cadence: poll interval when nobody is working (default 300)
    pub interval_secs: u64,
    /// fast cadence while anyone is working/blocked/unknown or right after a
    /// dispatch (default 30s — the pre-config balance; floor 5s lives in
    /// `next_interval_secs`). `#[serde(default = "default_active_interval")]`
    /// keeps pre-yna saved params loadable.
    #[serde(default = "default_active_interval")]
    pub active_interval_secs: u64,
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

fn default_active_interval() -> u64 {
    30
}

/// One polled agent state: (name, status string).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub name: String,
    pub status: String,
    /// herdr agent kind (`opencode`/`pi`/...). Learned from polls; used to
    /// respawn an agent whose herdr record has been dropped (the escalation
    /// path cannot poll_kind a gone record). `#[serde(default)]` so
    /// checkpoints written before this field existed still resume.
    #[serde(default)]
    pub kind: String,
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
    /// Why the respawn happened — logged with the event (`stalled`,
    /// `context_limit`, `error:<...>`, ...).
    pub reason: String,
    /// Consecutive send failures for this agent, logged with the event.
    pub attempts: u64,
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
        out_states.push(parse_poll_entry(p).map_err(map_err)?);
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
/// The state-file contract (bd-prompt-state-file) tells the agent to write
/// `.flywheel/agent-state/{AGENT_NAME}.json` when context is high; the
/// `{AGENT_NAME}` placeholder is substituted with the receiving agent's herdr
/// name at dispatch time (plan_actions) so the agent never guesses the
/// filename. `context_pct` < 80 is ignored by the READER (bd-read-state-
/// fallback) — the threshold lives there, not here.
pub fn bead_prompt(id: &str, title: &str) -> String {
    let template = r#"Execute bead {id}: {title}. Follow the bead lifecycle: claim (`br update {id} --claim --actor "$AGENT_NAME" --json`), reserve your files via agent-mail, announce, execute one narrow slice, verify acceptance criteria, close with an honest reason, `br sync --flush-only` and commit `.beads/`. If your session context usage exceeds about 80%: write `{"context_pct": <pct>, "ts": "<now RFC3339>"}` to `.flywheel/agent-state/{AGENT_NAME}.json` in the project root (best-effort, never block on it), then finish and sync the current bead, then reply exactly CONTEXT_LIMIT so the orchestrator can respawn you. If {id} is already claimed, pick the next ready bead from `br ready --json` or report back."#;
    template.replace("{id}", id).replace("{title}", title)
}

/// Prompt to resume an in-progress bead after an interruption. Carries the
/// same state-file contract (placeholder substituted at dispatch).
pub fn resume_prompt(id: &str, title: &str) -> String {
    format!(
        "Resume bead {id}: {title}. It is still in progress — continue it: re-check your work, finish, verify acceptance criteria, close with an honest reason, `br sync --flush-only` and commit `.beads/`. If your session context usage exceeds about 80%: write `{{\"context_pct\": <pct>, \"ts\": \"<now RFC3339>\"}}` to `.flywheel/agent-state/{{AGENT_NAME}}.json` in the project root (best-effort, never block on it), then finish and sync the current bead, then reply exactly CONTEXT_LIMIT so the orchestrator can respawn you."
    )
}

/// Exhaustion verdict from the two context channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextVerdict {
    /// Agent is at ~80%+ context — respawn before delegating more work.
    Exhausted,
    /// No exhaustion signal — safe to delegate.
    Healthy,
}

/// Combined context-exhaustion verdict: terminal marker OR a fresh state file
/// with `context_pct >= 80`. Stale files (older than `DEFAULT_MAX_AGE`), low
/// percentages, and absent state never exhaust — worst case Healthy (today's
/// semantics preserved when no agent writes files). The file channel wins
/// when the channels disagree: it cannot scroll away (alt-screen caveat).
///
/// Pure and deterministic for a fixed `now_unix` — the caller (the leaf)
/// evaluates it at run time so the verdict is checkpointed with the leaf
/// result (replay-safe).
pub fn context_verdict(
    terminal: &str,
    state: Option<&crate::agentstate::AgentStateFile>,
    now_unix: u64,
) -> ContextVerdict {
    if terminal.contains("CONTEXT_LIMIT") {
        return ContextVerdict::Exhausted;
    }
    if let Some(s) = state {
        if s.context_pct >= 80
            && crate::agentstate::is_fresh_at(&s.ts, now_unix, crate::agentstate::DEFAULT_MAX_AGE)
        {
            return ContextVerdict::Exhausted;
        }
    }
    ContextVerdict::Healthy
}

/// Parsed `__read` leaf output: `{"terminal": "...", "state": {...}|null}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadOutput {
    pub terminal: String,
    #[serde(default)]
    pub state: Option<crate::agentstate::AgentStateFile>,
}

/// Parse one `__read` JSON output into [`ReadOutput`]. A null or missing
/// `state` yields `None` (file absent/malformed — the leaf never errors on
/// the file channel).
pub fn parse_read_output(v: &serde_json::Value) -> anyhow::Result<ReadOutput> {
    Ok(ReadOutput {
        terminal: v
            .get("terminal")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        state: v
            .get("state")
            .filter(|s| !s.is_null())
            .and_then(|s| serde_json::from_value(s.clone()).ok()),
    })
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
async fn read_output(args: ReadArgs) -> Result<ContextVerdict> {
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
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(raw.trim())
        .map_err(|e| map_err(anyhow::anyhow!("__read bad JSON: {e}: {raw}")))?;
    let ro = parse_read_output(&v).map_err(map_err)?;
    // freshness evaluated HERE at run time so the verdict is checkpointed
    // with the leaf result — a replay cannot re-judge a stale file later
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(context_verdict(&ro.terminal, ro.state.as_ref(), now_unix))
}

/// Replace an exhausted agent with a fresh instance in a new pane.
/// Checkpointed. Logs a `Respawn` event (side effect inside the leaf so the
/// log entry is checkpointed with the call).
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
    let _ = crate::state::log_event(
        &args.project,
        crate::state::EventKind::Respawn,
        serde_json::json!({
            "name": args.name,
            "reason": args.reason,
            "attempts": args.attempts,
            "source": "loop",
        }),
    );
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
                    // bead/resume prompts carry a {AGENT_NAME} placeholder —
                    // substitute the receiving agent's herdr name so the
                    // state-file path is concrete (no-op for plain prompts)
                    actions.push(LoopAction::Send {
                        name: s.name.clone(),
                        text: text.replace("{AGENT_NAME}", &s.name),
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

/// Adaptive poll cadence: fast while the swarm is busy, slow when settled.
///
/// A dispatch this iteration (`dispatched`) forces the active cadence: the
/// recipient will be working during the sleep, but `states` is the PRE-action
/// poll — without this the loop would sleep `base_secs` right after handing
/// out work (the failure mode this function exists to fix). Otherwise, any
/// agent `working`, `blocked`, or `unknown` → `active_secs`; a fully
/// idle/done (or empty) swarm → `base_secs`. Clamped to >= 5s.
///
/// `unknown` counts as unsettled BY DESIGN: an unknown-heavy swarm is a swarm
/// needing repair (`bd-unknown-respawn`), and repairs must proceed on the
/// active cadence. An all-unknown swarm therefore polls at 30s rather than
/// 300s — acceptable, because `--timeout-ms` (max_iterations) still bounds the
/// run if it never converges.
///
/// Deterministic across replays: derives from the checkpointed states vector
/// and the (also deterministic) action plan.
///
/// Follow-up idea (do NOT build here): a future redesign could wake the loop
/// on herdr status events via a Resonate external promise — event-driven
/// instead of polled. Requires an event listener with its own durability.
pub fn next_interval_secs(
    states: &[AgentState],
    dispatched: bool,
    base_secs: u64,
    active_secs: u64,
) -> u64 {
    const INTERVAL_MIN: u64 = 5;
    let busy = dispatched
        || states
            .iter()
            .any(|s| matches!(s.status.as_str(), "working" | "blocked" | "unknown"));
    let chosen = if busy { active_secs } else { base_secs };
    chosen.max(INTERVAL_MIN)
}

/// Decision for one non-landing (or ambiguous) confirmed send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Escalation {
    /// Prompt did not land — respawn the agent and re-queue the prompt at the
    /// FRONT so the fresh instance inherits it next poll.
    RespawnAndRequeue,
    /// Prompt landed (or is ambiguous): nothing to escalate. A landed send
    /// clears the agent's failure history; a timeout leaves it untouched and
    /// is verified by the next poll.
    Accept,
    /// 3+ consecutive failures on the same agent: hard stop the loop. A
    /// genuinely broken environment must not burn the swarm forever.
    FailStop,
}

/// Pure verdict for one confirmed send outcome.
///
/// `landed: true` (incl. a `blocked` status — a valid landing the
/// `--on-blocked` policy owns) → [`Escalation::Accept`]; `landed: None`
/// (legacy fire-and-forget path) → [`Escalation::Accept`]; a `timeout` reason
/// is ambiguous (the prompt may have started) → [`Escalation::Accept`]
/// WITHOUT touching the failure counter; any other non-landing (stalled,
/// `error:<code>`) is a pane-health failure → respawn until `failures >= 3`
/// then [`Escalation::FailStop`]. Deterministic: derives from the checkpointed
/// leaf result and the (also deterministic) counter.
pub fn escalate(outcome: &SendOutcome, failures: u64) -> Escalation {
    match outcome.landed {
        Some(true) | None => Escalation::Accept,
        Some(false) => match outcome.reason.as_deref() {
            Some("timeout") => Escalation::Accept,
            _ => {
                if failures >= 3 {
                    Escalation::FailStop
                } else {
                    Escalation::RespawnAndRequeue
                }
            }
        },
    }
}

/// Pure send-outcome bookkeeping: run [`escalate`] AND update the per-agent
/// failure counter in one place.
///
/// Reset contract (documented, both events checkpointed): a landed send
/// removes the agent's counter; a timeout leaves it untouched (ambiguous —
/// verified by the next poll); a non-landing failure increments it; at the
/// cap the counter does not grow further ([`Escalation::FailStop`]).
pub fn handle_send_result(
    failures: &mut std::collections::HashMap<String, u64>,
    name: &str,
    outcome: &SendOutcome,
) -> Escalation {
    match escalate(outcome, failures.get(name).copied().unwrap_or(0)) {
        Escalation::Accept => {
            if outcome.landed == Some(true) {
                failures.remove(name);
            }
            Escalation::Accept
        }
        Escalation::RespawnAndRequeue => {
            *failures.entry(name.to_string()).or_insert(0) += 1;
            Escalation::RespawnAndRequeue
        }
        fail_stop => fail_stop,
    }
}

/// Pure proof-of-life reset: agents showing `working` or `blocked` in the
/// poll are receiving prompts — drop their failure history. Agents absent
/// from the poll (herdr dropped them) or `unknown` keep their counters.
pub fn clear_live_failures(
    failures: &mut std::collections::HashMap<String, u64>,
    states: &[AgentState],
) {
    for s in states {
        if matches!(s.status.as_str(), "working" | "blocked") {
            failures.remove(&s.name);
        }
    }
}

/// Learn agent kinds from a poll. Never removes entries: a dropped agent's
/// kind is exactly what the escalation path needs to respawn it, and a later
/// kind-less poll must not clobber it.
pub fn learn_kinds(kinds: &mut std::collections::HashMap<String, String>, states: &[AgentState]) {
    for s in states {
        if !s.kind.is_empty() {
            kinds.insert(s.name.clone(), s.kind.clone());
        }
    }
}

/// Pick the kind for a respawn: the kind learned from polls (authoritative —
/// a dropped agent's record cannot be re-queried), else the caller falls back
/// to the `poll_kind` leaf.
pub fn respawn_kind(kinds: &std::collections::HashMap<String, String>, name: &str) -> Option<String> {
    kinds.get(name).cloned()
}

/// Parse one `__poll` entry: `[name, status, kind]` (kind added for respawn
/// support; legacy 2-element entries yield an empty kind).
pub fn parse_poll_entry(v: &serde_json::Value) -> anyhow::Result<AgentState> {
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("__poll bad entry: {v}"))?;
    Ok(AgentState {
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
        kind: arr
            .get(2)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// Consecutive `unknown` polls before an agent is presumed a dead pane and
/// respawned. 2 balances transient classification flicker (unknown → idle →
/// unknown must never respawn) against a genuinely dead session: at the
/// settled 300s cadence this is ~10 min before repair; with the adaptive
/// cadence (active while the swarm is unsettled) repair typically fires
/// within ~1 min.
pub const UNKNOWN_RESPAWN_THRESHOLD: u64 = 2;

/// Decision for one agent's unknown-status bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownAction {
    /// Keep waiting (count below threshold, or agent classified).
    None,
    /// The agent has been unclassified for >= threshold polls — respawn it.
    Respawn,
}

/// Pure unknown-status verdict: an agent counted `unknown` for at least
/// `threshold` consecutive polls is presumed a dead pane. Never fires for a
/// classified status (working/idle/blocked/done are proof of life).
pub fn unknown_decision(status: &str, count: u64, threshold: u64) -> UnknownAction {
    if status == "unknown" && count >= threshold {
        UnknownAction::Respawn
    } else {
        UnknownAction::None
    }
}

/// Per-poll unknown bookkeeping + decision for one agent (the workflow calls
/// this once per polled state). Deterministic: rebuilt identically on replay
/// from the checkpointed poll results.
///
/// Map contract: `unknown` increments the count; ANY other status removes the
/// entry (flicker reset); a respawn removes the entry so the fresh instance
/// starts a clean episode — exactly one respawn per stuck episode.
pub fn track_unknown(
    unknown_seen: &mut std::collections::HashMap<String, u64>,
    name: &str,
    status: &str,
    threshold: u64,
) -> UnknownAction {
    if status != "unknown" {
        unknown_seen.remove(name);
        return UnknownAction::None;
    }
    let count = unknown_seen.entry(name.to_string()).or_insert(0);
    *count += 1;
    if unknown_decision(status, *count, threshold) == UnknownAction::Respawn {
        unknown_seen.remove(name);
        UnknownAction::Respawn
    } else {
        UnknownAction::None
    }
}

/// Shared respawn path for the loop: re-poll the agent kind (preferring the
/// kind learned from polls — a dropped agent's record cannot be re-queried),
/// respawn a fresh instance under the same name, and re-queue `text` at the
/// FRONT so the fresh agent inherits it next poll (None when there is nothing
/// to requeue — e.g. the unknown-status repair, whose agent was never
/// dispatched work). The single respawn point in the workflow — used by the
/// CONTEXT_LIMIT guard, the send-escalation path (bd-send-stall-escalation),
/// and the unknown-status repair (bd-unknown-respawn) so an agent can never
/// be double-respawned in one iteration.
async fn respawn_and_requeue(
    ctx: &Context,
    params: &LoopParams,
    queue: &mut Vec<String>,
    kinds: &std::collections::HashMap<String, String>,
    name: &str,
    text: Option<&str>,
    reason: &str,
    attempts: u64,
) -> Result<()> {
    let kind = match respawn_kind(kinds, name) {
        Some(k) => k,
        None => {
            ctx.run(
                poll_kind,
                PollKindArgs {
                    project: params.project.clone(),
                    name: name.to_string(),
                },
            )
            .await?
        }
    };
    ctx.run(
        respawn_agent,
        RespawnArgs {
            project: params.project.clone(),
            name: name.to_string(),
            kind,
            reason: reason.to_string(),
            attempts,
        },
    )
    .await?;
    if let Some(t) = text {
        queue.insert(0, t.to_string());
    }
    Ok(())
}

/// The durable monitoring workflow. Runs until the queue drains and the swarm
/// settles, or a blocked agent stops it (policy).
#[function]
async fn swarm_loop(ctx: &Context, params: LoopParams) -> Result<String> {
    let mut queue = params.queue.clone();
    let mut iteration = 0u64;
    // bead-driven mode tracks which beads were already handed out
    let mut assigned: Vec<String> = Vec::new();
    // per-agent consecutive send-failure count (send-stall escalation).
    // Workflow-local and deterministic: rebuilt identically on replay from
    // the checkpointed leaf results (poll states + send outcomes).
    let mut send_failures: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    // per-agent herdr kind learned from polls — needed to respawn an agent
    // whose record has been dropped (the escalation path).
    let mut agent_kinds: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // per-agent consecutive `unknown` poll count (unknown-status repair).
    // Workflow-local and deterministic: rebuilt identically on replay from
    // the checkpointed poll results.
    let mut unknown_seen: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

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

        // proof of life: a working/blocked agent is receiving prompts — its
        // failure history is stale (landed sends reset at the send site too)
        clear_live_failures(&mut send_failures, &states);
        // learn agent kinds for the respawn path (idempotent, never removes)
        learn_kinds(&mut agent_kinds, &states);

        // unknown-status repair: an agent unclassified for 2+ consecutive
        // polls is presumed a dead pane. Respawn it via the shared helper —
        // nothing to requeue (unknown agents are never dispatched work) and
        // the fresh instance starts a clean episode (counter reset inside
        // track_unknown). Skip-dispatch (bd-unknown-skip-dispatch) keeps the
        // queue intact meanwhile.
        for s in &states {
            let prev = unknown_seen.get(&s.name).copied().unwrap_or(0);
            if track_unknown(&mut unknown_seen, &s.name, &s.status, UNKNOWN_RESPAWN_THRESHOLD)
                == UnknownAction::Respawn
            {
                respawn_and_requeue(
                    ctx,
                    &params,
                    &mut queue,
                    &agent_kinds,
                    &s.name,
                    None,
                    "unknown-stuck",
                    prev + 1,
                )
                .await?;
            }
        }

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
                    let verdict = ctx
                        .run(
                            read_output,
                            ReadArgs {
                                project: params.project.clone(),
                                name: name.clone(),
                            },
                        )
                        .await?;
                    if verdict == ContextVerdict::Exhausted {
                        respawn_and_requeue(
                            ctx,
                            &params,
                            &mut queue,
                            &agent_kinds,
                            name,
                            Some(text),
                            "context_limit",
                            0,
                        )
                        .await?;
                        continue;
                    }
                    let outcome = match ctx
                        .run(
                            send_prompt,
                            SendArgs {
                                project: params.project.clone(),
                                name: name.clone(),
                                text: text.clone(),
                                confirm: params.confirm_sends,
                                confirm_timeout_ms: params.confirm_timeout_ms,
                            },
                        )
                        .await
                    {
                        Ok(o) => o,
                        // leaf error = the prompt did not land (agent dropped
                        // mid-wait, __send crashed) — same escalation path as
                        // a stalled send, never silently lost
                        Err(e) => SendOutcome {
                            ok: false,
                            name: name.clone(),
                            landed: Some(false),
                            status: None,
                            reason: Some(format!("error:{e}")),
                        },
                    };
                    any_work = true;
                    match handle_send_result(&mut send_failures, name, &outcome) {
                        Escalation::Accept => {}
                        Escalation::FailStop => {
                            return Err(map_err(anyhow::anyhow!(
                                "agent {name} failed to accept 3 prompts in a row (last reason: {:?})",
                                outcome.reason
                            )));
                        }
                        Escalation::RespawnAndRequeue => {
                            let attempts = send_failures.get(name).copied().unwrap_or(0);
                            let reason = outcome.reason.as_deref().unwrap_or("stalled");
                            respawn_and_requeue(
                                ctx,
                                &params,
                                &mut queue,
                                &agent_kinds,
                                name,
                                Some(text),
                                reason,
                                attempts,
                            )
                            .await?;
                        }
                    }
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

        // durable pause — the worker may exit here; the server resumes us.
        // Adaptive cadence: active interval after a dispatch and while anyone
        // is working/blocked/unknown, base interval when settled (the 5s
        // floor lives in the fn).
        ctx.sleep(Duration::from_secs(next_interval_secs(
            &states,
            any_work,
            params.interval_secs,
            params.active_interval_secs,
        )))
        .await?;
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
    use crate::agentstate::AgentStateFile;

    fn st(name: &str, status: &str) -> AgentState {
        AgentState {
            name: name.to_string(),
            status: status.to_string(),
            kind: String::new(),
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

    // ---- adaptive poll interval (bd-adaptive-interval) ----

    #[test]
    fn next_interval_working_is_active() {
        let states = vec![st("a", "working")];
        assert_eq!(next_interval_secs(&states, false, 300, 30), 30);
    }

    #[test]
    fn next_interval_blocked_is_active() {
        let states = vec![st("a", "blocked")];
        assert_eq!(next_interval_secs(&states, false, 300, 30), 30);
    }

    #[test]
    fn next_interval_all_idle_is_base() {
        let states = vec![st("a", "idle"), st("b", "done")];
        assert_eq!(next_interval_secs(&states, false, 300, 30), 300);
    }

    #[test]
    fn next_interval_dispatch_forces_active() {
        // states are PRE-action: an idle agent that just received a queue item
        // will be working during the sleep — a dispatch must force the active
        // cadence or the swarm drains at base speed
        let states = vec![st("a", "idle")];
        assert_eq!(next_interval_secs(&states, true, 300, 30), 30);
    }

    #[test]
    fn next_interval_all_done_is_base() {
        let states = vec![st("a", "done")];
        assert_eq!(next_interval_secs(&states, false, 300, 30), 300);
    }

    #[test]
    fn next_interval_unknown_is_active() {
        // unknown counts as unsettled → active cadence, so pending repairs
        // (bd-unknown-respawn) proceed promptly
        let states = vec![st("a", "unknown")];
        assert_eq!(next_interval_secs(&states, false, 300, 30), 30);
    }

    #[test]
    fn next_interval_empty_is_base() {
        let states = vec![];
        assert_eq!(next_interval_secs(&states, false, 300, 30), 300);
    }

    #[test]
    fn next_interval_clamps_to_interval_min() {
        let states = vec![st("a", "working")];
        assert_eq!(next_interval_secs(&states, false, 300, 2), 5);
        // base below the floor also clamps
        let idle = vec![st("a", "idle")];
        assert_eq!(next_interval_secs(&idle, false, 3, 30), 5);
    }

    // ---- send-stall escalation (bd-send-stall-escalation) ----

    fn so(landed: Option<bool>, reason: Option<&str>) -> SendOutcome {
        SendOutcome {
            ok: true,
            name: "a".to_string(),
            landed,
            status: None,
            reason: reason.map(str::to_string),
        }
    }

    #[test]
    fn escalate_stalled_below_cap_respawns() {
        // 1st..3rd stall (0/1/2 prior failures) → respawn + requeue
        assert_eq!(
            escalate(&so(Some(false), Some("stalled")), 0),
            Escalation::RespawnAndRequeue
        );
        assert_eq!(
            escalate(&so(Some(false), Some("stalled")), 1),
            Escalation::RespawnAndRequeue
        );
        assert_eq!(
            escalate(&so(Some(false), Some("stalled")), 2),
            Escalation::RespawnAndRequeue
        );
    }

    #[test]
    fn escalate_stalled_at_cap_stops() {
        // 4th consecutive stall (3 prior failures) → hard stop, no infinite loop
        assert_eq!(
            escalate(&so(Some(false), Some("stalled")), 3),
            Escalation::FailStop
        );
    }

    #[test]
    fn escalate_coded_error_respawns() {
        // agent dropped mid-wait (agent_not_found) or any other hard error
        assert_eq!(
            escalate(&so(Some(false), Some("error:agent_not_found")), 0),
            Escalation::RespawnAndRequeue
        );
    }

    #[test]
    fn escalate_timeout_accepts() {
        // ambiguous: the prompt may have started — verify next poll, never
        // respawn, and never FailStop even with a large counter
        assert_eq!(escalate(&so(Some(false), Some("timeout")), 0), Escalation::Accept);
        assert_eq!(escalate(&so(Some(false), Some("timeout")), 9), Escalation::Accept);
    }

    #[test]
    fn escalate_landed_accepts() {
        let mut o = so(Some(true), Some("landed"));
        o.status = Some("working".to_string());
        assert_eq!(escalate(&o, 0), Escalation::Accept);
        assert_eq!(escalate(&o, 7), Escalation::Accept);
    }

    #[test]
    fn escalate_blocked_accepts() {
        // blocked is a valid landing — the --on-blocked policy owns it
        let mut o = so(Some(true), Some("landed"));
        o.status = Some("blocked".to_string());
        assert_eq!(escalate(&o, 0), Escalation::Accept);
    }

    #[test]
    fn escalate_no_verdict_accepts() {
        // legacy fire-and-forget path (confirm=false): no landing info at all
        assert_eq!(escalate(&so(None, None), 0), Escalation::Accept);
    }

    #[test]
    fn handle_send_result_landed_resets() {
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("a".to_string(), 2);
        let esc = handle_send_result(&mut f, "a", &so(Some(true), Some("landed")));
        assert_eq!(esc, Escalation::Accept);
        assert!(!f.contains_key("a"), "landed send must clear the counter");
    }

    #[test]
    fn handle_send_result_timeout_keeps_counter() {
        // timeout is not a landing and not a failure — counter untouched
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("a".to_string(), 2);
        let esc = handle_send_result(&mut f, "a", &so(Some(false), Some("timeout")));
        assert_eq!(esc, Escalation::Accept);
        assert_eq!(f.get("a"), Some(&2));
    }

    #[test]
    fn handle_send_result_stall_increments() {
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("a".to_string(), 1);
        let esc = handle_send_result(&mut f, "a", &so(Some(false), Some("stalled")));
        assert_eq!(esc, Escalation::RespawnAndRequeue);
        assert_eq!(f.get("a"), Some(&2), "stall must increment the counter");
        // first failure on a clean agent starts at 1
        let mut g: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        handle_send_result(&mut g, "a", &so(Some(false), Some("error:agent_not_found")));
        assert_eq!(g.get("a"), Some(&1));
    }

    #[test]
    fn handle_send_result_failstop_does_not_increment() {
        // at the cap the loop stops — no further bookkeeping matters, but the
        // counter must not grow past the cap
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("a".to_string(), 3);
        let esc = handle_send_result(&mut f, "a", &so(Some(false), Some("stalled")));
        assert_eq!(esc, Escalation::FailStop);
        assert_eq!(f.get("a"), Some(&3));
    }

    #[test]
    fn clear_live_failures_resets_working_and_blocked() {
        // proof of life: a working/blocked agent is receiving prompts
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("work".to_string(), 2);
        f.insert("block".to_string(), 1);
        f.insert("idle".to_string(), 2);
        let states = vec![st("work", "working"), st("block", "blocked"), st("idle", "idle")];
        clear_live_failures(&mut f, &states);
        assert!(!f.contains_key("work"));
        assert!(!f.contains_key("block"));
        assert_eq!(f.get("idle"), Some(&2), "idle is not proof of life");
    }

    #[test]
    fn clear_live_failures_keeps_absent_and_unknown() {
        // an agent missing from the poll (herdr dropped it) or unknown keeps
        // its counter — no proof of life
        let mut f: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        f.insert("gone".to_string(), 1);
        f.insert("unk".to_string(), 1);
        let states = vec![st("unk", "unknown")];
        clear_live_failures(&mut f, &states);
        assert_eq!(f.get("gone"), Some(&1));
        assert_eq!(f.get("unk"), Some(&1));
    }

    #[test]
    fn resume_prompt_mentions_continuation() {
        let p = resume_prompt("bd-7", "finish the parser");
        assert!(p.contains("bd-7"));
        assert!(p.contains("Resume"));
        assert!(p.contains("CONTEXT_LIMIT"));
    }

    #[test]
    fn resume_prompt_carries_state_file_contract() {
        let p = resume_prompt("bd-7", "finish the parser");
        // the literal path pattern + the field; the name placeholder is
        // substituted at dispatch (plan_actions) — the template must not
        // hardcode any agent
        assert!(p.contains(".flywheel/agent-state/"));
        assert!(p.contains("context_pct"));
        assert!(p.contains("{AGENT_NAME}"));
        assert!(p.contains(".flywheel/agent-state/{AGENT_NAME}.json"));
    }

    #[test]
    fn context_marker_detection() {
        // the terminal-marker channel still exhausts on its own
        assert_eq!(
            context_verdict("...CONTEXT_LIMIT", None, 1_785_726_000),
            ContextVerdict::Exhausted
        );
        assert_eq!(
            context_verdict("CONTEXT_LIMIT\nall done", None, 1_785_726_000),
            ContextVerdict::Exhausted
        );
        assert_eq!(
            context_verdict("all done", None, 1_785_726_000),
            ContextVerdict::Healthy
        );
    }

    #[test]
    fn context_verdict_fresh_file_exhausts_without_marker() {
        // alt-screen case: terminal read shows nothing, file is fresh + hot
        let state = Some(AgentStateFile {
            context_pct: 85,
            ts: "2026-08-03T03:00:00Z".into(),
        });
        assert_eq!(
            context_verdict("", state.as_ref(), 1_785_726_000 + 100),
            ContextVerdict::Exhausted
        );
    }

    #[test]
    fn context_verdict_stale_file_is_healthy() {
        // a file from a previous session must never kill a healthy agent
        let state = Some(AgentStateFile {
            context_pct: 85,
            ts: "2026-08-03T03:00:00Z".into(),
        });
        assert_eq!(
            context_verdict("", state.as_ref(), 1_785_726_000 + 3600),
            ContextVerdict::Healthy
        );
    }

    #[test]
    fn context_verdict_both_channels_exhausts() {
        let state = Some(AgentStateFile {
            context_pct: 90,
            ts: "2026-08-03T03:00:00Z".into(),
        });
        assert_eq!(
            context_verdict("CONTEXT_LIMIT", state.as_ref(), 1_785_726_000),
            ContextVerdict::Exhausted
        );
    }

    #[test]
    fn context_verdict_neither_channel_is_healthy() {
        assert_eq!(
            context_verdict("working fine", None, 1_785_726_000),
            ContextVerdict::Healthy
        );
        // absent state file → identical to today's behavior
        assert_eq!(
            context_verdict("", None, 1_785_726_000),
            ContextVerdict::Healthy
        );
    }

    #[test]
    fn context_verdict_pct_boundary() {
        let ts = "2026-08-03T03:00:00Z";
        let low = AgentStateFile { context_pct: 79, ts: ts.into() };
        let at = AgentStateFile { context_pct: 80, ts: ts.into() };
        assert_eq!(
            context_verdict("", Some(&low), 1_785_726_000),
            ContextVerdict::Healthy
        );
        assert_eq!(
            context_verdict("", Some(&at), 1_785_726_000),
            ContextVerdict::Exhausted
        );
    }

    #[test]
    fn context_verdict_age_boundary() {
        let ts = "2026-08-03T03:00:00Z"; // unix 1_785_726_000, max age 900s
        let state = AgentStateFile { context_pct: 85, ts: ts.into() };
        // exactly 15 min old → fresh → exhausted
        assert_eq!(
            context_verdict("", Some(&state), 1_785_726_000 + 900),
            ContextVerdict::Exhausted
        );
        // one second older → stale → healthy
        assert_eq!(
            context_verdict("", Some(&state), 1_785_726_000 + 901),
            ContextVerdict::Healthy
        );
    }

    #[test]
    fn parse_read_output_parses_new_leaf_shape() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"terminal": "hello", "state": {"context_pct": 85, "ts": "2026-08-03T03:00:00Z"}}"#,
        )
        .unwrap();
        let ro = parse_read_output(&v).unwrap();
        assert_eq!(ro.terminal, "hello");
        assert_eq!(ro.state.as_ref().map(|s| s.context_pct), Some(85));
    }

    #[test]
    fn parse_read_output_null_and_missing_state() {
        let null_state: serde_json::Value =
            serde_json::from_str(r#"{"terminal": "x", "state": null}"#).unwrap();
        let ro = parse_read_output(&null_state).unwrap();
        assert_eq!(ro.state, None);
        // legacy plain-text output (pre-zwa) tolerated via terminal field
        let missing: serde_json::Value = serde_json::from_str(r#"{"terminal": "y"}"#).unwrap();
        let ro = parse_read_output(&missing).unwrap();
        assert_eq!(ro.state, None);
        assert_eq!(ro.terminal, "y");
    }

    #[test]
    fn bead_prompt_is_self_contained() {
        let p = bead_prompt("bd-12", "fix auth flow");
        assert!(p.contains("bd-12"));
        assert!(p.contains("--claim"));
        assert!(p.contains("br sync --flush-only"));
    }

    #[test]
    fn bead_prompt_carries_state_file_contract() {
        let p = bead_prompt("bd-12", "fix auth flow");
        assert!(p.contains(".flywheel/agent-state/"));
        assert!(p.contains("context_pct"));
        assert!(p.contains(".flywheel/agent-state/{AGENT_NAME}.json"));
        assert!(p.contains("CONTEXT_LIMIT"));
    }

    #[test]
    fn plan_actions_substitutes_agent_name_at_dispatch() {
        // a queued bead prompt carries {AGENT_NAME}; the idle agent's name
        // replaces it when the Send action is built
        let mut queue = vec![bead_prompt("bd-12", "fix auth flow")];
        let states = vec![st("flywheel-proj-oc", "idle")];
        let actions = plan_actions(&states, &mut queue, None);
        match &actions[0] {
            LoopAction::Send { name, text } => {
                assert_eq!(name, "flywheel-proj-oc");
                assert!(text.contains(".flywheel/agent-state/flywheel-proj-oc.json"));
                assert!(!text.contains("{AGENT_NAME}"));
            }
            other => panic!("expected Send, got {other:?}"),
        }
        assert!(queue.is_empty());
    }

    #[test]
    fn plan_actions_leaves_plain_prompts_untouched() {
        let mut queue = vec!["t1".to_string()];
        let states = vec![st("flywheel-proj-oc", "idle")];
        let actions = plan_actions(&states, &mut queue, None);
        match &actions[0] {
            LoopAction::Send { text, .. } => assert_eq!(text, "t1"),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn params_serde_roundtrip() {
        let p = LoopParams {
            project: "notetoself".into(),
            to: "all".into(),
            interval_secs: 300,
            active_interval_secs: 15,
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
        assert_eq!(back.active_interval_secs, 15);
        assert_eq!(back.max_iterations, Some(10));
        assert!(back.confirm_sends);
        assert_eq!(back.confirm_timeout_ms, Some(45_000));
    }

    #[test]
    fn params_active_interval_defaults_30_when_absent() {
        // pre-yna saved params (loops.json) must still load: the active
        // cadence defaults to 30, matching the pre-config balance
        let v: serde_json::Value = serde_json::json!({
            "project": "notetoself",
            "to": "all",
            "interval_secs": 300,
            "queue": [],
            "nudge": null,
            "on_blocked": "continue",
            "max_iterations": null,
            "work": false,
            "confirm_sends": true,
            "confirm_timeout_ms": null,
        });
        let back: LoopParams = serde_json::from_value(v).unwrap();
        assert_eq!(back.active_interval_secs, 30);
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

    #[test]
    fn learn_kinds_from_poll_records_kinds() {
        let mut kinds = std::collections::HashMap::new();
        let states = vec![
            AgentState {
                name: "a".into(),
                status: "idle".into(),
                kind: "pi".into(),
            },
            AgentState {
                name: "b".into(),
                status: "working".into(),
                kind: "opencode".into(),
            },
        ];
        learn_kinds(&mut kinds, &states);
        assert_eq!(kinds.get("a").map(String::as_str), Some("pi"));
        assert_eq!(kinds.get("b").map(String::as_str), Some("opencode"));
    }

    #[test]
    fn learn_kinds_keeps_previous_kind_when_poll_has_none() {
        // an agent whose record was dropped disappears from the poll; a
        // later poll with an empty kind must not clobber the learned one
        let mut kinds = std::collections::HashMap::new();
        kinds.insert("a".to_string(), "opencode".to_string());
        let states = vec![AgentState {
            name: "a".into(),
            status: "idle".into(),
            kind: String::new(),
        }];
        learn_kinds(&mut kinds, &states);
        assert_eq!(kinds.get("a").map(String::as_str), Some("opencode"));
    }

    #[test]
    fn respawn_kind_prefers_map_and_returns_none_for_unknown() {
        let mut kinds = std::collections::HashMap::new();
        kinds.insert("a".to_string(), "pi".to_string());
        assert_eq!(respawn_kind(&kinds, "a"), Some("pi".to_string()));
        assert_eq!(respawn_kind(&kinds, "zzz"), None);
    }

    #[test]
    fn agent_state_parses_kind_from_poll_entry() {
        // __poll emits [name, status, kind]; kind must be parsed (and absent
        // in legacy entries tolerated)
        let v: serde_json::Value =
            serde_json::from_str(r#"["a", "idle", "pi"]"#).unwrap();
        let s = parse_poll_entry(&v).unwrap();
        assert_eq!(s.name, "a");
        assert_eq!(s.status, "idle");
        assert_eq!(s.kind, "pi");
        let legacy: serde_json::Value = serde_json::from_str(r#"["a", "idle"]"#).unwrap();
        let s = parse_poll_entry(&legacy).unwrap();
        assert_eq!(s.kind, "");
    }

    #[test]
    fn unknown_decision_respawns_at_threshold_only() {
        // 0/1 unknown polls → wait; 2 → respawn; beyond → respawn (already stuck)
        assert_eq!(unknown_decision("unknown", 0, UNKNOWN_RESPAWN_THRESHOLD), UnknownAction::None);
        assert_eq!(unknown_decision("unknown", 1, UNKNOWN_RESPAWN_THRESHOLD), UnknownAction::None);
        assert_eq!(unknown_decision("unknown", 2, UNKNOWN_RESPAWN_THRESHOLD), UnknownAction::Respawn);
        assert_eq!(unknown_decision("unknown", 5, UNKNOWN_RESPAWN_THRESHOLD), UnknownAction::Respawn);
    }

    #[test]
    fn unknown_decision_never_fires_on_classified_status() {
        // a working/idle agent is confirmed alive — no respawn even at a high count
        assert_eq!(unknown_decision("idle", 5, UNKNOWN_RESPAWN_THRESHOLD), UnknownAction::None);
        assert_eq!(unknown_decision("working", 2, UNKNOWN_RESPAWN_THRESHOLD), UnknownAction::None);
        assert_eq!(unknown_decision("blocked", 2, UNKNOWN_RESPAWN_THRESHOLD), UnknownAction::None);
    }

    #[test]
    fn track_unknown_sequence_two_unknown_polls_respawns_once() {
        // integration-style: (unknown, unknown) → exactly one Respawn, episode reset
        let mut m = std::collections::HashMap::new();
        assert_eq!(
            track_unknown(&mut m, "a", "unknown", UNKNOWN_RESPAWN_THRESHOLD),
            UnknownAction::None
        );
        assert_eq!(m.get("a"), Some(&1));
        assert_eq!(
            track_unknown(&mut m, "a", "unknown", UNKNOWN_RESPAWN_THRESHOLD),
            UnknownAction::Respawn
        );
        // the fresh instance starts a clean episode — counter gone
        assert!(m.is_empty());
    }

    #[test]
    fn track_unknown_flicker_never_respawns() {
        // unknown → idle → unknown: the idle poll resets the counter, so the
        // second unknown starts at 1 and never reaches the threshold
        let mut m = std::collections::HashMap::new();
        track_unknown(&mut m, "a", "unknown", UNKNOWN_RESPAWN_THRESHOLD);
        track_unknown(&mut m, "a", "idle", UNKNOWN_RESPAWN_THRESHOLD);
        assert!(m.is_empty());
        assert_eq!(
            track_unknown(&mut m, "a", "unknown", UNKNOWN_RESPAWN_THRESHOLD),
            UnknownAction::None
        );
        assert_eq!(m.get("a"), Some(&1));
        assert_eq!(
            track_unknown(&mut m, "a", "unknown", UNKNOWN_RESPAWN_THRESHOLD),
            UnknownAction::Respawn
        );
    }

    #[test]
    fn track_unknown_tracks_agents_independently() {
        let mut m = std::collections::HashMap::new();
        track_unknown(&mut m, "a", "unknown", UNKNOWN_RESPAWN_THRESHOLD);
        track_unknown(&mut m, "b", "unknown", UNKNOWN_RESPAWN_THRESHOLD);
        track_unknown(&mut m, "b", "unknown", UNKNOWN_RESPAWN_THRESHOLD); // b respawns
        assert_eq!(m.get("a"), Some(&1)); // a untouched by b's respawn
        assert!(!m.contains_key("b"));
    }
}

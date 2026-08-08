//! Durable swarm loop — the monitoring orchestrator as a Resonate workflow.
//!
//! Design (resonate-philosophy): the loop is sequential happy-path code with
//! checkpoints. Every `ctx.run` (poll, send) and `ctx.sleep` is a durable save
//! point — if the flywheel process crashes, the workflow resumes from the last
//! checkpoint on restart with the same invocation id. The queue and the agent
//! states are deterministic across replays because every effect is checkpointed.
//!
//! Leaves shell out to the flywheel binary itself (`__poll` / `__poll_events` /
//! `__send` hidden subcommands) so the blocking herdr socket client never runs
//! on the async executor and each side effect is a single crash-safe process.
//!
//! **Event stream (bd-event-subscribe-reactor):** `__poll_events` is a forward-
//! only latency optimization. herdr has no since/seq replay. The heartbeat
//! `__poll` (agent.list snapshot) is the correctness floor on every iteration
//! and after crash-resume.
//!
//! **At-least-once + leaf idempotency (cg6.3):** leaves run under at-least-once
//! delivery — a crash while a leaf is in flight re-invokes it on resume. Every
//! SIDE-EFFECTING leaf is idempotent under that window:
//! - `send_prompt` → the folded `__send` subprocess dedups via the
//!   invocation-scoped `record_sent`/`should_resend` registry (main.rs);
//! - `respawn_agent` → `__respawn` no-ops when an agent with the same name
//!   already exists healthy in the workspace (main.rs; the `context_limit`
//!   path passes `--force` and keeps a documented replay window);
//! - `notify_blocked` → the persisted `last_ping_tier` marker gates the mail:
//!   at most one ping per logical tier transition (durable.rs escalation
//!   step);
//! - `spawn_xproj` → the per-project `{project}.xproj-seen.jsonl` marker is
//!   written BEFORE spawning: one swarm per message id.
//!   Pure read leaves (poll_*) need no dedup — re-running them is harmless.

use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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
    /// this invocation's id, set at run time (last-sent registry; empty when
    /// running from pre-registry params — such sends record no invocation).
    #[serde(default)]
    pub invocation: String,
    /// consecutive `unknown` polls before Diagnose (bd-event-self-heal).
    #[serde(default = "default_unknown_diagnose_threshold")]
    pub unknown_diagnose_threshold: u32,
    /// max respawns per agent per loop run before EscalateBlocked.
    #[serde(default = "default_respawn_budget")]
    pub respawn_budget: u32,
}

fn default_unknown_diagnose_threshold() -> u32 {
    DEFAULT_UNKNOWN_DIAGNOSE_THRESHOLD
}

fn default_respawn_budget() -> u32 {
    DEFAULT_RESPAWN_BUDGET
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
    /// herdr pane id — required to map push events → agent name
    /// (bd-event-dispatch-zero-latency). `#[serde(default)]` for old polls.
    #[serde(default)]
    pub pane_id: String,
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

/// Args for the `__poll_events` leaf (bd-event-subscribe-reactor).
/// Simple identifier params only — Resonate constraint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollEventsArgs {
    pub project: String,
    /// block up to this many ms collecting events
    pub timeout_ms: u64,
    /// best-effort in-run dedup cursor (0 = none)
    #[serde(default)]
    pub after_seq: u64,
}

/// One status-change event from `__poll_events`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PollEvent {
    pub event_type: String,
    pub pane_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    pub seq: u64,
}

/// Batch returned by `__poll_events`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct PollEventsBatch {
    #[serde(default)]
    pub events: Vec<PollEvent>,
    #[serde(default)]
    pub max_seq: u64,
    #[serde(default)]
    pub stream_error: Option<String>,
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
    /// When a prompt is requeued with this respawn, the hash of that prompt
    /// — its delivery record is INVALIDATED (the delivery died with the
    /// respawned agent; the fresh agent must get the prompt again).
    #[serde(default)]
    pub text_hash: Option<u64>,
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
    /// loop invocation id (last-sent registry bookkeeping; empty when the
    /// loop predates the registry)
    pub invocation: String,
    /// loop iteration at send time (last-sent registry bookkeeping)
    pub iteration: u64,
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
    if !args.invocation.is_empty() {
        argv.push("--invocation".to_string());
        argv.push(args.invocation.clone());
        argv.push("--iteration".to_string());
        argv.push(args.iteration.to_string());
    }
    argv
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotifyArgs {
    pub project: String,
    pub name: String,
    /// Seed for a FIRST escalation entry (None → the leaf stamps `now`).
    /// The leaf loads the persisted entry when one exists — replay never
    /// re-seeds, so the tier sequence is deterministic (cg6.1).
    pub first_blocked_ts: Option<i64>,
    /// Loop poll interval in seconds — the escalation tier thresholds
    /// (2×/4×/8× interval) derive from it.
    pub interval_secs: u64,
    /// Loop params, preserved into a freshly-created registry entry
    /// (parity with the pre-cg6.1 body-side first-init).
    pub params: serde_json::Value,
}

/// What the `notify_blocked` leaf computed and persisted (cg6.1). The
/// workflow body acts on these values — it never recomputes the tier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EscalationOutcome {
    /// Escalation tier 0..3, derived from the PERSISTED first-blocked
    /// timestamp at leaf-execution time.
    pub tier: u32,
    /// Ping count after this notification (persisted; starts at 1).
    pub ping_count: u32,
}

/// Args for the diagnose leaf (read tail + health mail).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnoseArgs {
    pub project: String,
    pub name: String,
    pub reason: String,
}

/// Args for the xproj-inbox leaf (cross-project swarm receive path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XprojInboxArgs {
    pub project: String,
    #[serde(default = "default_xproj_limit")]
    pub limit: u32,
}

fn default_xproj_limit() -> u32 {
    30
}

/// Args for the xproj-spawn leaf (cross-project swarm receive, full branch).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XprojSpawnArgs {
    pub project: String,
    /// String: agent-mail ids are u64 (arrive as numbers), toron ids are hex
    /// event ids (strings). The leaf passes it through verbatim.
    pub message_id: String,
    pub thread_id: String,
    pub from: String,
    pub recipe: String,
    pub workers: u32,
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
///
/// SOFT-FAIL: if `__poll` exits non-zero (e.g. the swarm was killed or never
/// spawned — a normal operational state, not an error), return an EMPTY state
/// list so the durable loop settles cleanly (`all_settled` treats an empty
/// swarm as settled) instead of crashing the whole invocation. This mirrors
/// the `poll_events` leaf's soft-fail and fixes the "durable loop invocation
/// failed: __poll failed" that killed a run after `flywheel kill <proj>`.
#[function]
async fn poll_swarm(args: PollArgs) -> Result<Vec<AgentState>> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args(["__poll", &args.project, &args.to])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__poll spawn failed: {e}")))?;
    if !out.status.success() {
        // Swarm gone / not spawned — soft-fail to empty so the loop settles
        // rather than aborting the durable run.
        let reason = String::from_utf8_lossy(&out.stderr).trim().to_string();
        eprintln!(
            "flywheel: __poll soft-fail (swarm absent?): {}",
            if reason.is_empty() {
                format!("__poll exit {}", out.status)
            } else {
                reason
            }
        );
        return Ok(Vec::new());
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

/// Collect herdr push events for up to `timeout_ms` (forward-only).
/// Checkpointed. Empty batch on stream failure is a soft outcome (the
/// heartbeat `__poll` remains the correctness floor).
#[function]
async fn poll_events(args: PollEventsArgs) -> Result<PollEventsBatch> {
    let exe = self_exe().map_err(map_err)?;
    let mut cmd = Command::new(&exe);
    cmd.args([
        "__poll_events",
        &args.project,
        "--timeout-ms",
        &args.timeout_ms.to_string(),
    ]);
    if args.after_seq > 0 {
        cmd.args(["--after-seq", &args.after_seq.to_string()]);
    }
    let out = cmd
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__poll_events spawn failed: {e}")))?;
    if !out.status.success() {
        // soft-fail: return empty batch with the error so the loop can fall
        // back to heartbeat __poll without killing the workflow
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Ok(PollEventsBatch {
            events: vec![],
            max_seq: args.after_seq,
            stream_error: Some(if err.is_empty() {
                format!("__poll_events exit {}", out.status)
            } else {
                err
            }),
        });
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let batch = parse_poll_events_batch(raw.trim(), args.after_seq).map_err(map_err)?;
    // is1.4: log the batch INSIDE the leaf so the entries are checkpointed
    // with the result — a body-level log re-fires on every crash-resume
    // replay and duplicates JSONL entries (the event log feeds analytics).
    log_poll_events(&args.project, &batch);
    Ok(batch)
}

/// Pure parse of `__poll_events` stdout (unit-testable).
pub fn parse_poll_events_batch(raw: &str, fallback_seq: u64) -> anyhow::Result<PollEventsBatch> {
    let v: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| anyhow::anyhow!("__poll_events bad JSON: {e}: {raw}"))?;
    let events = v
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(events.len());
    for e in events {
        let event_type = e
            .get("event_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let pane_id = e
            .get("pane_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let agent = e.get("agent").and_then(|a| {
            if a.is_null() {
                None
            } else {
                a.as_str().map(str::to_string)
            }
        });
        let status = e.get("status").and_then(|s| {
            if s.is_null() {
                None
            } else {
                s.as_str().map(str::to_string)
            }
        });
        let seq = e.get("seq").and_then(Value::as_u64).unwrap_or(0);
        out.push(PollEvent {
            event_type,
            pane_id,
            agent,
            status,
            seq,
        });
    }
    let max_seq = v
        .get("max_seq")
        .and_then(Value::as_u64)
        .unwrap_or(fallback_seq);
    let stream_error = v.get("stream_error").and_then(|e| {
        if e.is_null() {
            None
        } else {
            e.as_str().map(str::to_string)
        }
    });
    Ok(PollEventsBatch {
        events: out,
        max_seq,
        stream_error,
    })
}

/// Log each routed event to the project JSONL audit trail.
pub fn log_poll_events(project: &str, batch: &PollEventsBatch) {
    for e in &batch.events {
        let _ = crate::state::log_event(
            project,
            crate::state::EventKind::State,
            serde_json::json!({
                "source": "poll_events",
                "event_type": e.event_type,
                "pane_id": e.pane_id,
                "agent": e.agent,
                "status": e.status,
                "seq": e.seq,
            }),
        );
    }
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
            id: b
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            title: b
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
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
    // Mail backend is toron by default (`flywheel mail` → [backend] mail=toron).
    // ALWAYS pass --as <mail identity> so peer collab is not CoralFinch-collapsed.
    // Prefer mail name from spawn (adjective+noun); herdr name also maps via --as.
    let template = r#"Execute bead {id}: {title}. Mail backend: toron via `flywheel mail` (NOT agent-mail unless config flips). Lifecycle: claim (`br update {id} --claim --actor "$AGENT_NAME" --json`) → reserve `flywheel mail locks <project> --as YOUR_MAIL_NAME --paths <files> --exclusive --reason {id} --ttl-seconds 7200` → announce `flywheel mail send <project> --as YOUR_MAIL_NAME --to all --thread-id {id} --subject '[{id}] starting' --body '<plan>'` → execute one narrow slice → verify AC → close with honest reason → `br sync --flush-only` + commit `.beads/` → `flywheel mail unlock <project> --as YOUR_MAIL_NAME` → `flywheel mail send ... --thread-id {id} --subject '[{id}] done'`. YOUR_MAIL_NAME = registered adjective+noun (DustySparrow/…) or set FLYWHEEL_MAIL_AS. Renew your own agent-mail file reservations while working (extend the TTL before it lapses — the orchestrator cannot renew on your behalf), so long edits never lose their lease. When a message has ack_required=true, call acknowledge_message after acting on it — the orchestrator cannot ack on your behalf. Peer blocks: reply on same --thread-id, never steal reserved files. If your session context usage exceeds about 80%: write `{"context_pct": <pct>, "ts": "<now RFC3339>"}` to `.flywheel/agent-state/{AGENT_NAME}.json` in the project root (best-effort, never block on it), then finish and sync the current bead, then reply exactly CONTEXT_LIMIT so the orchestrator can respawn you. If {id} already claimed, next from `br ready --json`."#;
    template.replace("{id}", id).replace("{title}", title)
}

/// Prompt to resume an in-progress bead after an interruption. Carries the
/// same state-file contract (placeholder substituted at dispatch).
pub fn resume_prompt(id: &str, title: &str) -> String {
    format!(
        "Resume bead {id}: {title}. Still in_progress — finish, verify AC, close honestly, `br sync --flush-only` + commit `.beads/`. Mail is toron via `flywheel mail`: Renew your own agent-mail file reservations with `--as YOUR_MAIL_NAME` (extend the TTL before it lapses — the orchestrator cannot renew on your behalf), keep using `--thread-id {id}` for peer updates. Never release another agent's exclusive lock. When a message has ack_required=true, call acknowledge_message after acting on it — the orchestrator cannot ack on your behalf. If your session context usage exceeds about 80%: write `{{\"context_pct\": <pct>, \"ts\": \"<now RFC3339>\"}}` to `.flywheel/agent-state/{{AGENT_NAME}}.json` in the project root (best-effort, never block on it), then finish and sync the current bead, then reply exactly CONTEXT_LIMIT so the orchestrator can respawn you."
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
    if let Some(s) = state
        && s.context_pct >= 80
        && crate::agentstate::is_fresh_at(&s.ts, now_unix, crate::agentstate::DEFAULT_MAX_AGE)
    {
        return ContextVerdict::Exhausted;
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

/// Resume-dedup verdict: should the loop (re-)send this prompt?
///
/// - no record → never sent → resend.
/// - record from ANOTHER invocation → a fresh run legitimately re-delegates
///   → resend.
/// - landed, same invocation → delivered before the crash → skip (the work
///   is in flight; the queue item was already consumed).
/// - unlanded, same invocation → crashed mid-flight → resend (safe, needed).
///
/// Without confirmed sends the registry records nothing (or unlanded), so the
/// guard degrades to today's always-resend behavior.
pub fn should_resend(record: Option<&crate::state::SentRecord>, current_invocation: &str) -> bool {
    match record {
        None => true,
        // cg6.5: a timeout-confirm record is AMBIGUOUS — the prompt may be
        // mid-flight; never re-delegate it on resume. The next poll is the
        // correctness floor (it reveals whether the prompt landed).
        Some(r) => r.invocation != current_invocation || (!r.landed && !r.ambiguous),
    }
}

/// Escalation tier for a blocked agent (bd-blocked-escalation-protocol, A6).
///
/// Tiered by elapsed time since first_blocked (Resonate `ctx.now()` i64-µs,
/// deterministic):
/// - `T0` (first detection): high + ack_required ping with the agent tail.
/// - `T1` (+2×interval, still un-acked): reply in-thread, high, fresh tail.
/// - `T2` (+4×interval): reply urgent + summarize_thread; CC human if set.
/// - `T3` (+8×interval): loop halts (bounded), final "needs human" mail.
///
/// Returns the tier to act at now. `ping_count` advances per notify so
/// re-pings are monotonic; the tier is derived from elapsed time, not the
/// counter, so a crash-resume continues at the correct tier.
///
/// Implemented halt rule (review-fix F4): `mail` mode halts at T3; `stop`
/// mode halts at T0 (existing behavior); `continue` mode never halts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EscalationTier {
    T0,
    T1,
    T2,
    T3,
}

/// The bounded-halt decision for a blocked agent. Returns the exit message
/// when the loop must halt for this tier under `on_blocked`, else None.
///
/// Rule (review-fix F4, bd-review-fix-epic-m0b.4):
/// - `mail` mode: halt at T3 only (T0/T1/T2 keep notifying and continuing).
/// - `stop` mode: halt at any tier (T0) — existing `--on-blocked stop`
///   semantics, unchanged.
/// - `continue` mode: never halt — blockers are ignored by contract.
///
/// Pure (no I/O) so the halt policy is table-testable and the loop arm is a
/// thin wrapper.
pub fn on_blocked_tier_exit(tier: EscalationTier, on_blocked: &str) -> Option<String> {
    match on_blocked {
        "mail" if tier == EscalationTier::T3 => {
            Some("halted: agent blocked past escalation budget (T3) — human required".to_string())
        }
        "stop" => Some(format!(
            "stopped: blocked agents (--on-blocked stop, tier {tier:?})"
        )),
        _ => None,
    }
}

/// Map elapsed-us since first_blocked to the escalation tier.
/// `interval_secs` is the loop's active interval (the durable sleep period).
/// Elapsed thresholds: T1 at 2×, T2 at 4×, T3 at 8× the interval.
pub fn escalation_tier(
    first_blocked_ts_us: i64,
    now_us: i64,
    interval_secs: u64,
) -> EscalationTier {
    let interval_us = (interval_secs as i64).saturating_mul(1_000_000);
    let elapsed = now_us.saturating_sub(first_blocked_ts_us);
    if elapsed >= 8 * interval_us {
        EscalationTier::T3
    } else if elapsed >= 4 * interval_us {
        EscalationTier::T2
    } else if elapsed >= 2 * interval_us {
        EscalationTier::T1
    } else {
        EscalationTier::T0
    }
}

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
            id: b
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            title: b
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
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
///
/// cg6.3 replay-idempotency: `__respawn` no-ops when an agent with the same
/// name already exists in a healthy state (the guard lives at the side
/// effect). The context_limit path passes `--force`: an idle-but-exhausted
/// agent is healthy and MUST still be respawned — its replay window is
/// documented at `__respawn` (main.rs).
#[function]
async fn respawn_agent(args: RespawnArgs) -> Result<()> {
    let exe = self_exe().map_err(map_err)?;
    let mut cmd = Command::new(&exe);
    cmd.args(["__respawn", &args.project, &args.name, &args.kind]);
    if args.reason == "context_limit" {
        cmd.arg("--force");
    }
    let out = cmd
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__respawn spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__respawn failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    // a requeued prompt's delivery record dies with the respawned agent —
    // invalidate it so the fresh agent gets the prompt (resume-dedup)
    if let Some(h) = args.text_hash {
        let mut records = crate::state::load_last_sent(&args.project);
        records.retain(|r| !(r.name == args.name && r.text_hash == h));
        if let Err(e) = crate::state::save_last_sent(&args.project, &records) {
            eprintln!("flywheel: cannot invalidate last-sent record: {e:#}");
        }
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
    /// true when __send skipped the herdr call because a landed record for
    /// this (name, prompt, invocation) already exists (resume-dedup)
    #[serde(default)]
    pub deduped: bool,
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
    if outcome.deduped {
        // is1.4: the resume-dedup skip is logged HERE (checkpointed) — the
        // body-side log duplicated on replay.
        let _ = crate::state::log_event(
            &args.project,
            crate::state::EventKind::SkippedDup,
            serde_json::json!({
                "name": args.name,
                "iteration": args.iteration,
                "source": "loop",
            }),
        );
    }
    Ok(outcome)
}

/// Current wall-clock in microseconds since the epoch. Only called INSIDE
/// leaves (cg6.1): leaf-side effects are checkpointed by Resonate, so a
/// completed leaf's result is replayed as-is — the body must never read
/// wall-clock directly (that was the replay-determinism bug).
fn now_us_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// One escalation step (pure): derive the tier from the persisted
/// first-blocked timestamp and decide whether to MAIL. Returns
/// (tier 0..3, ping_count, should_mail). cg6.3: the mail is gated on a
/// TIER TRANSITION (`last_ping_tier` marker) — a replay re-execution at the
/// same tier neither re-mails nor double-counts; at most one ping per
/// logical tier transition (ADR-0001 §4.4's T0→T1→T2→T3 cadence).
fn escalation_step(
    st: &mut crate::state::EscalationState,
    now_us: i64,
    interval_secs: u64,
) -> (u32, u32, bool) {
    let t = escalation_tier(st.first_blocked_ts, now_us, interval_secs);
    let u = match t {
        EscalationTier::T0 => 0u32,
        EscalationTier::T1 => 1,
        EscalationTier::T2 => 2,
        EscalationTier::T3 => 3,
    };
    let should_mail = st.last_ping_tier != Some(u);
    if should_mail {
        st.ping_count = st.ping_count.saturating_add(1);
        st.last_ping_tier = Some(u);
    }
    (u, st.ping_count, should_mail)
}

/// Load-or-init the escalation entry, run ONE escalation step, persist.
/// File I/O is legal here: this runs inside the checkpointed `notify_blocked`
/// leaf (cg6.1). The tier derives from the PERSISTED first_blocked_ts (or the
/// seed on a brand-new entry), never from a body-side re-read — replay of an
/// in-flight leaf re-executes against the same persisted value, so the tier
/// cannot jump across a crash boundary. Returns (tier, ping_count,
/// should_mail) — the mail decision comes from the persisted
/// `last_ping_tier` marker (cg6.3), so a replay at the same tier no-ops.
fn escalate_and_persist(
    project: &str,
    name: &str,
    first_blocked_seed: Option<i64>,
    interval_secs: u64,
    params: serde_json::Value,
) -> anyhow::Result<(u32, u32, bool)> {
    let now_us = now_us_micros();
    let mut al = crate::state::load_active_loop(project)?
        .unwrap_or_else(|| crate::state::ActiveLoop::new("swarm-loop", params, crate::state::now_iso()));
    let st = al
        .escalation
        .entry(name.to_string())
        .or_insert_with(|| crate::state::EscalationState {
            first_blocked_ts: first_blocked_seed.unwrap_or(now_us),
            ping_count: 0,
            last_ping_tier: None,
            last_tail: String::new(),
        });
    let (tier, ping_count, should_mail) = escalation_step(st, now_us, interval_secs);
    crate::state::save_active_loop(project, &al)?;
    Ok((tier, ping_count, should_mail))
}

/// Notify a blocked agent via agent-mail. Checkpointed side effect — and
/// (cg6.1) the home of the ENTIRE escalation state machine: it loads or
/// initializes the escalation entry, computes the tier from the persisted
/// first_blocked_ts, increments + persists ping_count, then mails. The
/// workflow body only acts on the returned `EscalationOutcome`.
///
/// At-least-once note (Resonate 0.6.0): a COMPLETED leaf invocation is never
/// re-executed on replay (result checkpointed). For an IN-FLIGHT leaf at
/// crash, re-execution derives from the persisted entry — same
/// first_blocked_ts → same tier, and the `last_ping_tier` marker (cg6.3)
/// suppresses the duplicate mail: at most one ping per logical tier
/// transition.
#[function]
async fn notify_blocked(args: NotifyArgs) -> Result<EscalationOutcome> {
    let (tier, ping_count, should_mail) = escalate_and_persist(
        &args.project,
        &args.name,
        args.first_blocked_ts,
        args.interval_secs,
        args.params,
    )
    .map_err(map_err)?;
    if should_mail {
        let exe = self_exe().map_err(map_err)?;
        let out = Command::new(&exe)
            .args([
                "__mail-blocked",
                &args.project,
                &args.name,
                "--tier",
                &tier.to_string(),
            ])
            .output()
            .map_err(|e| map_err(anyhow::anyhow!("__mail-blocked spawn failed: {e}")))?;
        if !out.status.success() {
            return Err(map_err(anyhow::anyhow!(
                "__mail-blocked failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        if tier == 3 {
            // is1.4: the T3 halt event is logged HERE (checkpointed with the
            // leaf) — the body-side log duplicated on crash-resume replay.
            let _ = crate::state::log_event(
                &args.project,
                crate::state::EventKind::Loop,
                serde_json::json!({
                    "status": "halted",
                    "tier": tier,
                    "name": args.name,
                }),
            );
        }
    }
    Ok(EscalationOutcome { tier, ping_count })
}

/// Scan orchestrator inbox for `[xproj]` requests. Soft-fail empty.
#[function]
async fn poll_xproj_inbox(args: XprojInboxArgs) -> Result<Vec<crate::XprojInboxItem>> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args([
            "__xproj-inbox",
            &args.project,
            "--limit",
            &args.limit.to_string(),
        ])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__xproj-inbox spawn failed: {e}")))?;
    if !out.status.success() {
        return Ok(vec![]);
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(raw.trim()).unwrap_or(json!({ "items": [] }));
    let items = v.get("items").cloned().unwrap_or(json!([]));
    let parsed: Vec<crate::XprojInboxItem> = serde_json::from_value(items).unwrap_or_default();
    Ok(parsed)
}

/// Spawn the swarm for a parsed `[xproj]` request (full loop branch).
/// Delegates to the `__xproj-spawn` leaf which calls `cmd_spawn` + replies
/// in-thread. Soft-fail: spawn errors are surfaced in-thread by the leaf, the
/// loop continues.
#[function]
async fn spawn_xproj(args: XprojSpawnArgs) -> Result<()> {
    // cg6.3: at-most-once per request — mark BEFORE spawning so a crash-
    // window replay no-ops instead of spawning two swarms for one [xproj]
    // message (the loop's body-side seen_xproj set is re-seeded on replay).
    if crate::state::xproj_seen_contains(&args.project, &args.message_id).map_err(map_err)? {
        return Ok(());
    }
    crate::state::xproj_seen_mark(&args.project, &args.message_id).map_err(map_err)?;
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args([
            "__xproj-spawn",
            &args.project,
            &args.message_id,
            &args.thread_id,
            &args.from,
            &args.recipe,
            &args.workers.to_string(),
        ])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__xproj-spawn spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__xproj-spawn failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    // is1.4: the spawn event is logged HERE (checkpointed with the leaf) —
    // the body-side log duplicated on crash-resume replay.
    let _ = crate::state::log_event(
        &args.project,
        crate::state::EventKind::State,
        serde_json::json!({
            "source": "xproj_spawn",
            "message_id": args.message_id,
            "recipe": args.recipe,
            "workers": args.workers,
        }),
    );
    Ok(())
}

/// Diagnose an unresponsive agent: capture recent output + high-importance
/// health mail (bd-event-self-heal). Soft-fail: never kill the loop.
#[function]
async fn diagnose_agent(args: DiagnoseArgs) -> Result<Value> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args([
            "__diagnose",
            &args.project,
            &args.name,
            "--reason",
            &args.reason,
        ])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__diagnose spawn failed: {e}")))?;
    // soft: non-zero still returns JSON if any; empty → ok:false
    let raw = String::from_utf8_lossy(&out.stdout);
    if let Ok(v) = serde_json::from_str::<Value>(raw.trim()) {
        let _ = crate::state::log_event(
            &args.project,
            crate::state::EventKind::State,
            serde_json::json!({
                "source": "diagnose",
                "name": args.name,
                "reason": args.reason,
                "result": &v,
            }),
        );
        return Ok(v);
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Ok(serde_json::json!({
        "ok": false,
        "name": args.name,
        "error": if err.is_empty() { format!("exit {}", out.status) } else { err },
    }))
}

/// Args for the [`renew_leases`] leaf.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenewLeaseArgs {
    pub project: String,
    /// TTL extension per active reservation, in seconds.
    pub extend_seconds: u64,
}

/// Renew the orchestrator's active file reservations (ADR-0001 gap A3,
/// bd-mail-lease-renewal-durable). Checkpointed side effect: the leaf calls
/// `__renew-leases` in a fresh process, so a crash between ticks is resumed
/// idempotently (renewing an already-fresh lease is a no-op). Best-effort by
/// contract: the caller logs a failure and continues — a missed renew is
/// recoverable on the next tick and must never kill an overnight loop.
#[function]
async fn renew_leases(args: RenewLeaseArgs) -> Result<Value> {
    let exe = self_exe().map_err(map_err)?;
    let out = Command::new(&exe)
        .args([
            "__renew-leases",
            &args.project,
            &args.extend_seconds.to_string(),
        ])
        .output()
        .map_err(|e| map_err(anyhow::anyhow!("__renew-leases spawn failed: {e}")))?;
    if !out.status.success() {
        return Err(map_err(anyhow::anyhow!(
            "__renew-leases failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout)
        .map_err(|e| map_err(anyhow::anyhow!("__renew-leases bad JSON: {e}: {stdout}")))
}

/// Lease TTL policy for loop-made reservations: an initial TTL of
/// Renewal extension per heartbeat tick: `4 × heartbeat_interval`, floored at
/// agent-mail's 60s `extend_seconds` minimum — never compute tiny extensions.
pub fn lease_renew_seconds(heartbeat_interval_secs: u64) -> u64 {
    (4 * heartbeat_interval_secs).max(60)
}

/// Pure planning: given current states, decide actions and drain the queue.
/// Deterministic dispatch plan for one poll: working agents wait, blocked
/// agents report, idle/done agents get the next queue item (or a nudge, or
/// wait). `unknown` is NOT dispatchable by design: herdr cannot classify its
/// lifecycle confidently — a prompt could land in a half-dead session and be
/// silently lost. Unknown agents get `Wait` and never consume the queue.
///
/// `DispatchImmediate` is the event-path twin of `Send` (bd-event-dispatch):
/// same `__send` leaf, same FIFO pop, marked so the loop can track
/// dispatch-pending until a `working` event confirms landing.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopAction {
    Send {
        name: String,
        text: String,
    },
    /// Event-wake redispatch: same leaf as `Send`, skips poll batching.
    DispatchImmediate {
        name: String,
        text: String,
    },
    Blocked {
        name: String,
    },
    Nudge {
        name: String,
    },
    /// Self-heal: read tail + high-importance mail (bd-event-self-heal).
    Diagnose {
        name: String,
        reason: String,
    },
    /// Self-heal: `__respawn` after diagnose failed to clear (budgeted).
    Respawn {
        name: String,
        reason: String,
    },
    Wait,
}

pub fn plan_actions(
    states: &[AgentState],
    queue: &mut Vec<String>,
    nudge: Option<&str>,
) -> Vec<LoopAction> {
    let mut actions = Vec::new();
    for s in states {
        match s.status.as_str() {
            "working" => actions.push(LoopAction::Wait),
            "blocked" => actions.push(LoopAction::Blocked {
                name: s.name.clone(),
            }),
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
                    actions.push(LoopAction::Nudge {
                        name: s.name.clone(),
                    });
                } else {
                    actions.push(LoopAction::Wait);
                }
            }
        }
    }
    actions
}

/// Resolve agent herdr name for a pane id from the latest poll snapshot.
pub fn name_for_pane(states: &[AgentState], pane_id: &str) -> Option<String> {
    if pane_id.is_empty() {
        return None;
    }
    states
        .iter()
        .find(|s| s.pane_id == pane_id)
        .map(|s| s.name.clone())
}

/// Event-shaped decision path (bd-event-dispatch-zero-latency).
///
/// Pure: updates `dispatch_pending` and may pop the queue front (FIFO, same
/// as `plan_actions`). Returns at most one action per event.
///
/// Double-dispatch guard: if `name` is already in `dispatch_pending`, a
/// further `done`/`idle` does not re-pop the queue until a `working` event
/// clears the flag (or `clear_dispatch_pending` after send landing).
pub fn route_event(
    states: &[AgentState],
    ev: &PollEvent,
    queue: &mut Vec<String>,
    dispatch_pending: &mut std::collections::HashSet<String>,
) -> Option<LoopAction> {
    let name = name_for_pane(states, &ev.pane_id).or_else(|| {
        // fallback: single-agent swarm with empty pane_id map — match by kind
        // only when exactly one agent shares the event's agent field
        let kind = ev.agent.as_deref()?;
        let matches: Vec<_> = states.iter().filter(|s| s.kind == kind).collect();
        if matches.len() == 1 {
            Some(matches[0].name.clone())
        } else {
            None
        }
    });

    let status = ev.status.as_deref().unwrap_or("");
    match ev.event_type.as_str() {
        "pane.agent_status_changed" => {
            let name = name?;
            match status {
                "working" => {
                    // landing confirmed — clear pending so next done can redispatch
                    dispatch_pending.remove(&name);
                    None
                }
                "blocked" => Some(LoopAction::Blocked { name }),
                "done" | "idle" => {
                    if dispatch_pending.contains(&name) {
                        // flutter: already dispatched, wait for working
                        return None;
                    }
                    if queue.is_empty() {
                        return None;
                    }
                    let text = queue.remove(0);
                    let text = text.replace("{AGENT_NAME}", &name);
                    dispatch_pending.insert(name.clone());
                    Some(LoopAction::DispatchImmediate { name, text })
                }
                _ => None,
            }
        }
        "pane.agent_detected" => {
            // new agent available — dispatch if free and queue non-empty
            let name = name?;
            if dispatch_pending.contains(&name) || queue.is_empty() {
                return None;
            }
            // only if not already working
            if status == "working" {
                return None;
            }
            let text = queue.remove(0);
            let text = text.replace("{AGENT_NAME}", &name);
            dispatch_pending.insert(name.clone());
            Some(LoopAction::DispatchImmediate { name, text })
        }
        "pane.exited" => {
            let name = name.unwrap_or_else(|| format!("pane:{}", ev.pane_id));
            dispatch_pending.remove(&name);
            // Diagnose immediately on exit (bd-event-self-heal subtask 4);
            // the heal state machine escalates to Respawn next tick if needed.
            Some(LoopAction::Diagnose {
                name,
                reason: "pane_exited".into(),
            })
        }
        _ => None,
    }
}

/// Clear dispatch-pending after a confirmed send landing (or fail-stop).
pub fn clear_dispatch_pending(
    pending: &mut std::collections::HashSet<String>,
    name: &str,
    landed: Option<bool>,
) {
    // clear on definitive outcome: landed true, or landed false (will respawn)
    // keep pending only when outcome is ambiguous (landed None on non-confirm)
    match landed {
        Some(true) => {
            // keep until working event? Bead: clear on real landing.
            // working event also clears; clearing here allows faster redispatch
            // if working event is delayed. Prefer clear on landed.
            pending.remove(name);
        }
        Some(false) => {
            pending.remove(name);
        }
        None => {
            // non-confirm path: assume sent, clear so we don't stick forever
            pending.remove(name);
        }
    }
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
    /// 4 consecutive non-landing sends on the same agent (3 respawns, then
    /// the 4th fails): hard stop the loop. A genuinely broken environment
    /// must not burn the swarm forever. (cg6.4: threshold is FOUR — the
    /// counter `escalate` reads is the pre-increment value, so
    /// `failures >= 3` at call time means 4 failures have happened.)
    FailStop,
}

/// The FailStop error message (cg6.4): states the REAL contract — the loop
/// respawns an agent after the first three non-landing sends and stops on
/// the FOURTH. Single source of truth for both halt sites + tests.
pub fn failstop_message(name: &str, reason: Option<&str>) -> String {
    format!(
        "agent {name} failed to accept 4 prompts in a row (last reason: {reason:?})"
    )
}

/// Pure verdict for one confirmed send outcome.
///
/// `landed: true` (incl. a `blocked` status — a valid landing the
/// `--on-blocked` policy owns) → [`Escalation::Accept`]; `landed: None`
/// (legacy fire-and-forget path) → [`Escalation::Accept`]; a `timeout` reason
/// is ambiguous (the prompt may have started) → [`Escalation::Accept`]
/// WITHOUT touching the failure counter; any other non-landing (stalled,
/// `error:<code>`) is a pane-health failure → respawn on the first 3
/// failures; the 4th (`failures >= 3` at call time — the counter is the
/// PRE-increment value, see [`handle_send_result`]) → [`Escalation::FailStop`].
/// Deterministic: derives from the checkpointed
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
pub fn respawn_kind(
    kinds: &std::collections::HashMap<String, String>,
    name: &str,
) -> Option<String> {
    kinds.get(name).cloned()
}

/// Parse one `__poll` entry: `[name, status, kind, pane_id?]`.
/// kind added for respawn; pane_id for event→name routing
/// (bd-event-dispatch). Legacy 2/3-element entries yield empty kind/pane_id.
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
        pane_id: arr
            .get(3)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// Consecutive `unknown` polls before **diagnose** (bd-event-self-heal).
/// Default 3: tolerates brief classification flicker without mail spam.
pub const DEFAULT_UNKNOWN_DIAGNOSE_THRESHOLD: u32 = 3;

/// Max respawns per agent per loop run before escalate-to-blocked.
pub const DEFAULT_RESPAWN_BUDGET: u32 = 2;

/// Legacy alias: old tests/callers used a hard respawn threshold of 2.
/// Per-agent self-heal bookkeeping (bd-event-self-heal-unknown).
/// Survives within a run; optionally mirrored into `loops.json` `respawn`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealState {
    pub unknown_count: u32,
    pub diagnose_count: u32,
    pub respawn_count: u32,
    /// After Diagnose this episode: next still-dead tick → Respawn.
    pub awaiting_clear: bool,
}

/// Record a diagnose outcome in the heal state (cg6.2): only a SUCCESSFUL
/// diagnose advances the episode (increment count + set awaiting_clear); a
/// failed diagnose leaves the state untouched so the next poll retries.
fn apply_diagnose_outcome(hs: &mut HealState, ok: bool) {
    if ok && !hs.awaiting_clear {
        hs.diagnose_count = hs.diagnose_count.saturating_add(1);
        hs.awaiting_clear = true;
    }
}

/// Pure self-heal decision for one agent (table-driven tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealAction {
    None,
    /// Read tail + high-importance mail (thread `health-<name>`).
    Diagnose,
    /// `__respawn` + requeue in-flight work (if any).
    Respawn,
    /// Budget exhausted — surface as Blocked / human escalate.
    EscalateBlocked,
}

/// Pure self-heal step (bd-event-self-heal-unknown).
///
/// | input | action |
/// |-------|--------|
/// | classified (not unknown, not exited) | reset episode counters; keep `respawn_count` |
/// | unknown × diagnose_threshold | Diagnose, set `awaiting_clear` |
/// | unknown while `awaiting_clear` | Respawn (if budget), else EscalateBlocked |
/// | `exited=true` first time | Diagnose immediately |
/// | `exited=true` after diagnose / again | Respawn or EscalateBlocked |
///
/// `respawn_budget` caps Respawn per agent for the whole loop run.
pub fn self_heal_step(
    hs: &mut HealState,
    status: &str,
    exited: bool,
    diagnose_threshold: u32,
    respawn_budget: u32,
) -> HealAction {
    let threshold = diagnose_threshold.max(1);
    let budget = respawn_budget; // 0 = never respawn, escalate immediately

    if !exited && status != "unknown" {
        // proof of life — end episode (budget is lifetime of the run)
        hs.unknown_count = 0;
        hs.awaiting_clear = false;
        return HealAction::None;
    }

    let try_respawn = |hs: &mut HealState| -> HealAction {
        if hs.respawn_count >= budget {
            return HealAction::EscalateBlocked;
        }
        hs.respawn_count = hs.respawn_count.saturating_add(1);
        hs.awaiting_clear = false;
        hs.unknown_count = 0;
        HealAction::Respawn
    };

    if exited {
        // definitive death signal — diagnose once, then respawn
        if !hs.awaiting_clear {
            hs.diagnose_count = hs.diagnose_count.saturating_add(1);
            hs.awaiting_clear = true;
            return HealAction::Diagnose;
        }
        return try_respawn(hs);
    }

    // unknown path
    hs.unknown_count = hs.unknown_count.saturating_add(1);

    if hs.awaiting_clear {
        // diagnosed last tick; still unknown → respawn
        return try_respawn(hs);
    }

    if hs.unknown_count >= threshold {
        hs.diagnose_count = hs.diagnose_count.saturating_add(1);
        hs.awaiting_clear = true;
        return HealAction::Diagnose;
    }

    HealAction::None
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
///
/// cg6.2: leaf errors NEVER abort the loop — on a poll_kind or respawn_agent
/// failure the failure is logged and the queue is left unchanged (the next
/// poll re-derives the agent's state; the send-dedup prevents duplicates).
#[allow(clippy::too_many_arguments)] // the single respawn point: ctx, params, queue,
                                     // kinds, name, text, reason, attempts — every parameter is load-bearing.
async fn respawn_and_requeue(
    ctx: &Context,
    params: &LoopParams,
    queue: &mut Vec<String>,
    kinds: &std::collections::HashMap<String, String>,
    name: &str,
    text: Option<&str>,
    reason: &str,
    attempts: u64,
) {
    let kind = match respawn_kind(kinds, name) {
        Some(k) => k,
        None => match ctx
            .run(
                poll_kind,
                PollKindArgs {
                    project: params.project.clone(),
                    name: name.to_string(),
                },
            )
            .await
        {
            Ok(k) => k,
            Err(e) => {
                // cg6.2: skip the respawn this tick; queue unchanged.
                eprintln!("flywheel: poll_kind leaf failed for {name} (skipping respawn): {e}");
                return;
            }
        },
    };
    if let Err(e) = ctx
        .run(
            respawn_agent,
            RespawnArgs {
                project: params.project.clone(),
                name: name.to_string(),
                kind,
                reason: reason.to_string(),
                attempts,
                text_hash: text.map(crate::state::fnv1a64),
            },
        )
        .await
    {
        // cg6.2: herdr hiccup during respawn must not kill the loop — the
        // prompt is NOT requeued (the respawn may not have happened; the
        // next poll re-evaluates the agent's state).
        eprintln!("flywheel: respawn_agent leaf failed for {name} (continuing): {e}");
        return;
    }
    if let Some(t) = text {
        queue.insert(0, t.to_string());
    }
}

/// The durable monitoring workflow. Runs until the queue drains and the swarm
/// settles, or a blocked agent stops it (policy).
///
/// ## Leaf-error policy (cg6.2)
/// Leaf errors NEVER abort the loop. Every `ctx.run(...)` outcome below is
/// converted to a soft default (logged via eprintln; the next poll is the
/// correctness floor). A transient herdr/mail hiccup — an agent exiting
/// between poll and read, herdr briefly down during a respawn, a daemon
/// blip on notify — degrades that tick, never kills the run. Genuinely
/// fatal conditions (config errors, Resonate unreachable) live OUTSIDE the
/// leaf boundary and still hard-fail.
///
/// Process-per-leaf cost (durable review #8): each iteration spawns 3-8
/// `flywheel` subprocesses (poll_events + poll_swarm + xproj_inbox +
/// renew_leases + per-dispatch read_output/send_prompt), ~7ms (clap-only) to
/// ~14ms (config+backend init) each — 60-120ms per iteration, negligible at
/// the 30-300s cadence. This boundary is the CRASH-ISOLATION design (a leaf
/// panic cannot kill the loop's memory) — do not "optimize" it away without
/// a crash-isolation replacement.
#[function]
async fn swarm_loop(ctx: &Context, params: LoopParams) -> Result<String> {
    let mut queue = params.queue.clone();
    let mut iteration = 0u64;
    // bead-driven mode tracks which beads were already handed out
    let mut assigned: Vec<String> = Vec::new();
    // per-agent consecutive send-failure count (send-stall escalation).
    // Workflow-local and deterministic: rebuilt identically on replay from
    // the checkpointed leaf results (poll states + send outcomes).
    let mut send_failures: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    // per-agent herdr kind learned from polls — needed to respawn an agent
    // whose record has been dropped (the escalation path).
    let mut agent_kinds: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    // per-agent self-heal bookkeeping (bd-event-self-heal-unknown). Deterministic
    // across replay via checkpointed leaf results that drive the same steps.
    let mut heal_states: std::collections::HashMap<String, HealState> =
        std::collections::HashMap::new();
    // pane_exited names observed this iteration (event path feeds heal step)
    let mut exited_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    // best-effort in-run event cursor (bd-event-subscribe-reactor). NOT used
    // for crash recovery — heartbeat __poll is the floor. Survives within a
    // single workflow run via local state; loops.json event_cursor is optional.
    let mut event_cursor_seq: u64 = 0;
    // consecutive __poll_events stream errors → force-log WARN at 3 (bead §6)
    let mut event_stream_errors: u64 = 0;
    // adaptive wait for the next poll_events block (starts active)
    let mut wait_secs: u64 = params.active_interval_secs.max(5);
    // agents that received DispatchImmediate and await working confirmation
    // (bd-event-dispatch double-dispatch guard)
    let mut dispatch_pending: std::collections::HashSet<String> = std::collections::HashSet::new();
    // cross-project [xproj] message ids already queued (avoid re-enqueue).
    // String: toron ids are hex event ids; agent-mail ids normalize to string.
    let mut seen_xproj: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        iteration += 1;
        if let Some(max) = params.max_iterations
            && iteration > max
        {
            return Err(map_err(anyhow::anyhow!(
                "loop timed out after {max} polls (queue remaining: {})",
                queue.len()
            )));
        }

        // ---- event wake (latency optimization, forward-only) ----
        // Blocks up to wait_secs collecting push events. Empty batch on
        // idle/timeout is normal. Stream errors soft-fail into heartbeat.
        let event_batch: PollEventsBatch = match ctx
            .run(
                poll_events,
                PollEventsArgs {
                    project: params.project.clone(),
                    timeout_ms: wait_secs.saturating_mul(1000),
                    after_seq: event_cursor_seq,
                },
            )
            .await
        {
            Ok(b) => b,
            Err(e) => {
                // cg6.2: leaf errors never abort the loop — a transport
                // hiccup degrades to the heartbeat fallback this tick.
                eprintln!("flywheel: poll_events leaf failed (continuing): {e}");
                PollEventsBatch {
                    events: vec![],
                    max_seq: 0,
                    stream_error: Some(format!("leaf error: {e}")),
                }
            }
        };
        if event_batch.stream_error.is_some() {
            event_stream_errors = event_stream_errors.saturating_add(1);
            if event_stream_errors == 3 {
                eprintln!(
                    "flywheel: WARN event stream may be dead; heartbeat fallback active ({})",
                    event_batch.stream_error.as_deref().unwrap_or("?")
                );
            }
        } else {
            event_stream_errors = 0;
        }
        if event_batch.max_seq > event_cursor_seq {
            event_cursor_seq = event_batch.max_seq;
        }
        // (is1.4: the event batch is logged INSIDE the poll_events leaf —
        // checkpointed; a body-side log would duplicate on replay.)

        // ---- correctness floor: authoritative agent.list snapshot ----
        // Always runs. Catches any state change missed during a crash gap
        // (events are not replayed). Full routing of event_batch is owned by
        // bd-event-dispatch-zero-latency; here events only wake the loop early.
        let states: Vec<AgentState> = match ctx
            .run(
                poll_swarm,
                PollArgs {
                    project: params.project.clone(),
                    to: params.to.clone(),
                },
            )
            .await
        {
            Ok(s) => s,
            Err(e) => {
                // cg6.2: the correctness floor must not be a crash point —
                // a transient herdr failure yields an empty snapshot; the
                // next tick re-polls.
                eprintln!("flywheel: poll_swarm leaf failed (continuing with empty snapshot): {e}");
                vec![]
            }
        };

        // proof of life: a working/blocked agent is receiving prompts — its
        // failure history is stale (landed sends reset at the send site too)
        clear_live_failures(&mut send_failures, &states);
        // learn agent kinds for the respawn path (idempotent, never removes)
        learn_kinds(&mut agent_kinds, &states);

        // ---- cross-project receive (bd-pattern-cross-project-swarm §9.5) ----
        // Scan orchestrator inbox for [xproj] requests. For each unseen item,
        // PARSE + SPAWN DIRECTLY (full loop branch): if the request carries a
        // recipe, call `__xproj-spawn` which spawns the swarm in this project
        // and replies in-thread. If it doesn't parse to a recipe, fall back to
        // queueing a prompt for an idle agent to interpret. Soft-fail.
        let xproj_items: Vec<crate::XprojInboxItem> = ctx
            .run(
                poll_xproj_inbox,
                XprojInboxArgs {
                    project: params.project.clone(),
                    limit: 30,
                },
            )
            .await
            .unwrap_or_default();
        for item in &xproj_items {
            if seen_xproj.contains(&item.id) {
                continue;
            }
            seen_xproj.insert(item.id.clone());
            let combined = format!("{} {}", item.subject, item.body);
            let req = crate::parse_xproj_request(&combined);
            let action = if let Some(r) = req.filter(|r| !r.recipe.is_empty()) {
                // FULL BRANCH: spawn the requested swarm directly.
                let spawned = ctx
                    .run(
                        spawn_xproj,
                        XprojSpawnArgs {
                            project: params.project.clone(),
                            message_id: item.id.clone(),
                            thread_id: item.thread_id.clone(),
                            from: item.from.clone(),
                            recipe: r.recipe.clone(),
                            workers: r.workers,
                        },
                    )
                    .await;
                if let Err(e) = &spawned {
                    eprintln!("flywheel: __xproj-spawn failed for msg {}: {e}", item.id);
                }
                "spawned"
            } else {
                // FALLBACK: an agent interprets the unparseable request.
                let prompt = crate::xproj_queue_prompt(
                    item,
                    &crate::XprojRequest {
                        recipe: String::new(),
                        workers: 2,
                        params: combined.clone(),
                        product: String::new(),
                    },
                );
                queue.push(prompt);
                "queued"
            };
            // is1.4: the spawned/queued outcome is NOT logged body-side —
            // spawn_xproj logs its own (checkpointed) event; the queued case
            // is visible via the queue + this one-shot eprintln.
            eprintln!("flywheel: cross-project request msg {} {action}", item.id);
        }

        // self-heal (bd-event-self-heal): diagnose → respawn (budgeted) → blocked.
        // Skip-dispatch for unknown still holds in plan_actions; heal runs here.
        exited_names.clear();
        for ev in &event_batch.events {
            if ev.event_type == "pane.exited" {
                if let Some(n) = name_for_pane(&states, &ev.pane_id) {
                    exited_names.insert(n);
                } else if let Some(a) = &ev.agent {
                    // best-effort: single-kind match
                    let hits: Vec<_> = states.iter().filter(|s| s.kind == *a).collect();
                    if hits.len() == 1 {
                        exited_names.insert(hits[0].name.clone());
                    }
                }
            }
        }
        let mut heal_actions: Vec<LoopAction> = Vec::new();
        for s in &states {
            let exited = exited_names.contains(&s.name);
            let hs = heal_states.entry(s.name.clone()).or_default();
            match self_heal_step(
                hs,
                &s.status,
                exited,
                params.unknown_diagnose_threshold,
                params.respawn_budget,
            ) {
                HealAction::None => {}
                HealAction::Diagnose => {
                    let reason = if exited {
                        "pane_exited"
                    } else {
                        "unknown-stuck"
                    };
                    heal_actions.push(LoopAction::Diagnose {
                        name: s.name.clone(),
                        reason: reason.into(),
                    });
                }
                HealAction::Respawn => {
                    heal_actions.push(LoopAction::Respawn {
                        name: s.name.clone(),
                        reason: if exited {
                            "pane_exited".into()
                        } else {
                            "unknown-stuck".into()
                        },
                    });
                }
                HealAction::EscalateBlocked => {
                    heal_actions.push(LoopAction::Blocked {
                        name: s.name.clone(),
                    });
                    eprintln!(
                        "flywheel: WARN agent {} exceeded respawn budget ({}); escalating blocked",
                        s.name, params.respawn_budget
                    );
                }
            }
        }

        // bead-driven mode: refresh the instruction queue from the tracker each
        // poll; the assigned-set is deterministic across replays (derives from
        // checkpointed poll results)
        if params.work {
            // interrupted work first: an in-progress bead's agent went idle —
            // resume it before handing out fresh beads
            let inprog: Vec<ReadyBead> = match ctx
                .run(
                    poll_inprogress,
                    ReadyArgs {
                        project: params.project.clone(),
                    },
                )
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    // cg6.2: leaf errors never abort the loop — a tracker
                    // hiccup yields no interrupted-work refresh this tick.
                    eprintln!("flywheel: poll_inprogress leaf failed (continuing): {e}");
                    vec![]
                }
            };
            let to_resume: Vec<&ReadyBead> = inprog
                .iter()
                .filter(|b| !assigned.contains(&b.id))
                .collect();
            for bead in to_resume {
                queue.push(resume_prompt(&bead.id, &bead.title));
                assigned.push(bead.id.clone());
            }

            let ready: Vec<ReadyBead> = match ctx
                .run(
                    poll_ready,
                    ReadyArgs {
                        project: params.project.clone(),
                    },
                )
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    // cg6.2: leaf errors never abort the loop.
                    eprintln!("flywheel: poll_ready leaf failed (continuing): {e}");
                    vec![]
                }
            };
            let to_assign: Vec<&ReadyBead> =
                ready.iter().filter(|b| !assigned.contains(&b.id)).collect();
            for bead in to_assign {
                queue.push(bead_prompt(&bead.id, &bead.title));
                assigned.push(bead.id.clone());
            }
        }

        let mut blocked_names: Vec<String> = Vec::new();
        // event path first (zero-latency redispatch), then heartbeat plan_actions
        let mut actions: Vec<LoopAction> = heal_actions;
        for ev in &event_batch.events {
            if let Some(a) = route_event(&states, ev, &mut queue, &mut dispatch_pending) {
                // pane.exited Diagnose is already covered by self_heal_step;
                // skip duplicate Diagnose from route_event when heal ran.
                if matches!(&a, LoopAction::Diagnose { name, reason }
                    if reason == "pane_exited" && exited_names.contains(name))
                {
                    continue;
                }
                if let LoopAction::Blocked { name } = &a {
                    blocked_names.push(name.clone());
                }
                actions.push(a);
            }
        }
        // heartbeat path: full-snapshot reconciliation (skips agents already
        // dispatch-pending so event path and poll path don't double-feed)
        let mut poll_actions = plan_actions(&states, &mut queue, params.nudge.as_deref());
        poll_actions.retain(|a| match a {
            LoopAction::Send { name, .. } => !dispatch_pending.contains(name),
            _ => true,
        });
        // re-add Send names that survived to pending so event flutter is covered
        for a in &poll_actions {
            if let LoopAction::Send { name, .. } = a {
                dispatch_pending.insert(name.clone());
            }
            if let LoopAction::Blocked { name } = a {
                blocked_names.push(name.clone());
            }
        }
        actions.extend(poll_actions);

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
                LoopAction::Send { name, text } | LoopAction::DispatchImmediate { name, text } => {
                    // context guard: if the idle agent is context-exhausted,
                    // respawn a fresh instance instead of delegating to it
                    let verdict = match ctx
                        .run(
                            read_output,
                            ReadArgs {
                                project: params.project.clone(),
                                name: name.clone(),
                            },
                        )
                        .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            // cg6.2: the agent may have exited between poll
                            // and read (a real race) — skip the context guard;
                            // the send path below soft-fails if the agent is
                            // gone. A leaf error must never abort the loop.
                            eprintln!(
                                "flywheel: read_output leaf failed for {name} (continuing): {e}"
                            );
                            ContextVerdict::Healthy
                        }
                    };
                    if verdict == ContextVerdict::Exhausted {
                        clear_dispatch_pending(&mut dispatch_pending, name, Some(false));
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
                        .await; // cg6.2: soft — leaf failures logged inside
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
                                invocation: params.invocation.clone(),
                                iteration,
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
                            deduped: false,
                        },
                    };
                    clear_dispatch_pending(&mut dispatch_pending, name, outcome.landed);
                    // resume-dedup: __send skipped the herdr call because this
                    // prompt already landed for this invocation (is1.4: the
                    // SkippedDup event is logged INSIDE the send_prompt leaf —
                    // checkpointed; the landed handling below resets the
                    // failure counter; a skipped item never fails)
                    any_work = true;
                    match handle_send_result(&mut send_failures, name, &outcome) {
                        Escalation::Accept => {}
                        Escalation::FailStop => {
                            return Err(map_err(anyhow::anyhow!(
                                "{}",
                                failstop_message(name, outcome.reason.as_deref())
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
                            .await; // cg6.2: soft — leaf failures logged inside
                        }
                    }
                }
                LoopAction::Blocked { name } => {
                    if params.on_blocked == "mail" {
                        // cg6.1: the ENTIRE escalation state machine lives in
                        // the notify_blocked leaf (checkpointed by Resonate) —
                        // it loads/initializes the escalation entry, computes
                        // the tier from the PERSISTED first_blocked_ts,
                        // increments + persists ping_count, and sends the
                        // tiered mail. The body is pure orchestration: no
                        // wall-clock reads, no escalation file I/O, no counter
                        // mutation — crash-resume replay reproduces the
                        // original tier sequence (a completed leaf result is
                        // replayed, never re-executed; even an in-flight
                        // re-execution derives from persisted state, so the
                        // tier cannot jump).
                        let out = match ctx
                            .run(
                                notify_blocked,
                                NotifyArgs {
                                    project: params.project.clone(),
                                    name: name.clone(),
                                    first_blocked_ts: None,
                                    interval_secs: params.interval_secs,
                                    params: serde_json::to_value(&params)
                                        .unwrap_or(serde_json::Value::Null),
                                },
                            )
                            .await
                        {
                            Ok(o) => o,
                            Err(e) => {
                                // cg6.2: leaf errors never abort the loop — a
                                // mail daemon hiccup skips this tick's ping;
                                // escalation state is untouched (the leaf
                                // persists only on success) and the next poll
                                // retries.
                                eprintln!(
                                    "flywheel: notify_blocked leaf failed for {name} (continuing): {e}"
                                );
                                continue;
                            }
                        };
                        let tier_enum = match out.tier {
                            0 => EscalationTier::T0,
                            1 => EscalationTier::T1,
                            2 => EscalationTier::T2,
                            _ => EscalationTier::T3,
                        };
                        // review-fix F4: bounded halt at T3 — the escalation
                        // contract says the loop halts (bounded) once a blocked
                        // agent exceeds the budget. The T3 mail was sent by the
                        // leaf; exit with a distinguishable outcome so
                        // `flywheel loop` and the event log show WHY it stopped
                        // (vs a normal "complete"). T0/T1/T2 keep notifying.
                        if let Some(msg) = on_blocked_tier_exit(tier_enum, &params.on_blocked) {
                            // (is1.4: the halted event is logged INSIDE the
                            // notify_blocked leaf at T3 — checkpointed, so a
                            // crash-resume cannot double-log the halt.)
                            return Ok(msg);
                        }
                    }
                }
                LoopAction::Nudge { name } => {
                    if let Some(n) = &params.nudge {
                        // same failure contract as the Send path: a leaf error
                        // (agent dropped mid-wait) must NOT kill the loop —
                        // escalate to a respawn so the fresh instance gets
                        // nudged next poll. A nudge consumes nothing from the
                        // queue, so there is nothing to requeue.
                        let outcome = match ctx
                            .run(
                                send_prompt,
                                SendArgs {
                                    project: params.project.clone(),
                                    name: name.clone(),
                                    text: n.clone(),
                                    confirm: params.confirm_sends,
                                    confirm_timeout_ms: params.confirm_timeout_ms,
                                    invocation: params.invocation.clone(),
                                    iteration,
                                },
                            )
                            .await
                        {
                            Ok(o) => o,
                            Err(e) => SendOutcome {
                                ok: false,
                                name: name.clone(),
                                landed: Some(false),
                                status: None,
                                reason: Some(format!("error:{e}")),
                                deduped: false,
                            },
                        };
                        any_work = true;
                        match handle_send_result(&mut send_failures, name, &outcome) {
                            Escalation::Accept => {}
                            Escalation::FailStop => {
                                return Err(map_err(anyhow::anyhow!(
                                    "{}",
                                    failstop_message(name, outcome.reason.as_deref())
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
                                    None,
                                    reason,
                                    attempts,
                                )
                                .await; // cg6.2: soft — leaf failures logged inside
                            }
                        }
                    }
                }
                LoopAction::Diagnose { name, reason } => {
                    let diag = ctx
                        .run(
                            diagnose_agent,
                            DiagnoseArgs {
                                project: params.project.clone(),
                                name: name.clone(),
                                reason: reason.clone(),
                            },
                        )
                        .await;
                    let hs = heal_states.entry(name.clone()).or_default();
                    match diag {
                        Ok(_) => {
                            // mark heal state awaiting_clear so next still-dead tick respawns
                            apply_diagnose_outcome(hs, true);
                        }
                        Err(e) => {
                            // cg6.2: a failed diagnose must not advance heal
                            // state (herdr hiccup ≠ agent healed) — the next
                            // poll retries the diagnose.
                            eprintln!(
                                "flywheel: diagnose_agent leaf failed for {name} (heal state untouched): {e}"
                            );
                        }
                    }
                    any_work = true;
                }
                LoopAction::Respawn { name, reason } => {
                    let attempts = heal_states
                        .get(name)
                        .map(|h| u64::from(h.respawn_count))
                        .unwrap_or(1);
                    respawn_and_requeue(
                        ctx,
                        &params,
                        &mut queue,
                        &agent_kinds,
                        name,
                        None,
                        reason,
                        attempts,
                    )
                    .await; // cg6.2: soft — leaf failures logged inside
                    any_work = true;
                }
                LoopAction::Wait => {}
            }
        }

        // work mode completion guard: a stranded in-progress bead (interrupted
        // agent) must keep the loop alive — re-prompt it next poll.
        let inprogress_empty = if params.work {
            match ctx
                .run(
                    poll_inprogress,
                    ReadyArgs {
                        project: params.project.clone(),
                    },
                )
                .await
            {
                Ok(bs) => bs.is_empty(),
                Err(e) => {
                    // cg6.2: a leaf hiccup must not crash the completion
                    // check — treat as non-empty so the loop keeps polling.
                    eprintln!("flywheel: poll_inprogress leaf failed (completion guard): {e}");
                    false
                }
            }
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

        // Lease renewal heartbeat (bd-mail-lease-renewal-durable): extend the
        // orchestrator's own file reservations by 4× the base interval each
        // tick. Best-effort BY DESIGN — agent-mail being unreachable must not
        // kill an overnight run; a missed renew is recoverable next tick
        // (idempotent: renewing a fresh lease is a no-op). `ctx.run` makes it
        // a checkpointed side effect, so crash-resume renews without
        // duplicating reservations.
        match ctx
            .run(
                renew_leases,
                RenewLeaseArgs {
                    project: params.project.clone(),
                    extend_seconds: lease_renew_seconds(params.interval_secs),
                },
            )
            .await
        {
            Ok(v) => {
                let renewed = v.get("renewed").and_then(Value::as_u64).unwrap_or(0);
                // is1.4: quiet at idle — only print when something renewed.
                if renewed > 0 {
                    eprintln!("flywheel: renewed {renewed} lease(s)");
                }
            }
            Err(e) => eprintln!("flywheel: lease renew failed (continuing): {e}"),
        }

        // Adaptive wait for the NEXT poll_events block. We already blocked
        // inside poll_events for wait_secs (or woke early on events), so there
        // is no trailing ctx.sleep — that would double the idle latency.
        // Resonate still checkpoints each leaf; crash mid-wait re-runs the
        // leaf (fresh forward-only subscription — correct).
        wait_secs = next_interval_secs(
            &states,
            any_work,
            params.interval_secs,
            params.active_interval_secs,
        );
        // if events woke us, tighten the next wait so we re-sample promptly
        if !event_batch.events.is_empty() {
            wait_secs = wait_secs.min(params.active_interval_secs).max(5);
        }
    }
}

/// Register the durable functions and start (or resume) the loop invocation.
/// Returns the workflow result; the process must stay alive while awaiting.
pub async fn run_durable_loop(cfg: &Config, mut params: LoopParams) -> anyhow::Result<String> {
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
        .register(poll_events)
        .map_err(|e| anyhow::anyhow!("register poll_events: {e}"))?;
    resonate
        .register(send_prompt)
        .map_err(|e| anyhow::anyhow!("register send_prompt: {e}"))?;
    resonate
        .register(notify_blocked)
        .map_err(|e| anyhow::anyhow!("register notify_blocked: {e}"))?;
    resonate
        .register(diagnose_agent)
        .map_err(|e| anyhow::anyhow!("register diagnose_agent: {e}"))?;
    resonate
        .register(poll_xproj_inbox)
        .map_err(|e| anyhow::anyhow!("register poll_xproj_inbox: {e}"))?;
    resonate
        .register(spawn_xproj)
        .map_err(|e| anyhow::anyhow!("register spawn_xproj: {e}"))?;
    resonate
        .register(renew_leases)
        .map_err(|e| anyhow::anyhow!("register renew_leases: {e}"))?;
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
                &crate::state::ActiveLoop::new(
                    fresh.clone(),
                    serde_json::to_value(&params)?,
                    crate::state::now_iso(),
                ),
            )?;
            fresh
        }
    };

    // the workflow must know its own invocation id for the last-sent registry
    // (bd-resume-dedup filters records to the SAME invocation)
    params.invocation = id.clone();

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
            pane_id: String::new(),
        }
    }

    fn st_pane(name: &str, status: &str, pane: &str) -> AgentState {
        AgentState {
            name: name.to_string(),
            status: status.to_string(),
            kind: "opencode".to_string(),
            pane_id: pane.to_string(),
        }
    }

    fn pev(event_type: &str, pane: &str, status: &str, seq: u64) -> PollEvent {
        PollEvent {
            event_type: event_type.to_string(),
            pane_id: pane.to_string(),
            agent: Some("opencode".to_string()),
            status: Some(status.to_string()),
            seq,
        }
    }

    #[test]
    fn idle_agent_pops_queue() {
        let states = vec![st("a", "idle")];
        let mut q = vec!["p1".to_string(), "p6".to_string()];
        let actions = plan_actions(&states, &mut q, None);
        assert_eq!(q.len(), 1);
        assert!(
            matches!(&actions[0], LoopAction::Send { name, text } if name == "a" && text == "p1")
        );
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
    fn empty_swarm_is_settled() {
        // An EMPTY state list is settled: the poll_swarm soft-fail (swarm
        // killed / never spawned -> pass an empty Vec) settles the loop
        // gracefully instead of crashing the durable invocation with
        // "__poll failed". This is the invariant the soft-fail relies on.
        assert!(all_settled(&[][..]));
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
            deduped: false,
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

    /// cg6.4: the FailStop message must state the REAL threshold (4 prompts),
    /// matching the pre-increment counter semantics — message, doc and code
    /// agree on one number.
    #[test]
    fn failstop_message_states_real_contract() {
        let m = failstop_message("agent-a", Some("stalled"));
        assert!(m.contains("agent-a"));
        assert!(
            m.contains("4 prompts in a row"),
            "message must match the actual 4-failure threshold: {m}"
        );
        assert!(!m.contains("3 prompts"), "no stale count in message: {m}");
        let m2 = failstop_message("agent-b", None);
        assert!(m2.contains("4 prompts in a row"));
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
        assert_eq!(
            escalate(&so(Some(false), Some("timeout")), 0),
            Escalation::Accept
        );
        assert_eq!(
            escalate(&so(Some(false), Some("timeout")), 9),
            Escalation::Accept
        );
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
        let states = vec![
            st("work", "working"),
            st("block", "blocked"),
            st("idle", "idle"),
        ];
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
        let low = AgentStateFile {
            context_pct: 79,
            ts: ts.into(),
        };
        let at = AgentStateFile {
            context_pct: 80,
            ts: ts.into(),
        };
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
        let state = AgentStateFile {
            context_pct: 85,
            ts: ts.into(),
        };
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

    // ---- lease renewal (bd-mail-lease-renewal-durable) ----

    /// Both spawn prompts carry the self-renew clause: agents renew their own
    /// reservations (the orchestrator cannot renew on their behalf).
    #[test]
    fn spawn_prompts_carry_lease_renew_clause() {
        for p in [
            bead_prompt("bd-12", "fix auth flow"),
            resume_prompt("bd-7", "finish the parser"),
        ] {
            assert!(
                p.contains("Renew your own agent-mail file reservations"),
                "prompt must instruct agents to renew their own leases: {p}"
            );
            assert!(
                p.contains("cannot renew on your behalf"),
                "prompt must explain the orchestrator cannot renew for agents: {p}"
            );
        }
    }

    /// bd-fug: ack contract in bead_prompt + resume_prompt (grep-verifiable).
    #[test]
    fn spawn_prompts_carry_ack_contract_clause() {
        for p in [
            bead_prompt("bd-12", "fix auth flow"),
            resume_prompt("bd-7", "finish the parser"),
        ] {
            assert!(
                p.contains("ack_required=true"),
                "prompt must mention ack_required: {p}"
            );
            assert!(
                p.contains("acknowledge_message"),
                "prompt must require acknowledge_message: {p}"
            );
            assert!(
                p.contains("cannot ack on your behalf"),
                "prompt must state orchestrator cannot ack: {p}"
            );
        }
    }

    /// Renewal extension per tick: `4 × heartbeat`, floored at agent-mail's
    /// 60s minimum — never compute tiny extensions.
    #[test]
    fn lease_renew_extension_floor_and_scale() {
        assert_eq!(lease_renew_seconds(300), 1200);
        assert_eq!(lease_renew_seconds(1000), 4000);
        assert_eq!(lease_renew_seconds(30), 120);
        // below the 60s floor → floor
        assert_eq!(lease_renew_seconds(10), 60);
        assert_eq!(lease_renew_seconds(0), 60);
    }

    /// The leaf args serialize/deserialize round-trip (Resonate requires
    /// serializable leaf params; simple identifier params per the SDK rule).
    #[test]
    fn renew_lease_args_serde_roundtrip() {
        let a = RenewLeaseArgs {
            project: "proj-a".into(),
            extend_seconds: 1200,
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["project"], "proj-a");
        assert_eq!(v["extend_seconds"], 1200);
        let back: RenewLeaseArgs = serde_json::from_value(v).unwrap();
        assert_eq!(back.project, a.project);
        assert_eq!(back.extend_seconds, a.extend_seconds);
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
            invocation: "inv-1".into(),
            unknown_diagnose_threshold: 3,
            respawn_budget: 2,
        };
        let v = serde_json::to_value(&p).unwrap();
        let back: LoopParams = serde_json::from_value(v).unwrap();
        assert_eq!(back.project, "notetoself");
        assert_eq!(back.interval_secs, 300);
        assert_eq!(back.active_interval_secs, 15);
        assert_eq!(back.max_iterations, Some(10));
        assert!(back.confirm_sends);
        assert_eq!(back.confirm_timeout_ms, Some(45_000));
        assert_eq!(back.invocation, "inv-1");
        assert_eq!(back.unknown_diagnose_threshold, 3);
        assert_eq!(back.respawn_budget, 2);
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
        assert!(
            p.confirm_sends,
            "confirm_sends must default true for legacy params"
        );
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
            invocation: "inv-1".into(),
            iteration: 3,
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
            invocation: "inv-1".into(),
            iteration: 3,
        };
        assert_eq!(
            send_prompt_argv(&args),
            vec![
                "__send",
                "p",
                "a",
                "t",
                "--confirm",
                "45000",
                "--invocation",
                "inv-1",
                "--iteration",
                "3",
            ]
        );
        // confirm + no timeout → bare `--confirm` (leaf applies its 45s default)
        let args_no_ms = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: true,
            confirm_timeout_ms: None,
            invocation: "inv-1".into(),
            iteration: 3,
        };
        assert_eq!(
            send_prompt_argv(&args_no_ms),
            vec![
                "__send",
                "p",
                "a",
                "t",
                "--confirm",
                "--invocation",
                "inv-1",
                "--iteration",
                "3",
            ]
        );
        // no confirm → legacy argv plus the registry flags
        let args_legacy = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: false,
            confirm_timeout_ms: None,
            invocation: "inv-1".into(),
            iteration: 3,
        };
        assert_eq!(
            send_prompt_argv(&args_legacy),
            vec![
                "__send",
                "p",
                "a",
                "t",
                "--invocation",
                "inv-1",
                "--iteration",
                "3",
            ]
        );
    }

    #[test]
    fn send_prompt_argv_omits_registry_flags_when_invocation_empty() {
        // pre-registry params (empty invocation) → byte-identical to the
        // legacy argv
        let args = SendArgs {
            project: "p".into(),
            name: "a".into(),
            text: "t".into(),
            confirm: true,
            confirm_timeout_ms: Some(45_000),
            invocation: String::new(),
            iteration: 3,
        };
        assert_eq!(
            send_prompt_argv(&args),
            vec!["__send", "p", "a", "t", "--confirm", "45000"]
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
        let legacy: SendOutcome = serde_json::from_str(r#"{"ok": true, "name": "a"}"#).unwrap();
        assert_eq!(legacy.landed, None);
        assert_eq!(legacy.reason, None);
        assert!(!legacy.deduped);
    }

    #[test]
    fn send_outcome_parses_deduped_flag() {
        // the resume-dedup skip shape: landed with deduped:true
        let v: SendOutcome = serde_json::from_str(
            r#"{"ok": true, "name": "a", "landed": true, "status": null, "reason": "landed", "deduped": true}"#,
        )
        .unwrap();
        assert_eq!(v.landed, Some(true));
        assert!(v.deduped);
    }

    #[test]
    fn learn_kinds_from_poll_records_kinds() {
        let mut kinds = std::collections::HashMap::new();
        let states = vec![
            AgentState {
                name: "a".into(),
                status: "idle".into(),
                kind: "pi".into(),
                pane_id: String::new(),
            },
            AgentState {
                name: "b".into(),
                status: "working".into(),
                kind: "opencode".into(),
                pane_id: String::new(),
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
            pane_id: String::new(),
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
        let v: serde_json::Value = serde_json::from_str(r#"["a", "idle", "pi"]"#).unwrap();
        let s = parse_poll_entry(&v).unwrap();
        assert_eq!(s.name, "a");
        assert_eq!(s.status, "idle");
        assert_eq!(s.kind, "pi");
        let legacy: serde_json::Value = serde_json::from_str(r#"["a", "idle"]"#).unwrap();
        let s = parse_poll_entry(&legacy).unwrap();
        assert_eq!(s.kind, "");
    }

    // ---- bd-event-subscribe-reactor: __poll_events batch parse ----

    #[test]
    fn parse_poll_events_batch_happy_path() {
        let raw = r#"{
            "events": [
                {"event_type":"pane.agent_status_changed","pane_id":"%1","agent":"opencode","status":"done","seq":1},
                {"event_type":"pane.agent_status_changed","pane_id":"%2","agent":null,"status":"idle","seq":2}
            ],
            "max_seq": 2,
            "stream_error": null
        }"#;
        let batch = parse_poll_events_batch(raw, 0).unwrap();
        assert_eq!(batch.events.len(), 2);
        assert_eq!(batch.max_seq, 2);
        assert!(batch.stream_error.is_none());
        assert_eq!(batch.events[0].pane_id, "%1");
        assert_eq!(batch.events[0].status.as_deref(), Some("done"));
        assert_eq!(batch.events[1].agent, None);
    }

    #[test]
    fn parse_poll_events_batch_empty_and_error() {
        let raw = r#"{"events":[],"max_seq":7,"stream_error":"subscribe failed: down"}"#;
        let batch = parse_poll_events_batch(raw, 0).unwrap();
        assert!(batch.events.is_empty());
        assert_eq!(batch.max_seq, 7);
        assert_eq!(
            batch.stream_error.as_deref(),
            Some("subscribe failed: down")
        );
    }

    #[test]
    fn parse_poll_events_batch_rejects_garbage() {
        assert!(parse_poll_events_batch("{not json", 0).is_err());
    }

    // ---- bd-event-dispatch-zero-latency: route_event ----

    #[test]
    fn route_event_done_dispatches_and_pops_queue() {
        let states = vec![st_pane("a", "done", "%1")];
        let mut queue = vec!["p1".into(), "p2".into()];
        let mut pending = std::collections::HashSet::new();
        let a = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "done", 1),
            &mut queue,
            &mut pending,
        );
        assert!(matches!(
            a,
            Some(LoopAction::DispatchImmediate { name, text }) if name == "a" && text == "p1"
        ));
        assert_eq!(queue, vec!["p2".to_string()]);
        assert!(pending.contains("a"));
    }

    #[test]
    fn route_event_double_dispatch_guard() {
        let states = vec![st_pane("a", "done", "%1")];
        let mut queue = vec!["p1".into(), "p2".into()];
        let mut pending = std::collections::HashSet::new();
        let _ = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "done", 1),
            &mut queue,
            &mut pending,
        );
        // second done while pending → no redispatch
        let a2 = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "done", 2),
            &mut queue,
            &mut pending,
        );
        assert!(a2.is_none());
        assert_eq!(queue, vec!["p2".to_string()]); // only one pop
                                                   // working clears pending
        let a3 = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "working", 3),
            &mut queue,
            &mut pending,
        );
        assert!(a3.is_none());
        assert!(!pending.contains("a"));
        // now a fresh done can redispatch
        let a4 = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "idle", 4),
            &mut queue,
            &mut pending,
        );
        assert!(matches!(
            a4,
            Some(LoopAction::DispatchImmediate { text, .. }) if text == "p2"
        ));
    }

    #[test]
    fn route_event_blocked_and_exited() {
        let states = vec![st_pane("a", "working", "%1")];
        let mut queue = vec!["p1".into()];
        let mut pending = std::collections::HashSet::new();
        let b = route_event(
            &states,
            &pev("pane.agent_status_changed", "%1", "blocked", 1),
            &mut queue,
            &mut pending,
        );
        assert!(matches!(b, Some(LoopAction::Blocked { name }) if name == "a"));
        assert_eq!(queue.len(), 1); // blocked does not pop
        let x = route_event(
            &states,
            &pev("pane.exited", "%1", "exited", 2),
            &mut queue,
            &mut pending,
        );
        assert!(matches!(
            x,
            Some(LoopAction::Diagnose { name, reason })
                if name == "a" && reason == "pane_exited"
        ));
    }

    // ---- bd-event-self-heal-unknown: self_heal_step state machine ----

    /// cg6.2: only a SUCCESSFUL diagnose advances the heal episode — a failed
    /// leaf must not inflate diagnose_count or pre-set awaiting_clear.
    #[test]
    fn apply_diagnose_outcome_only_advances_on_success() {
        let mut hs = HealState::default();
        // leaf failed: state untouched
        apply_diagnose_outcome(&mut hs, false);
        assert_eq!(hs.diagnose_count, 0);
        assert!(!hs.awaiting_clear);
        // success: episode advances
        apply_diagnose_outcome(&mut hs, true);
        assert_eq!(hs.diagnose_count, 1);
        assert!(hs.awaiting_clear);
        // second success with awaiting_clear already set: count unchanged
        apply_diagnose_outcome(&mut hs, true);
        assert_eq!(hs.diagnose_count, 1);
    }

    #[test]
    fn self_heal_unknown_diagnose_then_respawn() {
        let mut hs = HealState::default();
        // threshold 3: first two unknowns → None
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 3, 2),
            HealAction::None
        );
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 3, 2),
            HealAction::None
        );
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 3, 2),
            HealAction::Diagnose
        );
        assert!(hs.awaiting_clear);
        // still unknown after diagnose → respawn
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 3, 2),
            HealAction::Respawn
        );
        assert_eq!(hs.respawn_count, 1);
    }

    #[test]
    fn self_heal_exited_diagnose_immediate() {
        let mut hs = HealState::default();
        assert_eq!(
            self_heal_step(&mut hs, "working", true, 3, 2),
            HealAction::Diagnose
        );
        assert_eq!(
            self_heal_step(&mut hs, "unknown", true, 3, 2),
            HealAction::Respawn
        );
    }

    #[test]
    fn self_heal_budget_exhausted_escalates() {
        let mut hs = HealState {
            respawn_count: 2,
            awaiting_clear: true,
            ..Default::default()
        };
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 3, 2),
            HealAction::EscalateBlocked
        );
    }

    #[test]
    fn self_heal_classified_resets_episode() {
        let mut hs = HealState {
            unknown_count: 2,
            awaiting_clear: true,
            diagnose_count: 1,
            respawn_count: 1,
        };
        assert_eq!(
            self_heal_step(&mut hs, "idle", false, 3, 2),
            HealAction::None
        );
        assert_eq!(hs.unknown_count, 0);
        assert!(!hs.awaiting_clear);
        assert_eq!(hs.respawn_count, 1); // budget preserved
    }

    #[test]
    fn self_heal_flicker_never_diagnoses() {
        let mut hs = HealState::default();
        self_heal_step(&mut hs, "unknown", false, 3, 2);
        self_heal_step(&mut hs, "idle", false, 3, 2); // flicker reset
        self_heal_step(&mut hs, "unknown", false, 3, 2);
        assert_eq!(hs.unknown_count, 1);
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 3, 2),
            HealAction::None
        );
    }

    #[test]
    fn route_event_substitutes_agent_name() {
        let states = vec![st_pane("flywheel-oc", "idle", "%9")];
        let mut queue = vec!["hi {AGENT_NAME}".into()];
        let mut pending = std::collections::HashSet::new();
        let a = route_event(
            &states,
            &pev("pane.agent_status_changed", "%9", "idle", 1),
            &mut queue,
            &mut pending,
        );
        match a {
            Some(LoopAction::DispatchImmediate { text, .. }) => {
                assert_eq!(text, "hi flywheel-oc");
            }
            other => panic!("expected DispatchImmediate, got {other:?}"),
        }
    }

    #[test]
    fn parse_poll_entry_reads_pane_id() {
        let v = serde_json::json!(["a", "idle", "opencode", "%p1"]);
        let s = parse_poll_entry(&v).unwrap();
        assert_eq!(s.pane_id, "%p1");
        assert_eq!(s.kind, "opencode");
        // legacy 3-element still works
        let v2 = serde_json::json!(["b", "working", "pi"]);
        let s2 = parse_poll_entry(&v2).unwrap();
        assert!(s2.pane_id.is_empty());
    }

    // ---- bd-test-event-poll-parity: decision-level differential ----
    // Live herdr session: tests/event_poll_parity.rs (#[ignore]).
    // Pure tests prove plan_actions vs route_event agree on (agent, action, content).

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct DispatchRec {
        agent: String,
        kind: &'static str,
        content: String,
    }

    fn rec_from_action(a: &LoopAction) -> Option<DispatchRec> {
        match a {
            LoopAction::Send { name, text } | LoopAction::DispatchImmediate { name, text } => {
                Some(DispatchRec {
                    agent: name.clone(),
                    kind: "send",
                    content: text.clone(),
                })
            }
            LoopAction::Blocked { name } => Some(DispatchRec {
                agent: name.clone(),
                kind: "blocked",
                content: String::new(),
            }),
            LoopAction::Diagnose { name, reason } => Some(DispatchRec {
                agent: name.clone(),
                kind: "diagnose",
                content: reason.clone(),
            }),
            LoopAction::Respawn { name, reason } => Some(DispatchRec {
                agent: name.clone(),
                kind: "respawn",
                content: reason.clone(),
            }),
            LoopAction::Nudge { name } => Some(DispatchRec {
                agent: name.clone(),
                kind: "nudge",
                content: String::new(),
            }),
            LoopAction::Wait => None,
        }
    }

    fn poll_dispatch(states: &[AgentState], queue: &mut Vec<String>) -> Vec<DispatchRec> {
        plan_actions(states, queue, None)
            .iter()
            .filter_map(rec_from_action)
            .collect()
    }

    fn event_dispatch(
        states: &[AgentState],
        events: &[PollEvent],
        queue: &mut Vec<String>,
        pending: &mut std::collections::HashSet<String>,
    ) -> Vec<DispatchRec> {
        let mut out = Vec::new();
        for ev in events {
            if let Some(a) = route_event(states, ev, queue, pending)
                && let Some(r) = rec_from_action(&a)
            {
                out.push(r);
            }
        }
        out
    }

    fn parity_report(poll: &[DispatchRec], event: &[DispatchRec]) -> String {
        format!("poll={poll:?}\nevent={event:?}")
    }

    #[test]
    fn event_poll_parity_done_dispatch_same_content() {
        let states = vec![st_pane("a", "done", "%1")];
        let mut q_poll = vec!["p1".into(), "p2".into()];
        let mut q_ev = q_poll.clone();
        let poll = poll_dispatch(&states, &mut q_poll);
        let mut pending = std::collections::HashSet::new();
        let event = event_dispatch(
            &states,
            &[pev("pane.agent_status_changed", "%1", "done", 1)],
            &mut q_ev,
            &mut pending,
        );
        assert_eq!(poll, event, "{}", parity_report(&poll, &event));
        assert_eq!(poll[0].content, "p1");
        assert_eq!(q_poll, q_ev);
    }

    #[test]
    fn event_poll_parity_double_dispatch_guard() {
        let states = vec![st_pane("a", "done", "%1")];
        let mut q = vec!["p1".into(), "p2".into()];
        let mut pending = std::collections::HashSet::new();
        let e1 = event_dispatch(
            &states,
            &[
                pev("pane.agent_status_changed", "%1", "done", 1),
                pev("pane.agent_status_changed", "%1", "done", 2),
            ],
            &mut q,
            &mut pending,
        );
        assert_eq!(e1.len(), 1, "double-dispatch guard: {e1:?}");
        assert_eq!(e1[0].content, "p1");
        assert_eq!(q, vec!["p2".to_string()]);
    }

    #[test]
    fn event_poll_parity_blocked_identical() {
        let states = vec![st_pane("a", "blocked", "%1")];
        let mut q_poll = vec!["p1".into()];
        let mut q_ev = q_poll.clone();
        let poll = poll_dispatch(&states, &mut q_poll);
        let mut pending = std::collections::HashSet::new();
        let event = event_dispatch(
            &states,
            &[pev("pane.agent_status_changed", "%1", "blocked", 1)],
            &mut q_ev,
            &mut pending,
        );
        assert_eq!(poll, event, "{}", parity_report(&poll, &event));
        assert_eq!(poll[0].kind, "blocked");
        assert_eq!(q_poll.len(), 1);
        assert_eq!(q_ev.len(), 1);
    }

    #[test]
    fn event_poll_parity_unknown_no_dispatch() {
        let states = vec![st_pane("a", "unknown", "%1")];
        let mut q_poll = vec!["p1".into()];
        let mut q_ev = q_poll.clone();
        let poll = poll_dispatch(&states, &mut q_poll);
        let mut pending = std::collections::HashSet::new();
        let event = event_dispatch(
            &states,
            &[pev("pane.agent_status_changed", "%1", "unknown", 1)],
            &mut q_ev,
            &mut pending,
        );
        assert!(poll.is_empty(), "poll must not send to unknown: {poll:?}");
        assert!(
            event.is_empty(),
            "event must not send to unknown: {event:?}"
        );
        assert_eq!(q_poll, vec!["p1".to_string()]);
        assert_eq!(q_ev, vec!["p1".to_string()]);
    }

    #[test]
    fn event_poll_parity_exited_diagnose_not_send() {
        let states = vec![st_pane("a", "working", "%1")];
        let mut q = vec!["p1".into()];
        let mut pending = std::collections::HashSet::new();
        let event = event_dispatch(
            &states,
            &[pev("pane.exited", "%1", "exited", 1)],
            &mut q,
            &mut pending,
        );
        assert_eq!(event.len(), 1);
        assert_eq!(event[0].kind, "diagnose");
        assert_eq!(event[0].content, "pane_exited");
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn event_poll_parity_agent_name_subst_identical() {
        let states = vec![st_pane("fly-oc", "idle", "%2")];
        let mut q_poll = vec!["go {AGENT_NAME}".into()];
        let mut q_ev = q_poll.clone();
        let poll = poll_dispatch(&states, &mut q_poll);
        let mut pending = std::collections::HashSet::new();
        let event = event_dispatch(
            &states,
            &[pev("pane.agent_status_changed", "%2", "idle", 1)],
            &mut q_ev,
            &mut pending,
        );
        assert_eq!(poll, event, "{}", parity_report(&poll, &event));
        assert_eq!(poll[0].content, "go fly-oc");
    }

    #[test]
    fn event_poll_parity_self_heal_diagnose_tick_agreement() {
        let mut hs = HealState::default();
        for _ in 0..2 {
            assert_eq!(
                self_heal_step(&mut hs, "unknown", false, 3, 2),
                HealAction::None
            );
        }
        assert_eq!(
            self_heal_step(&mut hs, "unknown", false, 3, 2),
            HealAction::Diagnose
        );
        let states = vec![st_pane("a", "unknown", "%1")];
        let mut q = vec!["p1".into()];
        let poll = poll_dispatch(&states, &mut q);
        assert!(poll.is_empty());
        assert_eq!(q, vec!["p1".to_string()]);
    }
}

#[cfg(test)]
mod resume_dedup_tests {
    use super::*;
    use crate::state::SentRecord;

    fn rec(name: &str, hash: u64, invocation: &str, landed: bool, ambiguous: bool) -> SentRecord {
        SentRecord {
            name: name.to_string(),
            text_hash: hash,
            invocation: invocation.to_string(),
            iteration: 1,
            landed,
            ambiguous,
        }
    }

    #[test]
    fn should_resend_matrix() {
        // no record -> never sent -> resend
        assert!(should_resend(None, "inv-1"));
        // landed, same invocation -> delivered; skip
        assert!(!should_resend(Some(&rec("a", 1, "inv-1", true, false)), "inv-1"));
        // unlanded, same invocation -> crashed mid-flight; resend
        assert!(should_resend(Some(&rec("a", 1, "inv-1", false, false)), "inv-1"));
        // cg6.5: ambiguous (timeout-confirm), same invocation -> may be
        // mid-flight; NEVER re-delegate
        assert!(!should_resend(Some(&rec("a", 1, "inv-1", false, true)), "inv-1"));
        // any record from ANOTHER invocation -> fresh run re-delegates
        assert!(should_resend(Some(&rec("a", 1, "inv-0", true, false)), "inv-1"));
        assert!(should_resend(Some(&rec("a", 1, "inv-0", false, false)), "inv-1"));
        assert!(should_resend(Some(&rec("a", 1, "inv-0", false, true)), "inv-1"));
    }

    #[test]
    fn should_resend_degrades_without_confirmation() {
        // pre-confirm loops recorded nothing (or unlanded): resend = today
        // (the fn docstring promises the guard degrades to re-sending)
        assert!(should_resend(None, "inv-1"));
        assert!(should_resend(Some(&rec("a", 1, "inv-1", false, false)), "inv-1"));
    }

    // ---- escalation tiers (bd-blocked-escalation-protocol) ----

    #[test]
    fn escalation_starts_at_t0() {
        // first detection: elapsed < 2 intervals -> T0
        assert_eq!(
            escalation_tier(1_000_000, 1_000_000, 30),
            EscalationTier::T0
        );
        // just under 2 intervals
        assert_eq!(escalation_tier(0, 59_999_999, 30), EscalationTier::T0);
    }

    #[test]
    fn escalation_advances_by_interval_multiple() {
        let i = 30; // seconds
        let us = |secs: i64| secs * 1_000_000;
        // T1 at 2x, T2 at 4x, T3 at 8x interval
        assert_eq!(escalation_tier(0, us(60), i), EscalationTier::T1);
        assert_eq!(escalation_tier(0, us(119), i), EscalationTier::T1);
        assert_eq!(escalation_tier(0, us(120), i), EscalationTier::T2);
        assert_eq!(escalation_tier(0, us(239), i), EscalationTier::T2);
        assert_eq!(escalation_tier(0, us(240), i), EscalationTier::T3);
    }

    #[test]
    fn escalation_is_deterministic_and_bounded() {
        // same inputs -> same tier (crash-resume continues at the right tier)
        let a = escalation_tier(0, 120_000_000, 30);
        let b = escalation_tier(0, 120_000_000, 30);
        assert_eq!(a, b);
        // never exceeds T3 (bounded, not infinite spin)
        assert_eq!(escalation_tier(0, i64::MAX, 30), EscalationTier::T3);
    }

    /// review-fix F4: the bounded-halt decision table. `mail` mode halts at
    /// T3 only; `stop` halts at any tier (T0); `continue` never halts.
    #[test]
    fn blocked_t3_halts_loop_mail_mode_only() {
        use EscalationTier::*;
        // mail mode: only T3 halts
        assert_eq!(on_blocked_tier_exit(T0, "mail"), None);
        assert_eq!(on_blocked_tier_exit(T1, "mail"), None);
        assert_eq!(on_blocked_tier_exit(T2, "mail"), None);
        let halt = on_blocked_tier_exit(T3, "mail");
        assert!(halt.is_some());
        assert!(halt.unwrap().contains("T3"));
        // stop mode: halts at ANY tier (existing --on-blocked stop semantics)
        for t in [T0, T1, T2, T3] {
            assert!(
                on_blocked_tier_exit(t, "stop").is_some(),
                "stop must halt at {t:?}"
            );
        }
        // continue mode: never halts
        for t in [T0, T1, T2, T3] {
            assert_eq!(on_blocked_tier_exit(t, "continue"), None);
        }
        // unknown mode string: never halts (defensive)
        assert_eq!(on_blocked_tier_exit(T3, "whatever"), None);
    }

    // ---- cg6.1: escalation state machine lives in the notify_blocked leaf ----

    /// Serializes the cg6.1 persistence tests AND makes them hermetic
    /// (193.2): the REAL state dir is shared with production loops and
    /// sibling test processes — a v1 fixture left in it re-triggers the
    /// sidecar migration mid-suite (the observed ENOENT flake). The guard
    /// holds the crate-wide env lock + points XDG_STATE_HOME at a fresh
    /// temp dir for the test's lifetime.
    fn cg61_registry_guard() -> crate::config::TestStateHome {
        crate::config::test_state_home()
    }

    /// Unique project slug for leaf-state tests (the state dir is the real
    /// one; tests clean their own entry up).
    fn cg61_test_project(tag: &str) -> String {
        format!("cg6.1-{tag}-{}", std::process::id())
    }

    /// Remove a test loop's registry sidecar (best-effort cleanup).
    fn cg61_cleanup(project: &str) {
        let _ = crate::state::remove_active_loop(project);
    }

    /// Pure: the step derives the tier from the persisted first-blocked
    /// timestamp and mails ONCE per tier transition (cg6.3) — ping_count
    /// increments only when a mail fires.
    #[test]
    fn escalation_step_bounds_tiers_and_increments_ping_count() {
        use crate::state::EscalationState;
        let i = 30u64;
        let us = |secs: i64| secs * 1_000_000;
        let mut st = EscalationState {
            first_blocked_ts: 0,
            ping_count: 0,
            last_ping_tier: None,
            last_tail: String::new(),
        };
        // fresh: T0 + count 1 + mail
        assert_eq!(escalation_step(&mut st, us(30), i), (0, 1, true));
        // 2x interval: T1 (transition → mail)
        assert_eq!(escalation_step(&mut st, us(60), i), (1, 2, true));
        // 4x interval: T2
        assert_eq!(escalation_step(&mut st, us(120), i), (2, 3, true));
        // 8x interval: T3, bounded
        assert_eq!(escalation_step(&mut st, i64::MAX, i), (3, 4, true));
        // same tier again: NO mail, NO increment (cg6.3 — replay / same-tier tick)
        assert_eq!(escalation_step(&mut st, i64::MAX, i), (3, 4, false));
        // overflow-safe: saturating add, never wraps
        let mut st2 = EscalationState {
            first_blocked_ts: 0,
            ping_count: u32::MAX,
            last_ping_tier: None,
            last_tail: String::new(),
        };
        assert_eq!(escalation_step(&mut st2, us(60), i), (1, u32::MAX, true));
    }

    /// Leaf-level (cg6.1): a fresh agent with no persisted entry starts at
    /// T0 with ping_count 1 (first_blocked_ts seeded by the leaf; mails).
    #[test]
    fn escalate_and_persist_fresh_agent_starts_at_t0() {
        let _g = cg61_registry_guard();
        let project = cg61_test_project("fresh");
        let (tier, count, mails) =
            escalate_and_persist(&project, "agent-a", None, 30, json!({})).unwrap();
        assert_eq!((tier, count, mails), (0, 1, true), "first escalation is T0 + ping 1 + mail");
        // the entry was persisted with the seeded timestamp
        let al = crate::state::load_active_loop(&project).unwrap().unwrap();
        let st = al.escalation.get("agent-a").expect("entry persisted");
        assert_eq!(st.ping_count, 1);
        cg61_cleanup(&project);
    }

    /// Leaf-level (cg6.1): a stale persisted first_blocked_ts (past the 8x
    /// window) escalates straight to T3 (and mails — a tier transition).
    #[test]
    fn escalate_and_persist_stale_first_blocked_reaches_t3() {
        use crate::state::{ActiveLoop, EscalationState};
        let _g = cg61_registry_guard();
        let project = cg61_test_project("stale");
        let stale = now_us_micros() - 9 * 30 * 1_000_000; // 9 intervals ago
        let mut al = ActiveLoop::new("swarm-loop", json!({}), crate::state::now_iso());
        al.escalation.insert(
            "agent-b".to_string(),
            EscalationState {
                first_blocked_ts: stale,
                ping_count: 0,
                last_ping_tier: None,
                last_tail: String::new(),
            },
        );
        crate::state::save_active_loop(&project, &al).unwrap();
        let (tier, count, mails) =
            escalate_and_persist(&project, "agent-b", None, 30, json!({})).unwrap();
        assert_eq!((tier, count, mails), (3, 1, true), "stale first_blocked_ts → T3 + mail");
        cg61_cleanup(&project);
    }

    /// cg6.1 + cg6.3 regression proof: re-execution (crash-window replay)
    /// derives the tier from the PERSISTED first_blocked_ts — the tier cannot
    /// jump across the crash boundary, and at the same tier the replay
    /// neither re-mails nor double-counts (at most one ping per logical tier
    /// transition).
    #[test]
    fn escalate_and_persist_replay_derives_from_persisted_first_blocked() {
        let _g = cg61_registry_guard();
        let project = cg61_test_project("replay");
        // seed ~1s ago: both executions land inside the T0 window
        let seed = now_us_micros() - 1_000_000;
        let (t1, c1, m1) =
            escalate_and_persist(&project, "agent-c", Some(seed), 30, json!({})).unwrap();
        // "crash" — re-execution of the same leaf invocation, later wall-clock
        let (t2, c2, m2) =
            escalate_and_persist(&project, "agent-c", Some(seed), 30, json!({})).unwrap();
        assert_eq!(t1, t2, "tier must not jump on replay");
        assert_eq!((c1, m1), (1, true));
        assert_eq!(
            (c2, m2),
            (1, false),
            "same-tier replay: no re-mail, no double-count (cg6.3)"
        );
        // the persisted first_blocked_ts is the ORIGINAL seed, not a re-stamp
        let al = crate::state::load_active_loop(&project).unwrap().unwrap();
        let st = al.escalation.get("agent-c").unwrap();
        assert_eq!(st.first_blocked_ts, seed, "first_blocked_ts never re-seeded");
        assert_eq!(st.ping_count, 1);
        assert_eq!(st.last_ping_tier, Some(0), "T0 ping marked");
        cg61_cleanup(&project);
    }
}
